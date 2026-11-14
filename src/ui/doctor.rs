//! The checkup window, the doctor (docs/UI.md section 6): scan on open, findings grouped in
//! sections with the memory each covers, checkboxes pre-checked from `recommended`, action
//! pills, a running total, CLEAN UP on a worker thread with every row turning into its result
//! in place, "End it anyway" with an in-window confirmation, "Undo last cleanup", "Check
//! again" and the ALL CLEAR state.
//!
//! The real `doctor` module (docs/DESIGN.md Part F) is built in parallel, so this file has its
//! own view model, a plain data mirror of Part F's `Report` (fields added for the window are
//! marked "Addition"). The integrator maps `doctor::Report` onto it and implements
//! `DoctorSource` over `doctor::scan` / `apply` / `undo`. `FakeDoctor` never touches a process.

use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

use super::canvas::{Align, Canvas};
use super::commands::Command;
use super::geom::{Pt, Rect};
use super::kit::{format_gb, Button, ButtonStyle, Dialog, Pill, Scroll, WState};
use super::setup::{indeterminate_bar, paint_link, wake, wrap_hard};
use super::theme::*;
use super::window::{self, Cursor, Event, Host, MenuItem, MouseButton, Surface, WindowSpec};

pub const KIND: &str = "doctor";
pub const TITLE: &str = "Ather MemGuard — Checkup";
const TIMER_WAKE: usize = 7101;
const TIMER_ANIM: usize = 7102;
const MENU_CHOICE: u32 = 1;
const F_RESULT: Font = Font::new(Face::Display, 34.0, REGULAR);

// ---------------------------------------------------------------- view model (mirror of Part F)

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemStatus {
    pub ram_total_gb: f64,
    pub ram_free_gb: f64,
    pub commit_limit_gb: f64,
    pub commit_free_gb: f64,
    pub memguard_running: bool,
    pub memguard_paused: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub time: String,
    pub system: SystemStatus,
    pub findings: Vec<Finding>,
    pub reclaimable_ram_gb: f64,
    pub reclaimable_commit_gb: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    App,
    Startup,
    Unreal,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Close,
    HoldBack,
    Keep,
    Review,
    HeavyTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Close,
    End,
    DisableStartup,
    HoldBack,
    BlockWhileUnreal,
    /// Addition: "Leave it" on a WORTH A LOOK row adds the app to `[doctor] keep`.
    Keep,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub kind: ActionKind,
    /// Plain words for the CLI ("Adds it to Browsers"); the window labels pills itself.
    pub detail: String,
}

impl Action {
    pub fn new(kind: ActionKind) -> Action {
        Action { kind, detail: String::new() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Stable: "app:steam", "startup:hkcu:Steam", "ue:orphans", "sys:pagefile".
    pub id: String,
    pub kind: FindingKind,
    pub verdict: Verdict,
    pub title: String,
    pub detail: String,
    pub processes: usize,
    pub ram_mb: u64,
    pub committed_mb: u64,
    /// Proposed, most recommended first.
    pub actions: Vec<Action>,
    pub recommended: bool,
    /// Addition: a startup entry in HKLM or the common Startup folder ("Needs an administrator").
    pub needs_admin: bool,
    /// Addition: company from the exe's version info (WORTH A LOOK rows), may be empty.
    pub company: String,
    /// Addition: where "How to fix" goes for THIS PC rows.
    pub help_url: Option<String>,
    /// Addition: a THIS PC row whose link runs a command instead ("Show me how" on the RAM
    /// check rows writes and opens the RAM report). Wins over `help_url`.
    pub help_command: Option<Command>,
}

/// The link a THIS PC row shows, if any.
pub fn help_link(f: &Finding) -> Option<&'static str> {
    if f.help_command.is_some() {
        Some("Show me how")
    } else if f.help_url.is_some() {
        Some("How to fix")
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
    Done,
    /// Asked to close and it's still running ("End it anyway" is offered).
    StillOpen,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub finding_id: String,
    pub action: ActionKind,
    pub status: OutcomeStatus,
    /// What happened, in plain words ("Closed", "Added to Browsers"). Empty = a default.
    pub message: String,
}

/// Scans and acts for the window. `Send` so it can run on a worker thread.
pub trait DoctorSource: Send + 'static {
    fn scan(&self) -> Report;
    fn apply(&self, picks: &[(String, ActionKind)], force: bool) -> Vec<Outcome>;
    fn undo_last(&self) -> Vec<Outcome>;
    fn can_undo(&self) -> bool;
    /// Addition: what could still be freed after the last `apply` (from a fresh scan), so the
    /// hero number goes down while the rows keep showing their results. `None` = unchanged.
    fn reclaimable_after_apply(&self) -> Option<f64> {
        None
    }
}

// ---------------------------------------------------------------- selection model (pure)

/// One entry of a row's action pill.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    pub picks: Vec<ActionKind>,
}

/// The pill entries for a finding, in the order of its actions (the first is the default).
pub fn choices(f: &Finding) -> Vec<Choice> {
    // "Close and don't start with Windows" when Close is proposed first; listed first, switching
    // the startup entry off is offered on its own (it is the recommended action of Part F).
    let close_first = |i: usize| f.actions[..i].iter().any(|a| a.kind == ActionKind::Close);
    let mut out = Vec::new();
    for (ai, a) in f.actions.iter().enumerate() {
        let (label, picks): (&str, Vec<ActionKind>) = match (f.kind, f.verdict, a.kind) {
            (FindingKind::Startup, _, ActionKind::DisableStartup) => ("Don't start with Windows", vec![ActionKind::DisableStartup]),
            (FindingKind::Unreal, _, ActionKind::End) => ("End them", vec![ActionKind::End]),
            (_, Verdict::HeavyTool, ActionKind::Close) => ("Close if you're not using it", vec![ActionKind::Close]),
            (_, _, ActionKind::Close) => ("Close", vec![ActionKind::Close]),
            (_, _, ActionKind::DisableStartup) if close_first(ai) => {
                ("Close and don't start with Windows", vec![ActionKind::Close, ActionKind::DisableStartup])
            }
            (_, _, ActionKind::DisableStartup) => ("Don't start with Windows", vec![ActionKind::DisableStartup]),
            (_, _, ActionKind::BlockWhileUnreal) => ("Close whenever Unreal is running", vec![ActionKind::Close, ActionKind::BlockWhileUnreal]),
            (_, _, ActionKind::HoldBack) => ("Hold back", vec![ActionKind::HoldBack]),
            (_, _, ActionKind::Keep) => ("Leave it", vec![ActionKind::Keep]),
            (_, _, ActionKind::End) => ("End it", vec![ActionKind::End]),
        };
        if !out.iter().any(|c: &Choice| c.label == label) {
            out.push(Choice { label: label.to_string(), picks });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    NotNeeded,
    Startup,
    BigTools,
    WorthALook,
    HeldBack,
    Unreal,
    ThisPc,
}

impl SectionKind {
    pub const ORDER: [SectionKind; 7] =
        [SectionKind::NotNeeded, SectionKind::Startup, SectionKind::BigTools, SectionKind::WorthALook, SectionKind::HeldBack, SectionKind::Unreal, SectionKind::ThisPc];
    pub fn title(self) -> &'static str {
        match self {
            SectionKind::NotNeeded => "NOT NEEDED FOR UNREAL",
            SectionKind::Startup => "STARTS WITH WINDOWS",
            SectionKind::BigTools => "BIG TOOLS YOU MIGHT NOT BE USING",
            SectionKind::WorthALook => "WORTH A LOOK",
            SectionKind::HeldBack => "HELD BACK, NOT CLOSED",
            SectionKind::Unreal => "UNREAL",
            SectionKind::ThisPc => "THIS PC",
        }
    }
}

/// Which section a finding goes in; `None` = not shown (`keep` apps).
pub fn section_of(f: &Finding) -> Option<SectionKind> {
    match (f.kind, f.verdict) {
        (FindingKind::Startup, _) => Some(SectionKind::Startup),
        (FindingKind::Unreal, _) => Some(SectionKind::Unreal),
        (FindingKind::System, _) => Some(SectionKind::ThisPc),
        (FindingKind::App, Verdict::Close) => Some(SectionKind::NotNeeded),
        (FindingKind::App, Verdict::HeavyTool) => Some(SectionKind::BigTools),
        (FindingKind::App, Verdict::Review) => Some(SectionKind::WorthALook),
        (FindingKind::App, Verdict::HoldBack) => Some(SectionKind::HeldBack),
        (FindingKind::App, Verdict::Keep) => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowState {
    pub checked: bool,
    pub choice: usize,
    /// Its picks are running now.
    pub busy: bool,
    /// What happened to it; when not empty the row shows these instead of its controls.
    pub outcomes: Vec<Outcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub kind: SectionKind,
    /// Indices into `report.findings`.
    pub rows: Vec<usize>,
    pub ram_mb: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DoctorModel {
    pub report: Report,
    pub rows: Vec<RowState>,
}

impl DoctorModel {
    pub fn new(report: Report) -> DoctorModel {
        let rows = report
            .findings
            .iter()
            .map(|f| RowState { checked: Self::selectable_finding(f) && Self::default_checked(f), choice: 0, busy: false, outcomes: Vec::new() })
            .collect();
        DoctorModel { report, rows }
    }

    fn selectable_finding(f: &Finding) -> bool {
        !f.needs_admin && f.kind != FindingKind::System && !choices(f).is_empty()
    }

    /// `recommended` rows start checked; so do HELD BACK rows (docs/UI.md). Big tools and
    /// WORTH A LOOK rows never do.
    fn default_checked(f: &Finding) -> bool {
        match section_of(f) {
            Some(SectionKind::BigTools | SectionKind::WorthALook) | None => false,
            Some(SectionKind::HeldBack) => true,
            _ => f.recommended,
        }
    }

    /// Can row `i` be checked now?
    pub fn selectable(&self, i: usize) -> bool {
        let r = &self.rows[i];
        Self::selectable_finding(&self.report.findings[i]) && !r.busy && r.outcomes.is_empty()
    }

    pub fn toggle(&mut self, i: usize) {
        if self.selectable(i) {
            self.rows[i].checked = !self.rows[i].checked;
        }
    }

    pub fn set_choice(&mut self, i: usize, c: usize) {
        if c < choices(&self.report.findings[i]).len() && self.selectable(i) {
            self.rows[i].choice = c;
        }
    }

    fn chosen(&self, i: usize) -> Option<Choice> {
        choices(&self.report.findings[i]).get(self.rows[i].choice).cloned()
    }

    /// What CLEAN UP would run: (finding id, action) for every checked row.
    pub fn picks(&self) -> Vec<(String, ActionKind)> {
        let mut out = Vec::new();
        for i in 0..self.rows.len() {
            if self.rows[i].checked && self.selectable(i) {
                if let Some(c) = self.chosen(i) {
                    for k in c.picks {
                        out.push((self.report.findings[i].id.clone(), k));
                    }
                }
            }
        }
        out
    }

    /// "Selected: frees about {x} GB": the RAM of checked rows whose action closes or ends
    /// something now. Switching off a startup entry or holding an app back frees nothing now.
    pub fn selected_gb(&self) -> f64 {
        (0..self.rows.len())
            .filter(|&i| self.rows[i].checked && self.selectable(i))
            .filter(|&i| self.chosen(i).map(|c| c.picks.iter().any(|k| matches!(k, ActionKind::Close | ActionKind::End))).unwrap_or(false))
            .map(|i| self.report.findings[i].ram_mb as f64 / 1024.0)
            .fold(0.0, |a, b| a + b) // not sum(): an empty f64 sum is -0.0
    }

    pub fn any_checked(&self) -> bool {
        !self.picks().is_empty()
    }

    /// Checked rows whose pick ends processes ("End it", "End them"). CLEAN UP asks before
    /// running these: anything unsaved in them is lost.
    pub fn ending_rows(&self) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|&i| self.rows[i].checked && self.selectable(i))
            .filter(|&i| self.chosen(i).is_some_and(|c| c.picks.contains(&ActionKind::End)))
            .collect()
    }

    /// Mark the checked rows as running and return their picks.
    pub fn begin_apply(&mut self) -> Vec<(String, ActionKind)> {
        let picks = self.picks();
        for i in 0..self.rows.len() {
            if self.rows[i].checked && self.selectable(i) {
                self.rows[i].busy = true;
                self.rows[i].checked = false;
            }
        }
        picks
    }

    /// Mark one row as running (End it anyway).
    pub fn begin_one(&mut self, i: usize) {
        self.rows[i].busy = true;
    }

    /// Results replace the rows they belong to. A new result for a row replaces its old ones.
    pub fn apply_outcomes(&mut self, outs: Vec<Outcome>) {
        let mut seen: Vec<usize> = Vec::new();
        for o in outs {
            if let Some(i) = self.report.findings.iter().position(|f| f.id == o.finding_id) {
                if !seen.contains(&i) {
                    self.rows[i].outcomes.clear();
                    seen.push(i);
                }
                self.rows[i].outcomes.push(o);
            }
        }
        for r in &mut self.rows {
            r.busy = false;
        }
    }

    /// True when the app in row `i` stayed open after a close.
    pub fn still_open(&self, i: usize) -> bool {
        self.rows[i].outcomes.iter().any(|o| o.status == OutcomeStatus::StillOpen)
    }

    pub fn sections(&self) -> Vec<Section> {
        let mut out = Vec::new();
        for kind in SectionKind::ORDER {
            let rows: Vec<usize> = (0..self.report.findings.len()).filter(|&i| section_of(&self.report.findings[i]) == Some(kind)).collect();
            if rows.is_empty() {
                continue;
            }
            let ram_mb = if kind == SectionKind::ThisPc { 0 } else { rows.iter().map(|&i| self.report.findings[i].ram_mb).sum() };
            out.push(Section { kind, rows, ram_mb });
        }
        out
    }

    /// Nothing proposed: no finding with an action anywhere.
    pub fn all_clear(&self) -> bool {
        self.report.findings.iter().all(|f| section_of(f).is_none() || choices(f).is_empty())
    }
}

/// The words a row shows for its results.
pub fn outcome_text(outs: &[Outcome]) -> (String, Rgb) {
    if let Some(o) = outs.iter().find(|o| o.status == OutcomeStatus::StillOpen) {
        let m = if o.message.is_empty() { "Still open — it may be asking you to save".to_string() } else { o.message.clone() };
        return (m, WARN);
    }
    let failed = outs.iter().any(|o| o.status == OutcomeStatus::Failed);
    let mut parts: Vec<String> = Vec::new();
    for o in outs {
        let m = if !o.message.is_empty() {
            o.message.clone()
        } else {
            match (o.status, o.action) {
                (OutcomeStatus::Failed, _) => "Didn't work".into(),
                (OutcomeStatus::Skipped, _) => "Skipped".into(),
                (_, ActionKind::Close) => "Closed".into(),
                (_, ActionKind::End) => "Ended".into(),
                (_, ActionKind::DisableStartup) => "Switched off".into(),
                (_, ActionKind::HoldBack) => "Held back".into(),
                (_, ActionKind::BlockWhileUnreal) => "Stays closed while Unreal is running".into(),
                (_, ActionKind::Keep) => "Left as it is".into(),
            }
        };
        if !parts.contains(&m) {
            parts.push(m);
        }
    }
    (parts.join(" · "), if failed { ERROR } else { TEXT_DIM })
}

// ---------------------------------------------------------------- fake

/// Plausible findings for every section; `apply` only reports, it never touches a process.
pub struct FakeDoctor {
    pub clear: bool,
    pub delay: Duration,
    applied: Mutex<bool>,
    /// What is left to free after the last `apply`.
    after_apply: Mutex<Option<f64>>,
}

impl FakeDoctor {
    pub fn new() -> FakeDoctor {
        FakeDoctor { clear: false, delay: Duration::ZERO, applied: Mutex::new(false), after_apply: Mutex::new(None) }
    }
    pub fn all_clear() -> FakeDoctor {
        FakeDoctor { clear: true, ..FakeDoctor::new() }
    }
    pub fn with_delay(mut self, d: Duration) -> FakeDoctor {
        self.delay = d;
        self
    }

    pub fn report(clear: bool) -> Report {
        let f = |id: &str, kind: FindingKind, verdict: Verdict, title: &str, detail: &str, processes: usize, ram_mb: u64, actions: &[ActionKind], recommended: bool| Finding {
            id: id.into(),
            kind,
            verdict,
            title: title.into(),
            detail: detail.into(),
            processes,
            ram_mb,
            committed_mb: ram_mb * 5 / 4,
            actions: actions.iter().map(|k| Action::new(*k)).collect(),
            recommended,
            needs_admin: false,
            company: String::new(),
            help_url: None,
            help_command: None,
        };
        use ActionKind as A;
        use FindingKind as K;
        use Verdict as V;
        let system = SystemStatus { ram_total_gb: 127.7, ram_free_gb: 45.1, commit_limit_gb: 144.0, commit_free_gb: 27.6, memguard_running: true, memguard_paused: false };
        if clear {
            return Report { time: "2026-11-14 14:02:11".into(), system, findings: Vec::new(), reclaimable_ram_gb: 0.0, reclaimable_commit_gb: 0.0 };
        }
        let mut findings = vec![
            f("app:steam", K::App, V::Close, "Steam", "Game launcher. Not used by Unreal.", 6, 1270, &[A::Close, A::DisableStartup, A::BlockWhileUnreal], true),
            f("app:epicgameslauncher", K::App, V::Close, "Epic Games Launcher", "Game launcher. Unreal doesn't need it running to open or build a project.", 4, 980, &[A::DisableStartup, A::Close], true),
            f("app:spotify", K::App, V::Close, "Spotify", "Music player. Not used by Unreal.", 5, 640, &[A::Close, A::BlockWhileUnreal], false),
            f("app:widgets", K::App, V::Close, "Widgets", "News and weather panel. It comes back when you open it.", 3, 410, &[A::Close], true),
            f("app:icue", K::App, V::Close, "iCUE", "Lighting and fan software. Your devices keep their last settings.", 4, 560, &[A::Close, A::DisableStartup, A::End], false),
            f("startup:hkcu:Steam", K::Startup, V::Close, "Steam", "", 0, 1270, &[A::DisableStartup], true),
            f("startup:hkcu:Spotify", K::Startup, V::Close, "Spotify", "", 0, 640, &[A::DisableStartup], true),
            f("startup:hklm:iCUE", K::Startup, V::Close, "iCUE", "", 0, 560, &[], false),
            f("app:blender", K::App, V::HeavyTool, "Blender", "3D modelling tool. Save your work first if you close it.", 1, 2350, &[A::Close], false),
            f("app:nwsync", K::App, V::Review, "NWSyncAgent", "Northwind Sync Agent. Not in the list of known apps.", 2, 2150, &[A::Close, A::HoldBack, A::Keep], false),
            f("app:discord", K::App, V::HoldBack, "Discord", "Chat app. It stays open and is held back while Unreal needs memory.", 6, 890, &[A::HoldBack], false),
            f("app:dropbox", K::App, V::HoldBack, "Dropbox", "Cloud sync. Held back so syncing doesn't compete with Unreal.", 3, 420, &[A::HoldBack], false),
            f("ue:orphans", K::Unreal, V::Close, "3 leftover Unreal helpers from a closed editor", "ShaderCompileWorker processes with no editor left to use them.", 3, 1230, &[A::End], false),
            f("sys:pagefile", K::System, V::Keep, "The page file is small for this much RAM.", "Windows can only promise apps 144 GB. Big cooks can fail even with RAM free.", 0, 0, &[], false),
            f(
                "sys:ram",
                K::System,
                V::Review,
                "Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz.",
                "The kit's fast profile (EXPO) looks switched off in the BIOS, so the memory runs at its slower default. Turning it on takes about five minutes and is safe to try: if it doesn't work, the BIOS goes back by itself.",
                0,
                0,
                &[],
                false,
            ),
        ];
        findings[7].needs_admin = true;
        findings[9].company = "Northwind Systems".into();
        findings[13].help_url = Some("https://github.com/AskTinNguyen/AtherMemGuard/blob/main/docs/PAGEFILE.md".into());
        findings[14].help_command = Some(Command::ShowRamReport);
        let reclaim: u64 = findings.iter().filter(|f| f.kind == K::App || f.kind == K::Unreal).filter(|f| f.verdict != V::HoldBack).map(|f| f.ram_mb).sum();
        Report { time: "2026-11-14 14:02:11".into(), system, findings, reclaimable_ram_gb: reclaim as f64 / 1024.0, reclaimable_commit_gb: reclaim as f64 * 1.25 / 1024.0 }
    }

    fn outcome(id: &str, action: ActionKind, force: bool) -> Outcome {
        let (status, message) = match (id, action) {
            ("app:spotify", ActionKind::Close) if !force => (OutcomeStatus::StillOpen, String::new()),
            ("app:discord", ActionKind::HoldBack) => (OutcomeStatus::Done, "Added to Chat".to_string()),
            ("app:dropbox", ActionKind::HoldBack) => (OutcomeStatus::Done, "Added to Background".to_string()),
            ("ue:orphans", ActionKind::End) => (OutcomeStatus::Done, "Ended 3 helpers".to_string()),
            _ => (OutcomeStatus::Done, String::new()),
        };
        Outcome { finding_id: id.into(), action, status, message }
    }
}

impl Default for FakeDoctor {
    fn default() -> FakeDoctor {
        FakeDoctor::new()
    }
}

impl DoctorSource for FakeDoctor {
    fn scan(&self) -> Report {
        std::thread::sleep(self.delay);
        let applied = *self.applied.lock().unwrap();
        // After a cleanup the closed apps are gone from the next scan.
        let mut r = FakeDoctor::report(self.clear || applied);
        if applied && !self.clear {
            r.findings.clear();
        }
        r
    }
    fn apply(&self, picks: &[(String, ActionKind)], force: bool) -> Vec<Outcome> {
        std::thread::sleep(self.delay);
        *self.applied.lock().unwrap() = true;
        let outs: Vec<Outcome> = picks.iter().map(|(id, k)| FakeDoctor::outcome(id, *k, force)).collect();
        // Like the real one: what closed or ended no longer counts as "could be freed".
        let r = FakeDoctor::report(self.clear);
        let mut freed: Vec<&str> = outs
            .iter()
            .filter(|o| o.status == OutcomeStatus::Done && matches!(o.action, ActionKind::Close | ActionKind::End))
            .map(|o| o.finding_id.as_str())
            .collect();
        freed.sort_unstable();
        freed.dedup();
        let freed_mb: u64 = freed.iter().filter_map(|id| r.findings.iter().find(|f| f.id == *id)).map(|f| f.ram_mb).sum();
        *self.after_apply.lock().unwrap() = Some((r.reclaimable_ram_gb - freed_mb as f64 / 1024.0).max(0.0));
        outs
    }
    fn undo_last(&self) -> Vec<Outcome> {
        std::thread::sleep(self.delay);
        *self.applied.lock().unwrap() = false;
        Vec::new()
    }
    fn can_undo(&self) -> bool {
        *self.applied.lock().unwrap()
    }
    fn reclaimable_after_apply(&self) -> Option<f64> {
        self.after_apply.lock().unwrap().take()
    }
}

// ---------------------------------------------------------------- the window

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Check(usize),
    Choice(usize),
    EndAnyway(usize),
    HowToFix(usize),
    CheckAgain,
    CleanUp,
    Undo,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    Logo,
    Eyebrow(String),
    Headline(String),
    /// "{x} GB" or "ALL CLEAR".
    Result(String),
    Caption(String),
    Link { label: String, color: Rgb },
    Section(String),
    SectionGb(String),
    Hairline(Rgb),
    RowBg,
    Check { on: bool },
    /// A result glyph where the checkbox was.
    Status(OutcomeStatus),
    Label { title: String, meta: String, hint: Vec<String> },
    Pill(Pill),
    Note { lines: Vec<String>, color: Rgb },
    Checking,
    Message(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub rect: Rect,
    pub paint: Paint,
    pub act: Option<Act>,
}

impl Item {
    fn new(rect: Rect, paint: Paint) -> Item {
        Item { rect, paint, act: None }
    }
    fn act(mut self, a: Act) -> Item {
        self.act = Some(a);
        self
    }
}

enum Job {
    Scanned(Report, bool),
    Applied(Vec<Outcome>, bool, Option<f64>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    /// "End it anyway" on a row that stayed open.
    End(usize),
    /// CLEAN UP with rows that end processes checked: these rows, before anything runs.
    CleanUp(Vec<usize>),
}

/// "Spotify, iCUE and 3 leftover Unreal helpers"
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

/// The confirmation's words: (title, body, OK button). One app reads like "End it anyway";
/// one row that covers several processes (Unreal leftovers) says "them".
pub fn confirm_words(names: &[String], one_row_is_many: bool) -> (String, String, String) {
    match names {
        [one] if one_row_is_many => (format!("End {}?", one), "Anything unsaved in them is lost.".into(), "End them".into()),
        [one] => (format!("End {}?", one), "Anything unsaved in it is lost.".into(), "End it".into()),
        _ => (
            format!("End {} apps?", names.len()),
            format!("Anything unsaved in them is lost.\n{}.", and_list(names)),
            "End them".into(),
        ),
    }
}

pub struct DoctorWindow {
    source: Arc<Mutex<Box<dyn DoctorSource>>>,
    pub model: Option<DoctorModel>,
    pub confirm: Option<Confirm>,
    scanning: bool,
    working: bool,
    can_undo: bool,
    rx: Option<Receiver<Job>>,
    hwnd: isize,
    started: bool,
    scale: Scale,
    size: (i32, i32),
    mouse: Option<Pt>,
    pressed: Option<Act>,
    focus: Option<Act>,
    scroll: Scroll,
    anim: f32,
    items: Vec<Item>,
    menu_row: Option<usize>,
    /// Where commands from rows go (the tray's dispatcher); a no-op by default.
    commands: Box<dyn Fn(Command)>,
}

impl DoctorWindow {
    pub fn new(source: Box<dyn DoctorSource>) -> DoctorWindow {
        DoctorWindow {
            commands: Box::new(|_| {}),
            source: Arc::new(Mutex::new(source)),
            model: None,
            confirm: None,
            scanning: false,
            working: false,
            can_undo: false,
            rx: None,
            hwnd: 0,
            started: false,
            scale: Scale(1.0),
            size: (760, 720),
            mouse: None,
            pressed: None,
            focus: None,
            scroll: Scroll::default(),
            anim: 0.3,
            items: Vec::new(),
            menu_row: None,
        }
    }

    /// Send commands from rows ("Show me how") to `commands`.
    pub fn with_commands(mut self, commands: Box<dyn Fn(Command)>) -> DoctorWindow {
        self.commands = commands;
        self
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    /// Run a job on a worker thread (or right here when there is no window, as in tests and
    /// snapshots); its result comes back through `on_job`.
    fn run(&mut self, f: impl FnOnce(&dyn DoctorSource) -> Job + Send + 'static, host: Option<&mut Host>) {
        let src = self.source.clone();
        if self.hwnd == 0 {
            let job = {
                let s = src.lock().unwrap();
                f(s.as_ref())
            };
            self.on_job(job);
            return;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let hwnd = self.hwnd;
        std::thread::spawn(move || {
            let job = match src.lock() {
                Ok(s) => f(s.as_ref()),
                Err(_) => return,
            };
            let _ = tx.send(job);
            wake(hwnd, TIMER_WAKE);
        });
        if let Some(h) = host {
            h.set_timer(TIMER_ANIM, 16);
        }
    }

    /// Scan (again). Results replace everything shown.
    pub fn scan(&mut self, host: Option<&mut Host>) {
        self.scanning = true;
        self.confirm = None;
        self.run(|s| Job::Scanned(s.scan(), s.can_undo()), host);
    }

    /// CLEAN UP: run every checked row's picks. Ending processes always needs a second,
    /// explicit confirmation (docs/DESIGN.md), so when a checked row's pick is "End it" or
    /// "End them" this only opens the in-window confirmation listing them; its "End them"
    /// runs everything (`confirm_ok`), Cancel runs nothing.
    pub fn clean_up(&mut self, host: Option<&mut Host>) {
        let Some(m) = self.model.as_ref() else { return };
        if self.working || !m.any_checked() {
            return;
        }
        let ending = m.ending_rows();
        if !ending.is_empty() {
            self.confirm = Some(Confirm::CleanUp(ending));
            return;
        }
        self.run_clean_up(host);
    }

    /// Run every checked row's picks now (after any confirmation).
    fn run_clean_up(&mut self, host: Option<&mut Host>) {
        let Some(m) = self.model.as_mut() else { return };
        if self.working || !m.any_checked() {
            return;
        }
        let picks = m.begin_apply();
        let force = picks.iter().any(|(_, k)| *k == ActionKind::End);
        self.working = true;
        self.run(
            move |s| {
                let outs = s.apply(&picks, force);
                Job::Applied(outs, s.can_undo(), s.reclaimable_after_apply())
            },
            host,
        );
    }

    /// "End it anyway" after the confirmation.
    pub fn end_anyway(&mut self, i: usize, host: Option<&mut Host>) {
        let Some(m) = self.model.as_mut() else { return };
        let id = m.report.findings[i].id.clone();
        m.begin_one(i);
        self.working = true;
        self.run(
            move |s| {
                let outs = s.apply(&[(id, ActionKind::End)], true);
                Job::Applied(outs, s.can_undo(), s.reclaimable_after_apply())
            },
            host,
        );
    }

    pub fn undo(&mut self, host: Option<&mut Host>) {
        self.working = true;
        self.scanning = true;
        self.run(
            |s| {
                s.undo_last();
                Job::Scanned(s.scan(), s.can_undo())
            },
            host,
        );
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::Scanned(r, can_undo) => {
                self.model = Some(DoctorModel::new(r));
                self.can_undo = can_undo;
                self.scanning = false;
                self.working = false;
                self.scroll.y = 0;
                self.focus = None;
            }
            Job::Applied(outs, can_undo, reclaimable) => {
                if let Some(m) = self.model.as_mut() {
                    m.apply_outcomes(outs);
                    if let Some(gb) = reclaimable {
                        m.report.reclaimable_ram_gb = gb;
                    }
                }
                self.can_undo = can_undo;
                self.working = false;
            }
        }
    }

    fn poll(&mut self, host: &mut Host) {
        let job = self.rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(job) = job {
            self.rx = None;
            host.kill_timer(TIMER_ANIM);
            self.on_job(job);
            host.invalidate();
        }
    }

    pub fn set_mouse(&mut self, p: Option<Pt>) {
        self.mouse = p;
    }

    // ------------------------------------------------------------ layout

    fn footer_h(c: &Canvas) -> i32 {
        c.s(60.0)
    }

    /// Lay the page out; fills `self.items`.
    pub fn layout(&mut self, c: &Canvas) {
        let mut items = Vec::new();
        let w = c.width();
        let fh = Self::footer_h(c);
        self.scroll.view = Rect::new(0, 0, w, c.height() - fh);
        let content_w = (w - 2 * c.s(32.0)).min(c.s(1040.0));
        let left = (w - content_w) / 2;
        let right = left + content_w;
        let top = c.s(28.0) - self.scroll.y;
        let mut y = top;

        // Hero.
        let ls = c.s(64.0);
        items.push(Item::new(Rect::new(left, y, ls, ls), Paint::Logo));
        let tx = left + ls + c.s(18.0);
        let eh = c.line_height(F_EYEBROW);
        let hh = c.s(44.0);
        let ty = y + (ls - (eh + c.s(6.0) + hh)) / 2;
        let ew = c.measure_spaced("ATHER MEMGUARD", F_EYEBROW, c.s(3.0)).0 + c.s(4.0);
        let hw = c.measure("CHECKUP", F_HEADLINE).0 + c.s(4.0);
        items.push(Item::new(Rect::new(tx, ty, ew, eh), Paint::Eyebrow("ATHER MEMGUARD".into())));
        items.push(Item::new(Rect::new(tx, ty + eh + c.s(6.0), hw, hh), Paint::Headline("CHECKUP".into())));
        let mut hero_bottom = y + ls;
        if !self.scanning {
            if let Some(m) = &self.model {
                let (big, cap) = if m.all_clear() {
                    ("ALL CLEAR".to_string(), None)
                } else {
                    (format!("{:.1} GB", m.report.reclaimable_ram_gb), Some("COULD BE FREED"))
                };
                let rh = c.line_height(F_RESULT);
                let ch = c.line_height(F_CAPTION);
                let lh = c.line_height(F_HINT);
                let block = rh + cap.map(|_| ch + c.s(2.0)).unwrap_or(0) + c.s(6.0) + lh;
                let by = y + (ls - block) / 2;
                let rw = c.measure(&big, F_RESULT).0 + c.s(4.0);
                items.push(Item::new(Rect::new(right - rw, by, rw, rh), Paint::Result(big)));
                let mut yy = by + rh;
                if let Some(cap) = cap {
                    let cw = c.measure_spaced(cap, F_CAPTION, c.s(1.3)).0 + c.s(2.0);
                    items.push(Item::new(Rect::new(right - cw, yy + c.s(2.0), cw, ch), Paint::Caption(cap.into())));
                    yy += ch + c.s(2.0);
                }
                let lw = c.measure("Check again", F_HINT).0 + c.s(2.0);
                if !self.working {
                    items.push(Item::new(Rect::new(right - lw, yy + c.s(6.0), lw, lh), Paint::Link { label: "Check again".into(), color: MUTED }).act(Act::CheckAgain));
                }
                hero_bottom = hero_bottom.max(yy + c.s(6.0) + lh);
            }
        }
        y = hero_bottom + c.s(8.0);

        if self.scanning || self.model.is_none() {
            y += c.s(36.0);
            items.push(Item::new(Rect::new(left, y, content_w, c.s(40.0)), Paint::Checking));
            y += c.s(40.0);
        } else if let Some(m) = self.model.clone() {
            let sections = m.sections();
            if sections.is_empty() {
                y += c.s(36.0);
                let msg = "Nothing here needs your attention. Every app that's running is one Unreal work needs, or too small to matter.";
                let lines = c.wrap_lines(msg, F_BODY, content_w.min(c.s(560.0)), 3);
                let h = lines.len() as i32 * (c.line_height(F_BODY) + c.s(2.0));
                items.push(Item::new(Rect::new(left, y, content_w, h), Paint::Message(lines)));
                y += h;
            }
            for s in &sections {
                let lh = c.line_height(F_SECTION);
                let sh = c.s(26.0) + lh + c.s(8.0);
                let gb = if s.ram_mb > 0 { format_gb(s.ram_mb as f64 / 1024.0) } else { String::new() };
                let gw = if gb.is_empty() { 0 } else { c.measure(&gb, F_LABEL).0 + c.s(2.0) };
                items.push(Item::new(Rect::new(left, y + c.s(26.0), content_w - gw - c.s(16.0), lh), Paint::Section(s.kind.title().into())));
                if gw > 0 {
                    items.push(Item::new(Rect::new(right - gw, y + c.s(26.0) + (lh - c.line_height(F_LABEL)) / 2, gw, c.line_height(F_LABEL)), Paint::SectionGb(gb)));
                }
                items.push(Item::new(Rect::new(left, y + sh - c.px1(1.0), content_w, c.px1(1.0)), Paint::Hairline(BORDER)));
                y += sh;
                for &i in &s.rows {
                    y += self.layout_row(c, &mut items, &m, s.kind, i, left, right, y);
                }
            }
        }
        y += c.s(24.0);
        self.scroll.content = y - top + c.s(28.0);
        self.scroll.clamp();
        self.items = items;
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_row(&self, c: &Canvas, items: &mut Vec<Item>, m: &DoctorModel, sec: SectionKind, i: usize, x0: i32, x1: i32, y: i32) -> i32 {
        let f = &m.report.findings[i];
        let st = &m.rows[i];
        let cb = c.s(20.0);
        let tx = x0 + cb + c.s(16.0);
        let mut ctl_w = c.s(280.0).min((x1 - tx) / 2);
        if sec == SectionKind::ThisPc {
            // Notes with at most a link: the text gets the room a pill would take.
            ctl_w = help_link(f).map_or(0, |l| c.measure(l, F_HINT).0 + c.s(2.0)).min(ctl_w);
        }
        let gap = c.s(16.0);
        let text_w = x1 - tx - ctl_w - gap;
        let gb = format_gb(f.ram_mb as f64 / 1024.0);
        let procs = if f.processes == 1 { "1 process".to_string() } else { format!("{} processes", f.processes) };
        let (meta, hint) = match sec {
            SectionKind::NotNeeded | SectionKind::BigTools | SectionKind::HeldBack => (format!("{} · {}", procs, gb), f.detail.clone()),
            SectionKind::WorthALook => (if f.company.is_empty() { gb.clone() } else { format!("{} · {}", f.company, gb) }, f.detail.clone()),
            SectionKind::Startup => (
                String::new(),
                if f.ram_mb > 0 { format!("Starts every time you sign in. Uses about {}.", gb) } else { "Starts every time you sign in.".to_string() },
            ),
            SectionKind::Unreal => (gb.clone(), f.detail.clone()),
            SectionKind::ThisPc => (String::new(), f.detail.clone()),
        };
        let hint_lines = if hint.is_empty() { Vec::new() } else { wrap_hard(c, &hint, F_HINT, text_w, 2) };
        let lh = c.line_height(F_LABEL);
        let text_h = lh + hint_lines.len() as i32 * c.line_height(F_HINT);

        // Right column: a pill, a note, a result, or a link.
        enum Right {
            Pill(Pill, bool),
            Note(Vec<String>, Rgb, Option<(String, Act)>),
            Link(String, Act),
            Nothing,
        }
        let right = if st.busy {
            Right::Note(vec!["Working…".into()], MUTED, None)
        } else if !st.outcomes.is_empty() {
            let (t, col) = outcome_text(&st.outcomes);
            let link = if m.still_open(i) && !self.working { Some(("End it anyway".to_string(), Act::EndAnyway(i))) } else { None };
            Right::Note(wrap_hard(c, &t, F_HINT, ctl_w, 2), col, link)
        } else if f.needs_admin {
            Right::Note(vec!["Needs an administrator".into()], MUTED, None)
        } else if f.kind == FindingKind::System {
            match help_link(f) {
                Some(label) => Right::Link(label.into(), Act::HowToFix(i)),
                None => Right::Nothing,
            }
        } else {
            let ch = choices(f);
            match ch.get(st.choice) {
                Some(cur) => {
                    let mut p = Pill::new(&cur.label);
                    p.chevron = ch.len() > 1;
                    Right::Pill(p, ch.len() > 1)
                }
                None => Right::Nothing,
            }
        };
        let right_h = match &right {
            Right::Pill(..) => c.s(34.0),
            Right::Note(lines, _, link) => lines.len() as i32 * c.line_height(F_HINT) + if link.is_some() { c.s(4.0) + c.line_height(F_HINT) } else { 0 },
            Right::Link(..) => c.line_height(F_HINT),
            Right::Nothing => 0,
        };
        let h = c.s(66.0).max(text_h + c.s(16.0)).max(right_h + c.s(16.0));
        items.push(Item::new(Rect::new(x0, y, x1 - x0, h), Paint::RowBg));

        // Checkbox, or the result glyph.
        let cbr = Rect::new(x0, y + (h - cb) / 2, cb, cb);
        if let Some(o) = st.outcomes.iter().map(|o| o.status).max_by_key(|s| match s {
            OutcomeStatus::Failed => 3,
            OutcomeStatus::StillOpen => 2,
            OutcomeStatus::Skipped => 1,
            OutcomeStatus::Done => 0,
        }) {
            items.push(Item::new(cbr, Paint::Status(o)));
        } else if m.selectable(i) {
            items.push(Item::new(cbr, Paint::Check { on: st.checked }).act(Act::Check(i)));
        }
        items.push(Item::new(Rect::new(tx, y + (h - text_h) / 2, text_w, text_h), Paint::Label { title: f.title.clone(), meta, hint: hint_lines }));
        match right {
            Right::Pill(p, clickable) => {
                let pw = p.width(c, 120.0).min(ctl_w);
                let r = Rect::new(x1 - pw, y + (h - c.s(34.0)) / 2, pw, c.s(34.0));
                let it = Item::new(r, Paint::Pill(p));
                items.push(if clickable { it.act(Act::Choice(i)) } else { it });
            }
            Right::Note(lines, col, link) => {
                let ry = y + (h - right_h) / 2;
                let nh = lines.len() as i32 * c.line_height(F_HINT);
                let nw = lines.iter().map(|l| c.measure(l, F_HINT).0).max().unwrap_or(0) + c.s(2.0);
                items.push(Item::new(Rect::new(x1 - nw.min(ctl_w), ry, nw.min(ctl_w), nh), Paint::Note { lines, color: col }));
                if let Some((label, act)) = link {
                    let lw = c.measure(&label, F_HINT).0 + c.s(2.0);
                    items.push(Item::new(Rect::new(x1 - lw, ry + nh + c.s(4.0), lw, c.line_height(F_HINT)), Paint::Link { label, color: ERROR }).act(act));
                }
            }
            Right::Link(label, act) => {
                let lw = c.measure(&label, F_HINT).0 + c.s(2.0);
                let lhh = c.line_height(F_HINT);
                items.push(Item::new(Rect::new(x1 - lw, y + (h - lhh) / 2, lw, lhh), Paint::Link { label, color: MUTED }).act(act));
            }
            Right::Nothing => {}
        }
        items.push(Item::new(Rect::new(x0, y + h - c.px1(1.0), x1 - x0, c.px1(1.0)), Paint::Hairline(HAIRLINE)));
        h
    }

    /// Footer pieces: (text rect, undo link rect, clean-up button rect).
    pub fn footer_layout(&self, c: &Canvas) -> (Rect, Option<Rect>, Rect) {
        let fh = Self::footer_h(c);
        let y = c.height() - fh;
        let w = c.width();
        let pad = c.s(32.0);
        let b = Button::new("CLEAN UP", ButtonStyle::Primary);
        let (bw, bh) = b.size(c);
        let bw = bw + c.s(16.0);
        let br = Rect::new(w - pad - bw, y + (fh - bh) / 2, bw, bh);
        let undo = if self.can_undo && !self.working {
            let lw = c.measure("Undo last cleanup", F_HINT).0 + c.s(2.0);
            let lh = c.line_height(F_HINT);
            Some(Rect::new(br.x - c.s(20.0) - lw, y + (fh - lh) / 2, lw, lh))
        } else {
            None
        };
        let tr = Rect::new(pad, y, undo.map(|u| u.x).unwrap_or(br.x) - pad - c.s(16.0), fh);
        (tr, undo, br)
    }

    fn footer_text(&self) -> String {
        match &self.model {
            Some(m) if !self.scanning && (0..m.rows.len()).any(|i| m.selectable(i)) => format!("Selected: frees about {:.1} GB", m.selected_gb()),
            _ => String::new(),
        }
    }

    fn can_clean(&self) -> bool {
        !self.working && !self.scanning && self.model.as_ref().map(|m| m.any_checked()).unwrap_or(false)
    }

    // ------------------------------------------------------------ painting

    fn state_for(&self, act: Option<Act>, r: Rect) -> WState {
        let over = self.confirm.is_none() && act.is_some() && self.mouse.map(|m| r.contains(m) && self.scroll.view.contains(m)).unwrap_or(false);
        WState { hover: over, pressed: over && self.pressed == act, focused: act.is_some() && self.focus == act, disabled: false, mouse: if over { self.mouse } else { None } }
    }

    fn row_hovered(&self, r: Rect) -> bool {
        self.confirm.is_none() && self.mouse.map(|m| r.contains(m) && self.scroll.view.contains(m)).unwrap_or(false)
    }

    fn paint_item(&self, c: &mut Canvas, it: &Item) {
        let st = self.state_for(it.act, it.rect);
        let r = it.rect;
        match &it.paint {
            Paint::Logo => c.logo(r),
            Paint::Eyebrow(s) => c.spaced_text(r, s, F_EYEBROW, MUTED, c.s(3.0), Align::Left),
            Paint::Headline(s) => c.text(r, s, F_HEADLINE, TEXT, Align::Left),
            Paint::Result(s) => c.text(r, s, F_RESULT, ACCENT, Align::Right),
            Paint::Caption(s) => c.spaced_text(r, s, F_CAPTION, ACCENT, c.s(1.3), Align::Right),
            Paint::Link { label, color } => paint_link(c, r, label, F_HINT, *color, &st),
            Paint::Section(s) => c.spaced_text(r, s, F_SECTION, ACCENT, c.s(3.1), Align::Left),
            Paint::SectionGb(s) => c.text(r, s, F_LABEL, TEXT_DIM, Align::Right),
            Paint::Hairline(col) => c.fill_rect(r, *col),
            Paint::RowBg => {
                if self.row_hovered(r) {
                    let g = c.s(12.0);
                    c.fill_round_rect(r.inset(-g, 0), c.s(RADIUS), SELECTED);
                }
            }
            Paint::Check { on } => {
                let rad = c.s(4.0);
                if *on {
                    c.fill_round_rect(r, rad, if st.hover { ACCENT_SOFT } else { ACCENT });
                    c.icon(r, '\u{E73E}', Font::new(Face::Icon, 12.0, REGULAR), ON_ACCENT);
                } else {
                    c.fill_round_rect_bordered(r, rad, if st.hover { SELECTED } else { RAISED }, if st.hover { MUTED } else { BORDER });
                }
                if st.focused {
                    super::kit::focus_ring(c, r, rad);
                }
            }
            Paint::Status(s) => {
                let (g, col) = match s {
                    OutcomeStatus::Done => ('\u{E73E}', ACCENT),
                    OutcomeStatus::StillOpen => ('\u{E7BA}', WARN),
                    OutcomeStatus::Failed => ('\u{E711}', ERROR),
                    OutcomeStatus::Skipped => ('\u{E738}', MUTED),
                };
                c.icon(r, g, Font::new(Face::Icon, 14.0, REGULAR), col);
            }
            Paint::Label { title, meta, hint } => {
                let lh = c.line_height(F_LABEL);
                let (tw, _) = c.measure(title, F_LABEL);
                let tw = tw.min(r.w);
                c.text(Rect::new(r.x, r.y, tw + c.s(2.0), lh), title, F_LABEL, TEXT, Align::Left);
                if !meta.is_empty() {
                    let mx = r.x + tw + c.s(10.0);
                    if mx < r.right() - c.s(40.0) {
                        c.text(Rect::new(mx, r.y, r.right() - mx, lh), meta, F_HINT, MUTED, Align::Left);
                    }
                }
                let hh = c.line_height(F_HINT);
                for (k, line) in hint.iter().enumerate() {
                    c.text_top(Rect::new(r.x, r.y + lh + k as i32 * hh, r.w, hh), line, F_HINT, MUTED, Align::Left);
                }
            }
            Paint::Pill(p) => p.paint(c, r, &st),
            Paint::Note { lines, color } => {
                let hh = c.line_height(F_HINT);
                for (k, line) in lines.iter().enumerate() {
                    c.text_top(Rect::new(r.x, r.y + k as i32 * hh, r.w, hh), line, F_HINT, *color, Align::Right);
                }
            }
            Paint::Checking => {
                let th = c.line_height(F_BODY);
                c.text(Rect::new(r.x, r.y, r.w, th), "Checking…", F_BODY, TEXT_DIM, Align::Left);
                indeterminate_bar(c, Rect::new(r.x, r.y + th + c.s(12.0), r.w, c.s(3.0).max(2)), self.anim);
            }
            Paint::Message(lines) => {
                let lh = c.line_height(F_BODY) + c.s(2.0);
                for (k, l) in lines.iter().enumerate() {
                    c.text(Rect::new(r.x, r.y + k as i32 * lh, r.w, lh), l, F_BODY, TEXT_DIM, Align::Left);
                }
            }
        }
    }

    fn paint_footer(&self, c: &mut Canvas) {
        let fh = Self::footer_h(c);
        let y = c.height() - fh;
        let w = c.width();
        c.fill_rect(Rect::new(0, y, w, fh), SURFACE);
        c.hline(0, y, w, BORDER);
        let (tr, undo, br) = self.footer_layout(c);
        let text = self.footer_text();
        if !text.is_empty() {
            // Canvas::text must not get an empty string (DrawTextW reads past it with DT_END_ELLIPSIS).
            c.text(tr, &text, F_VALUE, TEXT_DIM, Align::Left);
        }
        if let Some(u) = undo {
            paint_link(c, u, "Undo last cleanup", F_HINT, MUTED, &self.state_for(Some(Act::Undo), u));
        }
        let enabled = self.can_clean();
        let mut st = self.state_for(Some(Act::CleanUp), br);
        if enabled {
            Button::new("CLEAN UP", ButtonStyle::Primary).paint(c, br, &st);
        } else {
            st.disabled = true;
            st.hover = false;
            Button::new("CLEAN UP", ButtonStyle::Normal).paint(c, br, &st);
        }
    }

    fn dialog(&self) -> Option<Dialog> {
        let m = self.model.as_ref()?;
        let (title, body, ok) = match self.confirm.as_ref()? {
            Confirm::End(i) => confirm_words(&[m.report.findings.get(*i)?.title.clone()], false),
            Confirm::CleanUp(rows) => {
                let names: Vec<String> = rows.iter().filter_map(|&i| m.report.findings.get(i)).map(|f| f.title.clone()).collect();
                let many = rows.len() == 1 && m.chosen(rows[0]).is_some_and(|c| c.label == "End them");
                confirm_words(&names, many)
            }
        };
        Some(Dialog {
            title,
            body,
            ok: Button::new(&ok, ButtonStyle::Danger),
            cancel: Button::new("Cancel", ButtonStyle::Normal),
        })
    }

    // ------------------------------------------------------------ input

    fn hit(&self, p: Pt) -> Option<Act> {
        let probe = Canvas::new(self.size.0.max(1), self.size.1.max(1), self.scale);
        let (_, undo, br) = self.footer_layout(&probe);
        if br.contains(p) {
            return Some(Act::CleanUp);
        }
        if undo.map(|u| u.contains(p)).unwrap_or(false) {
            return Some(Act::Undo);
        }
        if !self.scroll.view.contains(p) {
            return None;
        }
        self.items.iter().rev().find(|i| i.act.is_some() && i.rect.contains(p)).and_then(|i| i.act)
    }

    /// The checkbox of the row under `p` (clicking anywhere on a row toggles it).
    fn row_check_at(&self, p: Pt) -> Option<Act> {
        if !self.scroll.view.contains(p) {
            return None;
        }
        let row = self.items.iter().find(|i| matches!(i.paint, Paint::RowBg) && i.rect.contains(p))?;
        self.items.iter().find(|i| matches!(i.act, Some(Act::Check(_))) && row.rect.encloses(&i.rect)).and_then(|i| i.act)
    }

    pub fn activate(&mut self, a: Act, host: &mut Host) {
        match a {
            Act::Check(i) => {
                if let Some(m) = self.model.as_mut() {
                    m.toggle(i);
                }
            }
            Act::Choice(i) => {
                let Some(m) = &self.model else { return };
                let f = &m.report.findings[i];
                let cur = m.rows[i].choice;
                let items: Vec<MenuItem> =
                    choices(f).iter().enumerate().map(|(k, ch)| MenuItem::new(k as u32 + 1, &ch.label).checked(k == cur)).collect();
                let at = self.items.iter().find(|it| it.act == Some(a)).map(|it| Pt::new(it.rect.x, it.rect.bottom())).unwrap_or_default();
                self.menu_row = Some(i);
                host.menu(MENU_CHOICE, items, at);
            }
            Act::EndAnyway(i) => self.confirm = Some(Confirm::End(i)),
            Act::HowToFix(i) => {
                let Some(f) = self.model.as_ref().and_then(|m| m.report.findings.get(i)) else { return };
                if let Some(cmd) = f.help_command {
                    (self.commands)(cmd);
                } else if let Some(url) = f.help_url.clone() {
                    host.shell_open(&url);
                }
            }
            Act::CheckAgain => {
                if !self.working {
                    self.scan(Some(host));
                }
            }
            Act::CleanUp => self.clean_up(Some(host)),
            Act::Undo => {
                if self.can_undo && !self.working {
                    self.undo(Some(host));
                }
            }
        }
        host.invalidate();
    }

    fn focusables(&self) -> Vec<Act> {
        let mut out: Vec<Act> = self.items.iter().filter_map(|i| i.act).collect();
        if self.can_undo && !self.working {
            out.push(Act::Undo);
        }
        if self.can_clean() {
            out.push(Act::CleanUp);
        }
        out
    }

    fn confirm_ok(&mut self, host: &mut Host) {
        match self.confirm.take() {
            Some(Confirm::End(i)) => self.end_anyway(i, Some(host)),
            Some(Confirm::CleanUp(_)) => self.run_clean_up(Some(host)),
            None => {}
        }
    }
}

impl Surface for DoctorWindow {
    fn paint(&mut self, c: &mut Canvas) {
        self.scale = c.scale;
        self.size = (c.width(), c.height());
        self.layout(c);
        c.fill_rect(c.bounds(), BG);
        let view = self.scroll.view;
        c.push_clip(view);
        let items = std::mem::take(&mut self.items);
        for it in &items {
            if it.rect.intersects(&view) {
                self.paint_item(c, it);
            }
        }
        self.items = items;
        c.pop_clip();
        self.scroll.paint(c, false);
        self.paint_footer(c);
        if let Some(d) = self.dialog() {
            d.paint(c, c.bounds(), self.mouse, false);
        }
    }

    fn event(&mut self, ev: Event, host: &mut Host) -> bool {
        self.scale = host.scale;
        if !host.hwnd.is_null() {
            self.hwnd = host.hwnd as isize;
        }
        match ev {
            Event::Resized => {
                if !self.started && self.model.is_none() {
                    self.started = true;
                    self.scan(Some(host));
                }
                host.invalidate();
            }
            Event::Timer(TIMER_WAKE) => self.poll(host),
            Event::Timer(TIMER_ANIM) => {
                self.anim = (self.anim + 0.012).rem_euclid(1.0);
                self.poll(host);
                host.invalidate();
            }
            Event::MouseMove(p) => {
                self.mouse = Some(p);
                let hand = self.confirm.is_none() && (self.hit(p).is_some() || self.row_check_at(p).is_some());
                host.set_cursor(if hand { Cursor::Hand } else { Cursor::Arrow });
                host.invalidate();
            }
            Event::MouseLeave => {
                self.mouse = None;
                host.invalidate();
            }
            Event::MouseDown(p, MouseButton::Left) => {
                if self.confirm.is_none() {
                    self.pressed = self.hit(p).or_else(|| self.row_check_at(p));
                    self.focus = self.pressed;
                }
                host.invalidate();
            }
            Event::MouseUp(p, MouseButton::Left) => {
                if self.confirm.is_some() {
                    if let Some(d) = self.dialog() {
                        let probe = Canvas::new(self.size.0.max(1), self.size.1.max(1), self.scale);
                        let (_, ok, cancel) = d.layout(&probe, probe.bounds());
                        if ok.contains(p) {
                            self.confirm_ok(host);
                        } else if cancel.contains(p) {
                            self.confirm = None;
                        }
                    }
                    host.invalidate();
                    return true;
                }
                let pressed = self.pressed.take();
                let now = self.hit(p).or_else(|| self.row_check_at(p));
                if let (Some(a), Some(b)) = (pressed, now) {
                    if a == b {
                        self.activate(a, host);
                    }
                }
                host.invalidate();
            }
            Event::Menu { choice, .. } => {
                if let (Some(i), Some(k), Some(m)) = (self.menu_row.take(), choice, self.model.as_mut()) {
                    m.set_choice(i, k as usize - 1);
                }
                host.invalidate();
            }
            Event::Wheel(delta) => {
                self.scroll.wheel(delta, (self.scale.0 * 66.0) as i32);
                host.invalidate();
            }
            Event::Key(vk, m) => {
                if m.alt {
                    return false;
                }
                if self.confirm.is_some() {
                    if vk == VK_ESCAPE || vk == VK_RETURN {
                        self.confirm = None; // Enter is the safe choice
                    }
                    host.invalidate();
                    return true;
                }
                match vk {
                    VK_ESCAPE => host.close(),
                    0x57 if m.ctrl => host.close(),
                    VK_F5 => self.activate(Act::CheckAgain, host),
                    VK_TAB => {
                        let list = self.focusables();
                        if !list.is_empty() {
                            let n = list.len();
                            let i = self.focus.and_then(|f| list.iter().position(|a| *a == f));
                            let next = match (i, m.shift) {
                                (None, false) => 0,
                                (None, true) => n - 1,
                                (Some(i), false) => (i + 1) % n,
                                (Some(i), true) => (i + n - 1) % n,
                            };
                            self.focus = Some(list[next]);
                            if let Some(r) = self.items.iter().find(|it| it.act == Some(list[next])).map(|it| it.rect) {
                                self.scroll.reveal(r, (self.scale.0 * 24.0) as i32);
                            }
                        }
                        host.invalidate();
                    }
                    VK_RETURN | VK_SPACE => {
                        if let Some(a) = self.focus {
                            self.activate(a, host);
                        }
                    }
                    VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT => {
                        let step = if vk == VK_PRIOR || vk == VK_NEXT { self.scroll.view.h - (self.scale.0 * 40.0) as i32 } else { (self.scale.0 * 40.0) as i32 };
                        self.scroll.y += if vk == VK_UP || vk == VK_PRIOR { -step } else { step };
                        self.scroll.clamp();
                        host.invalidate();
                    }
                    _ => {}
                }
            }
            Event::Destroyed => {
                if let Some(r) = host.window_rect() {
                    let _ = super::ui_state::save(KIND, r);
                }
            }
            _ => return false,
        }
        true
    }
}

/// Open the checkup window (it scans right away), or bring the open one to the front.
/// Call on the UI thread. `commands` receives what rows ask for ("Show me how").
pub fn open_doctor(source: Box<dyn DoctorSource>, commands: Box<dyn Fn(Command)>) -> HWND {
    if let Some(h) = window::find(KIND) {
        window::bring_to_front(h);
        return h;
    }
    let spec = WindowSpec { kind: KIND, title: TITLE.into(), size: (760, 720), min_size: (620, 480), placement: super::ui_state::load(KIND), resizable: true };
    window::open(spec, Box::new(DoctorWindow::new(source).with_commands(commands)))
}

/// Paint the window at `w` (96-DPI px) wide and `h` high, or the whole page with `h == None`.
pub fn render(win: &mut DoctorWindow, scale: Scale, w: i32, h: Option<i32>) -> Canvas {
    let pw = scale.s(w as f32);
    let ph = match h {
        Some(h) => scale.s(h as f32),
        None => {
            let probe = Canvas::new(pw, scale.s(720.0), scale);
            win.layout(&probe);
            win.scroll.content + DoctorWindow::footer_h(&probe)
        }
    };
    let mut c = Canvas::new(pw, ph, scale);
    win.paint(&mut c);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> DoctorModel {
        DoctorModel::new(FakeDoctor::report(false))
    }

    fn idx(m: &DoctorModel, id: &str) -> usize {
        m.report.findings.iter().position(|f| f.id == id).unwrap()
    }

    #[test]
    fn sections_follow_the_spec_order_and_hide_empty_ones() {
        let m = model();
        let kinds: Vec<SectionKind> = m.sections().iter().map(|s| s.kind).collect();
        assert_eq!(kinds, SectionKind::ORDER.to_vec());
        let not_needed = &m.sections()[0];
        assert_eq!(not_needed.rows.len(), 5);
        assert_eq!(not_needed.ram_mb, 1270 + 980 + 640 + 410 + 560);
        assert_eq!(m.sections()[6].ram_mb, 0, "THIS PC shows no memory");
        let clear = DoctorModel::new(FakeDoctor::report(true));
        assert!(clear.sections().is_empty());
        assert!(clear.all_clear());
        assert!(!m.all_clear());
    }

    #[test]
    fn recommended_rows_start_checked() {
        let m = model();
        let checked = |id: &str| m.rows[idx(&m, id)].checked;
        assert!(checked("app:steam") && checked("app:epicgameslauncher") && checked("app:widgets"));
        assert!(!checked("app:spotify") && !checked("app:icue"), "apps with a window are the person's pick");
        assert!(checked("startup:hkcu:Steam") && checked("startup:hkcu:Spotify"));
        assert!(!checked("app:blender") && !checked("app:nwsync"), "big tools and unknown apps start unchecked");
        assert!(checked("app:discord") && checked("app:dropbox"), "HELD BACK rows start checked");
        assert!(!checked("ue:orphans"), "ending processes is never pre-checked");
        assert!(!checked("sys:pagefile"));
    }

    #[test]
    fn admin_and_system_rows_cannot_be_selected() {
        let mut m = model();
        let admin = idx(&m, "startup:hklm:iCUE");
        let sys = idx(&m, "sys:pagefile");
        assert!(!m.selectable(admin) && !m.selectable(sys));
        m.toggle(admin);
        m.toggle(sys);
        assert!(!m.rows[admin].checked && !m.rows[sys].checked);
        assert!(m.picks().iter().all(|(id, _)| id != "startup:hklm:iCUE" && id != "sys:pagefile"));
    }

    #[test]
    fn running_total_and_picks() {
        let mut m = model();
        // Steam (close) 1270 + Widgets (close) 410; Epic's default is "Don't start with Windows",
        // startup rows and hold-backs free nothing right now.
        assert!((m.selected_gb() - (1270.0 + 410.0) / 1024.0).abs() < 1e-9);
        let epic = idx(&m, "app:epicgameslauncher");
        assert_eq!(choices(&m.report.findings[epic])[1].label, "Close");
        m.set_choice(epic, 1);
        assert!((m.selected_gb() - (1270.0 + 410.0 + 980.0) / 1024.0).abs() < 1e-9);
        let spotify = idx(&m, "app:spotify");
        m.toggle(spotify);
        m.set_choice(spotify, 1);
        assert!(m.picks().contains(&("app:spotify".into(), ActionKind::BlockWhileUnreal)));
        assert!(m.picks().contains(&("app:spotify".into(), ActionKind::Close)));
        // Unchecking everything: nothing to clean up.
        for i in 0..m.rows.len() {
            m.rows[i].checked = false;
        }
        assert!(!m.any_checked());
        assert_eq!(m.selected_gb(), 0.0);
    }

    #[test]
    fn pill_wording() {
        let m = model();
        let labels = |id: &str| choices(&m.report.findings[idx(&m, id)]).into_iter().map(|c| c.label).collect::<Vec<_>>();
        assert_eq!(labels("app:steam"), ["Close", "Close and don't start with Windows", "Close whenever Unreal is running"]);
        assert_eq!(labels("startup:hkcu:Steam"), ["Don't start with Windows"]);
        assert_eq!(labels("app:blender"), ["Close if you're not using it"]);
        assert_eq!(labels("app:nwsync"), ["Close", "Hold back", "Leave it"]);
        assert_eq!(labels("app:discord"), ["Hold back"]);
        assert_eq!(labels("ue:orphans"), ["End them"]);
        assert!(labels("sys:pagefile").is_empty());
    }

    #[test]
    fn results_replace_rows() {
        let mut m = model();
        let spotify = idx(&m, "app:spotify");
        m.toggle(spotify);
        let picks = m.begin_apply();
        assert!(m.rows.iter().enumerate().all(|(i, r)| r.busy == picks.iter().any(|(id, _)| *id == m.report.findings[i].id)));
        let fake = FakeDoctor::new();
        m.apply_outcomes(fake.apply(&picks, false));
        assert!(m.rows.iter().all(|r| !r.busy));
        let steam = idx(&m, "app:steam");
        assert_eq!(outcome_text(&m.rows[steam].outcomes).0, "Closed");
        assert_eq!(outcome_text(&m.rows[idx(&m, "startup:hkcu:Steam")].outcomes).0, "Switched off");
        assert_eq!(outcome_text(&m.rows[idx(&m, "app:discord")].outcomes).0, "Added to Chat");
        assert!(m.still_open(spotify));
        assert_eq!(outcome_text(&m.rows[spotify].outcomes), ("Still open — it may be asking you to save".to_string(), WARN));
        // Done rows can't be picked again; untouched rows still can.
        assert!(!m.selectable(steam));
        assert!(m.selectable(idx(&m, "app:icue")));
        // End it anyway replaces the still-open result.
        m.begin_one(spotify);
        m.apply_outcomes(fake.apply(&[("app:spotify".into(), ActionKind::End)], true));
        assert!(!m.still_open(spotify));
        assert_eq!(outcome_text(&m.rows[spotify].outcomes).0, "Ended");
    }

    #[test]
    fn window_flow_scan_clean_up_end_undo() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        assert!(w.model.is_some() && !w.is_scanning());
        assert!(!w.can_undo);
        w.activate(Act::CleanUp, &mut host);
        assert!(w.can_undo);
        let m = w.model.as_ref().unwrap();
        assert!(m.rows.iter().any(|r| !r.outcomes.is_empty()));
        // Spotify wasn't checked, so nothing is still open; check it and clean up again.
        let spotify = idx(m, "app:spotify");
        w.activate(Act::Check(spotify), &mut host);
        w.activate(Act::CleanUp, &mut host);
        assert!(w.model.as_ref().unwrap().still_open(spotify));
        w.activate(Act::EndAnyway(spotify), &mut host);
        assert_eq!(w.confirm, Some(Confirm::End(spotify)));
        assert_eq!(w.dialog().unwrap().title, "End Spotify?");
        w.confirm_ok(&mut host);
        assert!(!w.model.as_ref().unwrap().still_open(spotify));
        w.activate(Act::Undo, &mut host);
        assert!(!w.can_undo);
        assert_eq!(w.model.as_ref().unwrap().rows.iter().filter(|r| !r.outcomes.is_empty()).count(), 0, "undo re-scans");
    }

    #[test]
    fn ending_rows_need_a_pick_and_a_check() {
        let mut m = model();
        assert!(m.ending_rows().is_empty(), "nothing that ends processes starts checked");
        let icue = idx(&m, "app:icue");
        let orphans = idx(&m, "ue:orphans");
        let end_it = choices(&m.report.findings[icue]).iter().position(|c| c.label == "End it").unwrap();
        // Picked but not checked: nothing.
        m.set_choice(icue, end_it);
        assert!(m.ending_rows().is_empty());
        m.toggle(icue);
        assert_eq!(m.ending_rows(), vec![icue]);
        m.toggle(orphans);
        assert_eq!(m.ending_rows(), vec![icue, orphans], "leftover helpers count too");
        // Back to "Close": only the helpers end.
        m.set_choice(icue, 0);
        assert_eq!(m.ending_rows(), vec![orphans]);
    }

    #[test]
    fn confirmation_words() {
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            confirm_words(&n(&["iCUE", "3 leftover Unreal helpers from a closed editor"]), false),
            (
                "End 2 apps?".to_string(),
                "Anything unsaved in them is lost.\niCUE and 3 leftover Unreal helpers from a closed editor.".to_string(),
                "End them".to_string()
            )
        );
        assert_eq!(confirm_words(&n(&["A", "B", "C"]), false).1, "Anything unsaved in them is lost.\nA, B and C.");
        assert_eq!(
            confirm_words(&n(&["Spotify"]), false),
            ("End Spotify?".to_string(), "Anything unsaved in it is lost.".to_string(), "End it".to_string())
        );
        assert_eq!(confirm_words(&n(&["3 leftover Unreal helpers"]), true).2, "End them");
    }

    #[test]
    fn clean_up_asks_before_ending_anything() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        let m = w.model.as_ref().unwrap();
        let (icue, orphans, steam) = (idx(m, "app:icue"), idx(m, "ue:orphans"), idx(m, "app:steam"));
        let end_it = choices(&m.report.findings[icue]).iter().position(|c| c.label == "End it").unwrap();
        w.activate(Act::Check(icue), &mut host);
        w.model.as_mut().unwrap().set_choice(icue, end_it);
        w.activate(Act::Check(orphans), &mut host);

        // CLEAN UP only asks: nothing runs, nothing changes.
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.confirm, Some(Confirm::CleanUp(vec![icue, orphans])));
        let d = w.dialog().unwrap();
        assert_eq!(d.title, "End 2 apps?");
        assert_eq!(d.body, "Anything unsaved in them is lost.\niCUE and 3 leftover Unreal helpers from a closed editor.");
        assert_eq!((d.ok.label.as_str(), d.cancel.label.as_str()), ("End them", "Cancel"));
        let m = w.model.as_ref().unwrap();
        assert!(m.rows.iter().all(|r| r.outcomes.is_empty() && !r.busy), "nothing ran before the answer");
        assert!(!w.can_undo);

        // Cancel (Enter is Cancel, the focused default; Esc too): nothing ran, the picks stay.
        w.event(Event::Key(VK_RETURN, Default::default()), &mut host);
        assert_eq!(w.confirm, None);
        let m = w.model.as_ref().unwrap();
        assert!(m.rows.iter().all(|r| r.outcomes.is_empty()));
        assert!(m.rows[icue].checked && m.rows[orphans].checked && m.rows[steam].checked);
        w.activate(Act::CleanUp, &mut host);
        w.event(Event::Key(VK_ESCAPE, Default::default()), &mut host);
        assert_eq!(w.confirm, None);
        assert!(w.model.as_ref().unwrap().rows.iter().all(|r| r.outcomes.is_empty()));

        // "End them": everything checked runs, the ends included.
        w.activate(Act::CleanUp, &mut host);
        w.confirm_ok(&mut host);
        assert_eq!(w.confirm, None);
        let m = w.model.as_ref().unwrap();
        assert_eq!(outcome_text(&m.rows[icue].outcomes).0, "Ended");
        assert_eq!(outcome_text(&m.rows[orphans].outcomes).0, "Ended 3 helpers");
        assert_eq!(outcome_text(&m.rows[steam].outcomes).0, "Closed");
        assert!(w.can_undo);
    }

    #[test]
    fn one_leftover_row_alone_still_asks() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        let m = w.model.as_mut().unwrap();
        for r in &mut m.rows {
            r.checked = false;
        }
        let orphans = idx(m, "ue:orphans");
        m.toggle(orphans);
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.confirm, Some(Confirm::CleanUp(vec![orphans])));
        let d = w.dialog().unwrap();
        assert_eq!(d.title, "End 3 leftover Unreal helpers from a closed editor?");
        assert_eq!(d.body, "Anything unsaved in them is lost.");
        assert_eq!(d.ok.label, "End them");
        // Clicking Cancel in the dialog.
        let probe = Canvas::new(760, 720, Scale(1.0));
        w.size = (760, 720);
        let (_, ok, cancel) = d.layout(&probe, probe.bounds());
        w.event(Event::MouseUp(Pt::new(cancel.cx(), cancel.cy()), MouseButton::Left), &mut host);
        assert_eq!(w.confirm, None);
        assert!(w.model.as_ref().unwrap().rows[orphans].outcomes.is_empty());
        // Clicking "End them".
        w.activate(Act::CleanUp, &mut host);
        w.event(Event::MouseUp(Pt::new(ok.cx(), ok.cy()), MouseButton::Left), &mut host);
        assert_eq!(outcome_text(&w.model.as_ref().unwrap().rows[orphans].outcomes).0, "Ended 3 helpers");
    }

    #[test]
    fn this_pc_links() {
        let sent: std::rc::Rc<std::cell::RefCell<Vec<Command>>> = Default::default();
        let log = sent.clone();
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new())).with_commands(Box::new(move |c| log.borrow_mut().push(c)));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        let c = Canvas::new(760, 3000, Scale(1.0));
        w.layout(&c);
        let m = w.model.clone().unwrap();
        let ram = idx(&m, "sys:ram");
        let page = idx(&m, "sys:pagefile");
        assert_eq!(help_link(&m.report.findings[ram]), Some("Show me how"));
        assert_eq!(help_link(&m.report.findings[page]), Some("How to fix"));
        assert!(!m.selectable(ram) && !m.rows[ram].checked, "the RAM row is a note, never a pick");
        let link = |w: &DoctorWindow, i: usize| {
            w.items().iter().find(|it| it.act == Some(Act::HowToFix(i))).map(|it| it.paint.clone())
        };
        assert_eq!(link(&w, ram), Some(Paint::Link { label: "Show me how".into(), color: MUTED }));
        // "Show me how" sends the command (the tray writes and opens the page); it opens nothing itself.
        w.activate(Act::HowToFix(ram), &mut host);
        assert_eq!(*sent.borrow(), vec![Command::ShowRamReport]);
        assert!(host.shell_targets().is_empty());
        // "How to fix" opens its page and sends nothing.
        w.activate(Act::HowToFix(page), &mut host);
        assert_eq!(sent.borrow().len(), 1);
        assert_eq!(host.shell_targets().len(), 1);
        assert!(host.shell_targets()[0].ends_with("PAGEFILE.md"));
    }

    fn check_layout(w: &mut DoctorWindow, scale: f32, width: i32, ctx: &str) {
        let s = Scale(scale);
        let probe = Canvas::new(s.s(width as f32), s.s(720.0), s);
        w.layout(&probe);
        let h = w.scroll.content + DoctorWindow::footer_h(&probe);
        let c = Canvas::new(s.s(width as f32), h, s);
        w.layout(&c);
        let items = w.items().to_vec();
        let ctx = format!("{ctx} scale {scale} width {width}");
        for it in &items {
            assert!(it.rect.x >= 0 && it.rect.right() <= c.width(), "{ctx}: {:?} outside", it.paint);
        }
        let solid: Vec<&Item> = items.iter().filter(|i| !matches!(i.paint, Paint::RowBg | Paint::Hairline(_))).collect();
        for (i, a) in solid.iter().enumerate() {
            for b in &solid[i + 1..] {
                assert!(!a.rect.intersects(&b.rect), "{ctx}: {:?} overlaps {:?}", a.paint, b.paint);
            }
        }
        let rows: Vec<&Item> = items.iter().filter(|i| matches!(i.paint, Paint::RowBg)).collect();
        for it in items.iter().filter(|i| matches!(i.paint, Paint::Check { .. } | Paint::Pill(_) | Paint::Label { .. } | Paint::Note { .. })) {
            assert!(rows.iter().any(|r| r.rect.encloses(&it.rect)), "{ctx}: {:?} not inside a row", it.paint);
        }
        let (tr, undo, br) = w.footer_layout(&c);
        assert!(!tr.intersects(&br), "{ctx}: footer text / button");
        if let Some(u) = undo {
            assert!(!u.intersects(&br) && !u.intersects(&tr), "{ctx}: undo link");
        }
        assert!(tr.w >= c.measure("Selected: frees about 10.0 GB", F_VALUE).0, "{ctx}: footer text cut");
    }

    #[test]
    fn paints_while_checking() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut c = Canvas::new(760, 720, Scale(1.0));
        w.paint(&mut c);
        // Every state paints (an empty footer text once crashed DrawTextW).
        let mut clear = DoctorWindow::new(Box::new(FakeDoctor::all_clear()));
        clear.scan(None);
        clear.paint(&mut c);
        w.scan(None);
        w.paint(&mut c);
        w.clean_up(None);
        w.paint(&mut c);
    }

    #[test]
    fn layout_does_not_overlap_at_two_scales() {
        for scale in [1.0, 1.5] {
            for width in [760, 620] {
                let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
                check_layout(&mut w, scale, width, "scanning");
                w.scan(None);
                check_layout(&mut w, scale, width, "report");
                let spotify = idx(w.model.as_ref().unwrap(), "app:spotify");
                w.model.as_mut().unwrap().toggle(spotify);
                w.clean_up(None);
                check_layout(&mut w, scale, width, "results");
                let mut clear = DoctorWindow::new(Box::new(FakeDoctor::all_clear()));
                clear.scan(None);
                check_layout(&mut clear, scale, width, "all clear");
            }
        }
    }
}

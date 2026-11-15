//! The checkup window, the doctor (docs/UI.md section 6): scan on open, two tabs that never
//! mix ("Memory": what closing apps frees in RAM; "Disk space": what can go from the drives),
//! findings grouped in sections, checkboxes pre-checked from `recommended`, action pills, a
//! running total per tab, FREE MEMORY / FREE DISK SPACE on a worker thread with every row
//! turning into its result in place, "End it anyway" with an in-window confirmation, "Undo
//! last cleanup" per tab, "Check again" and the ALL CLEAR state.
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
/// Posted by `open_doctor` to an open window: show this tab.
const TIMER_TAB_MEMORY: usize = 7103;
const TIMER_TAB_DISK: usize = 7104;
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
    /// Addition: when the last disk checkup ran (its drive rows are in `findings`); `None` =
    /// no disk checkup yet, and the Disk space tab explains it and offers to run one.
    pub disk_checked: Option<String>,
}

/// The window's two views. A cleanup, its total and its undo belong to one tab; the window
/// never runs memory and drive picks in the same action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tab {
    #[default]
    Memory,
    Disk,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Memory, Tab::Disk];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Memory => "Memory",
            Tab::Disk => "Disk space",
        }
    }
    /// The footer's primary button.
    pub fn button(self) -> &'static str {
        match self {
            Tab::Memory => "FREE MEMORY",
            Tab::Disk => "FREE DISK SPACE",
        }
    }
    /// How ui-state.toml remembers it.
    pub fn key(self) -> &'static str {
        match self {
            Tab::Memory => "memory",
            Tab::Disk => "disk",
        }
    }
    pub fn from_key(k: &str) -> Option<Tab> {
        Tab::ALL.into_iter().find(|t| t.key() == k)
    }
    fn index(self) -> usize {
        self as usize
    }
    fn next(self) -> Tab {
        match self {
            Tab::Memory => Tab::Disk,
            Tab::Disk => Tab::Memory,
        }
    }
}

/// Which tab a finding shows on, by where it came from: drive items of the disk checkup and
/// the THIS PC notices the drive side of the checkup adds (`drive_notice`: low free space,
/// Dev Drive health, caches on C:) go to Disk space; everything else is memory.
pub fn tab_of(f: &Finding) -> Tab {
    match f.kind {
        FindingKind::Disk => Tab::Disk,
        FindingKind::System if f.drive_notice => Tab::Disk,
        _ => Tab::Memory,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    App,
    Startup,
    Unreal,
    System,
    /// Addition: an item of the last disk checkup that can be cleaned.
    Disk,
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
    /// Addition, drive items: delete for good.
    DiskNow,
    /// Addition, drive items: move into Ather MemGuard's bin (undoable; frees nothing yet).
    DiskBin,
    /// Addition, drive items: the tool's own clean command (can't be undone).
    DiskTool,
}

/// Addition: what a drive row knows (from the last disk checkup).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskRow {
    /// "D:".
    pub volume: String,
    /// Predicted to free if deleted alone; `None` = not measured.
    pub frees_gb: Option<f64>,
    /// exact, lower_bound, estimate, upper_bound, unknown.
    pub confidence: String,
    /// The most it could free, for words only (never summed): "up to 23.7 GB" where the lower
    /// bound is about nothing because the clusters weren't counted in time.
    pub frees_max_gb: Option<f64>,
}

impl DiskRow {
    /// "16.4 GB", "at least 16.4 GB", "about 16.4 GB", "up to 23.7 GB", "an amount not counted
    /// in time".
    pub fn frees_words(&self) -> String {
        match self.frees_gb {
            None => "an amount not counted".into(),
            Some(g) if self.confidence == "lower_bound" && g < 0.05 => match self.frees_max_gb {
                Some(m) if m >= 0.05 => format!("up to {}", format_gb(m)),
                _ => format!("at least {}", format_gb(g)),
            },
            Some(g) => {
                let q = match self.confidence.as_str() {
                    "lower_bound" => "at least ",
                    "estimate" => "about ",
                    "upper_bound" => "at most ",
                    _ => "",
                };
                format!("{}{}", q, format_gb(g))
            }
        }
    }
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
    /// Addition: set on drive rows.
    pub disk: Option<DiskRow>,
    /// Addition: a THIS PC notice from the drive side of the checkup (`doctor::is_drive_finding`):
    /// it shows on the Disk space tab.
    pub drive_notice: bool,
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
    /// The window only ever passes picks of one tab.
    fn apply(&self, picks: &[(String, ActionKind)], force: bool) -> Vec<Outcome>;
    /// Undo the last cleanup of `tab` only: Memory = the memory checkup's undo log, Disk = the
    /// drive cleanup's bin moves.
    fn undo_last(&self, tab: Tab) -> Vec<Outcome>;
    fn can_undo(&self, tab: Tab) -> bool;
    /// Addition: run the disk checkup (it counts in its own low-priority process and deletes
    /// nothing); the next `scan` has its drive rows. `Err` says why it didn't run.
    fn disk_checkup(&self) -> Result<(), String> {
        Err("The disk checkup can't run from here.".into())
    }
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
        if a.kind == ActionKind::DiskTool {
            let label = if a.detail.is_empty() { "Clean with the tool".to_string() } else { a.detail.clone() };
            out.push(Choice { label, picks: vec![ActionKind::DiskTool] });
            continue;
        }
        let (label, picks): (&str, Vec<ActionKind>) = match (f.kind, f.verdict, a.kind) {
            (_, _, ActionKind::DiskNow) => ("Delete it", vec![ActionKind::DiskNow]),
            (_, _, ActionKind::DiskBin) => ("Move to the bin", vec![ActionKind::DiskBin]),
            (_, _, ActionKind::DiskTool) => ("Clean with the tool", vec![ActionKind::DiskTool]),
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
    /// Addition: what can go from the drives (from the last disk checkup).
    Disk,
    ThisPc,
}

impl SectionKind {
    pub const ORDER: [SectionKind; 8] = [
        SectionKind::NotNeeded,
        SectionKind::Startup,
        SectionKind::BigTools,
        SectionKind::WorthALook,
        SectionKind::HeldBack,
        SectionKind::Unreal,
        SectionKind::Disk,
        SectionKind::ThisPc,
    ];
    pub fn title(self) -> &'static str {
        match self {
            SectionKind::NotNeeded => "NOT NEEDED FOR UNREAL",
            SectionKind::Startup => "STARTS WITH WINDOWS",
            SectionKind::BigTools => "BIG TOOLS YOU MIGHT NOT BE USING",
            SectionKind::WorthALook => "WORTH A LOOK",
            SectionKind::HeldBack => "HELD BACK, NOT CLOSED",
            SectionKind::Unreal => "UNREAL",
            SectionKind::Disk => "ON YOUR DRIVES",
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
        (FindingKind::Disk, _) => Some(SectionKind::Disk),
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
    /// The header: the kind's title, or "ON D:" for a drive's items on the Disk space tab.
    pub title: String,
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
            // Deleting files is always the person's pick.
            Some(SectionKind::BigTools | SectionKind::WorthALook | SectionKind::Disk) | None => false,
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

    /// Is row `i` on `tab`?
    pub fn in_tab(&self, i: usize, tab: Tab) -> bool {
        tab_of(&self.report.findings[i]) == tab
    }

    /// Checked, selectable rows of `tab`.
    fn checked_in(&self, tab: Tab) -> Vec<usize> {
        (0..self.rows.len()).filter(|&i| self.rows[i].checked && self.selectable(i) && self.in_tab(i, tab)).collect()
    }

    /// What FREE MEMORY / FREE DISK SPACE would run on `tab`: (finding id, action) for every
    /// checked row of that tab only.
    pub fn picks(&self, tab: Tab) -> Vec<(String, ActionKind)> {
        let mut out = Vec::new();
        for i in self.checked_in(tab) {
            if let Some(c) = self.chosen(i) {
                for k in c.picks {
                    out.push((self.report.findings[i].id.clone(), k));
                }
            }
        }
        out
    }

    /// "Selected: frees about {x} GB of RAM": the RAM of checked Memory rows whose action
    /// closes or ends something now. Switching off a startup entry or holding an app back
    /// frees nothing now.
    pub fn selected_gb(&self) -> f64 {
        self.checked_in(Tab::Memory)
            .into_iter()
            .filter(|&i| self.chosen(i).map(|c| c.picks.iter().any(|k| matches!(k, ActionKind::Close | ActionKind::End))).unwrap_or(false))
            .map(|i| self.report.findings[i].ram_mb as f64 / 1024.0)
            .fold(0.0, |a, b| a + b) // not sum(): an empty f64 sum is -0.0
    }

    /// The checked drive rows' picks: (the chosen action, what the row knows).
    pub fn disk_selection(&self) -> Vec<(ActionKind, DiskRow)> {
        self.disk_rows()
            .into_iter()
            .filter_map(|i| Some((*self.chosen(i)?.picks.first()?, self.report.findings[i].disk.clone().unwrap_or_default())))
            .collect()
    }

    pub fn any_checked(&self, tab: Tab) -> bool {
        !self.picks(tab).is_empty()
    }

    /// Can anything on `tab` be checked?
    pub fn has_selectable(&self, tab: Tab) -> bool {
        (0..self.rows.len()).any(|i| self.selectable(i) && self.in_tab(i, tab))
    }

    /// Checked rows whose pick ends processes ("End it", "End them"; Memory rows only). FREE
    /// MEMORY asks before running these: anything unsaved in them is lost.
    pub fn ending_rows(&self) -> Vec<usize> {
        self.checked_in(Tab::Memory).into_iter().filter(|&i| self.chosen(i).is_some_and(|c| c.picks.contains(&ActionKind::End))).collect()
    }

    /// Checked drive rows. FREE DISK SPACE lists them (what goes, how, the predicted frees) and
    /// asks before anything runs.
    pub fn disk_rows(&self) -> Vec<usize> {
        self.checked_in(Tab::Disk).into_iter().filter(|&i| self.report.findings[i].kind == FindingKind::Disk).collect()
    }

    /// Mark the checked rows of `tab` as running and return their picks. The other tab's
    /// checkboxes stay as they are.
    pub fn begin_apply(&mut self, tab: Tab) -> Vec<(String, ActionKind)> {
        let picks = self.picks(tab);
        for i in self.checked_in(tab) {
            self.rows[i].busy = true;
            self.rows[i].checked = false;
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

    /// A fresh scan for `tab` only: that tab's rows are replaced (new checkboxes, no results),
    /// the other tab keeps its rows, checks and results as they were.
    pub fn replace_tab(&mut self, tab: Tab, fresh: Report) {
        let fresh = DoctorModel::new(fresh);
        let mut findings = Vec::new();
        let mut rows = Vec::new();
        for (f, r) in self.report.findings.drain(..).zip(self.rows.drain(..)) {
            if tab_of(&f) != tab {
                findings.push(f);
                rows.push(r);
            }
        }
        let mut fr = fresh.report;
        for (f, r) in fr.findings.drain(..).zip(fresh.rows) {
            if tab_of(&f) == tab {
                findings.push(f);
                rows.push(r);
            }
        }
        match tab {
            Tab::Memory => {
                self.report.time = fr.time;
                self.report.system = fr.system;
                self.report.reclaimable_ram_gb = fr.reclaimable_ram_gb;
                self.report.reclaimable_commit_gb = fr.reclaimable_commit_gb;
            }
            Tab::Disk => self.report.disk_checked = fr.disk_checked,
        }
        self.report.findings = findings;
        self.rows = rows;
    }

    /// True when the app in row `i` stayed open after a close.
    pub fn still_open(&self, i: usize) -> bool {
        self.rows[i].outcomes.iter().any(|o| o.status == OutcomeStatus::StillOpen)
    }

    /// The sections of `tab`, in order, empty ones hidden. Memory: the memory sections, then
    /// its THIS PC notices. Disk space: one section per drive ("ON D:", drives in letter
    /// order), then the drive notices under THIS PC.
    pub fn sections(&self, tab: Tab) -> Vec<Section> {
        let f = &self.report.findings;
        let in_section = |i: usize, kind: SectionKind| tab_of(&f[i]) == tab && section_of(&f[i]) == Some(kind);
        let mut out = Vec::new();
        for kind in SectionKind::ORDER {
            if kind == SectionKind::Disk {
                let mut vols: Vec<String> = (0..f.len()).filter(|&i| in_section(i, kind)).map(|i| volume_of(&f[i])).collect();
                vols.sort();
                vols.dedup();
                for v in vols {
                    let rows: Vec<usize> = (0..f.len()).filter(|&i| in_section(i, kind) && volume_of(&f[i]) == v).collect();
                    let title = if v.is_empty() { kind.title().to_string() } else { format!("ON {}", v) };
                    out.push(Section { kind, title, rows, ram_mb: 0 });
                }
                continue;
            }
            let rows: Vec<usize> = (0..f.len()).filter(|&i| in_section(i, kind)).collect();
            if rows.is_empty() {
                continue;
            }
            let ram_mb = if kind == SectionKind::ThisPc { 0 } else { rows.iter().map(|&i| f[i].ram_mb).sum() };
            out.push(Section { kind, title: kind.title().to_string(), rows, ram_mb });
        }
        out
    }

    /// How many rows `tab` shows (its title's count).
    pub fn count(&self, tab: Tab) -> usize {
        self.sections(tab).iter().map(|s| s.rows.len()).sum()
    }

    /// What `tab`'s title counts: its rows you can act on (a notice isn't counted), so the
    /// Disk space count is the hero's "N ITEMS COULD GO".
    pub fn actionable(&self, tab: Tab) -> usize {
        self.sections(tab).iter().flat_map(|s| s.rows.iter()).filter(|&&i| !choices(&self.report.findings[i]).is_empty()).count()
    }

    /// Nothing proposed on `tab`: no finding with an action there.
    pub fn all_clear(&self, tab: Tab) -> bool {
        self.report.findings.iter().filter(|f| tab_of(f) == tab).all(|f| section_of(f).is_none() || choices(f).is_empty())
    }
}

/// The drive a drive row is on ("D:"), "" when unknown.
fn volume_of(f: &Finding) -> String {
    f.disk.as_ref().map(|d| d.volume.trim().to_ascii_uppercase()).unwrap_or_default()
}

/// The Memory tab's footer.
pub fn memory_footer_words(gb: f64) -> String {
    format!("Selected: frees about {:.1} GB of RAM", gb)
}

/// The Disk space tab's footer for the checked drive rows: what deleting (or a tool's clean)
/// is predicted to free, per drive ("Selected: frees about 16.8 GB on D: and about 3.1 GB on
/// C: (predicted)"), and how many go to the bin, which frees nothing until it's emptied.
pub fn disk_footer_words(sel: &[(ActionKind, DiskRow)]) -> String {
    if sel.is_empty() {
        return "Selected: nothing yet".into();
    }
    // (drive, known GB, some unknown, some only a lower bound, the most it could free)
    let mut vols: Vec<(String, f64, bool, bool, f64)> = Vec::new();
    for (_, row) in sel.iter().filter(|(k, _)| *k != ActionKind::DiskBin) {
        let v = row.volume.trim().to_ascii_uppercase();
        let v = if v.is_empty() { "your drives".to_string() } else { v };
        let at = match vols.iter().position(|e| e.0 == v) {
            Some(at) => at,
            None => {
                vols.push((v, 0.0, false, false, 0.0));
                vols.len() - 1
            }
        };
        match row.frees_gb {
            Some(g) => {
                vols[at].1 += g;
                vols[at].4 += row.frees_max_gb.unwrap_or(g).max(g);
                vols[at].3 |= row.confidence == "lower_bound";
            }
            None => vols[at].2 = true,
        }
    }
    vols.sort_by(|a, b| a.0.cmp(&b.0));
    let parts: Vec<String> = vols
        .iter()
        .map(|(v, gb, unknown, lower, most)| match (*gb >= 0.05, *unknown, *lower) {
            // Only sized on a Dev Drive: the lower bound is about nothing, the ceiling is real.
            (false, false, true) if *most >= 0.05 => format!("up to {} on {}", format_gb(*most), v),
            (_, false, true) => format!("at least {} on {}", format_gb(*gb), v),
            (_, false, false) => format!("about {} on {}", format_gb(*gb), v),
            (true, true, _) => format!("at least {} on {}", format_gb(*gb), v),
            (false, true, _) => format!("an amount not counted on {}", v),
        })
        .collect();
    let bins = sel.iter().filter(|(k, _)| *k == ActionKind::DiskBin).count();
    let items = |n: usize| if n == 1 { "1 item".to_string() } else { format!("{} items", n) };
    match (parts.is_empty(), bins) {
        (false, 0) => format!("Selected: frees {} (predicted)", parts.join(" and ")),
        (false, n) => format!("Selected: frees {} (predicted) · {} to the bin", parts.join(" and "), items(n)),
        (true, n) => format!("Selected: {} to the bin (frees nothing until it's emptied)", items(n)),
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
                (_, ActionKind::DiskNow) => "Deleted".into(),
                (_, ActionKind::DiskBin) => "Moved to the bin".into(),
                (_, ActionKind::DiskTool) => "Cleaned".into(),
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
    /// A cleanup ran on [Memory, Disk] (and can be undone there).
    applied: Mutex<[bool; 2]>,
    /// What is left to free after the last `apply`.
    after_apply: Mutex<Option<f64>>,
    /// A disk checkup has run (the drive rows exist).
    disk_checked: Mutex<bool>,
}

impl FakeDoctor {
    pub fn new() -> FakeDoctor {
        FakeDoctor { clear: false, delay: Duration::ZERO, applied: Mutex::new([false; 2]), after_apply: Mutex::new(None), disk_checked: Mutex::new(true) }
    }
    pub fn all_clear() -> FakeDoctor {
        FakeDoctor { clear: true, ..FakeDoctor::new() }
    }
    /// No disk checkup has run yet: no drive rows until `disk_checkup`.
    pub fn without_disk_checkup() -> FakeDoctor {
        FakeDoctor { disk_checked: Mutex::new(false), ..FakeDoctor::new() }
    }
    pub fn with_delay(mut self, d: Duration) -> FakeDoctor {
        self.delay = d;
        self
    }

    pub fn report(clear: bool) -> Report {
        FakeDoctor::report_with(clear, true)
    }

    /// The sample report; `disk_checked` false leaves out the drive rows (no disk checkup yet).
    pub fn report_with(clear: bool, disk_checked: bool) -> Report {
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
            disk: None,
            drive_notice: false,
        };
        use ActionKind as A;
        use FindingKind as K;
        use Verdict as V;
        let system = SystemStatus { ram_total_gb: 127.7, ram_free_gb: 45.1, commit_limit_gb: 144.0, commit_free_gb: 27.6, memguard_running: true, memguard_paused: false };
        let disk_time = disk_checked.then(|| "2026-11-14 13:40".to_string());
        if clear {
            return Report { time: "2026-11-14 14:02:11".into(), system, findings: Vec::new(), reclaimable_ram_gb: 0.0, reclaimable_commit_gb: 0.0, disk_checked: disk_time };
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
        // The drive side's notices (THIS PC on the Disk space tab), there with or without a
        // disk checkup.
        let mut low = f("disk:low:d", K::System, V::Review, "Your Dev Drive is almost full", "D: has 38 GB free, and a cook usually needs 100 GB.", 0, 0, &[], false);
        low.drive_notice = true;
        let mut perf = f("dev:perf_mode_off", K::System, V::Review, "Defender's performance mode is off", "Files on your Dev Drive are scanned before they open. Fix, as administrator: Set-MpPreference -PerformanceModeStatus Enabled", 0, 0, &[], false);
        perf.drive_notice = true;
        findings.push(low);
        findings.push(perf);
        if disk_checked {
            let mut build = f("disk:d1", K::Disk, V::Review, "Build files for S2", r"D:\s2\Intermediate", 0, 0, &[A::DiskNow, A::DiskBin], false);
            build.disk = Some(DiskRow { volume: "D:".into(), frees_gb: Some(16.4), confidence: "exact".into(), frees_max_gb: None });
            let mut logs = f("disk:d2", K::Disk, V::Review, "Logs for S2", r"D:\s2\Saved\Logs", 0, 0, &[A::DiskBin, A::DiskNow], false);
            logs.disk = Some(DiskRow { volume: "D:".into(), frees_gb: Some(0.4), confidence: "exact".into(), frees_max_gb: None });
            let mut npm = f("disk:d3", K::Disk, V::Review, "npm cache", r"C:\Users\you\AppData\Local\npm-cache\_cacache", 0, 0, &[A::DiskNow, A::DiskBin, A::DiskTool], false);
            npm.actions[2].detail = "Clean with npm".into();
            npm.disk = Some(DiskRow { volume: "C:".into(), frees_gb: Some(3.1), confidence: "estimate".into(), frees_max_gb: None });
            findings.insert(13, npm);
            findings.insert(13, logs);
            findings.insert(13, build);
        }
        let reclaim: u64 = findings.iter().filter(|f| f.kind == K::App || f.kind == K::Unreal).filter(|f| f.verdict != V::HoldBack).map(|f| f.ram_mb).sum();
        Report {
            time: "2026-11-14 14:02:11".into(),
            system,
            findings,
            reclaimable_ram_gb: reclaim as f64 / 1024.0,
            reclaimable_commit_gb: reclaim as f64 * 1.25 / 1024.0,
            disk_checked: disk_time,
        }
    }

    fn outcome(id: &str, action: ActionKind, force: bool) -> Outcome {
        let (status, message) = match (id, action) {
            ("app:spotify", ActionKind::Close) if !force => (OutcomeStatus::StillOpen, String::new()),
            ("app:discord", ActionKind::HoldBack) => (OutcomeStatus::Done, "Added to Chat".to_string()),
            ("app:dropbox", ActionKind::HoldBack) => (OutcomeStatus::Done, "Added to Background".to_string()),
            ("ue:orphans", ActionKind::End) => (OutcomeStatus::Done, "Ended 3 helpers".to_string()),
            (_, ActionKind::DiskNow) => (OutcomeStatus::Done, "Deleted · frees 16.4 GB (predicted); measured later".to_string()),
            (_, ActionKind::DiskBin) => (OutcomeStatus::Done, "Moved to the bin · frees nothing until it's emptied".to_string()),
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
        let mut r = FakeDoctor::report_with(self.clear, *self.disk_checked.lock().unwrap());
        // After a cleanup what it acted on is gone from the next scan of that tab.
        r.findings.retain(|f| !applied[tab_of(f).index()] || f.kind == FindingKind::System);
        r
    }
    fn apply(&self, picks: &[(String, ActionKind)], force: bool) -> Vec<Outcome> {
        std::thread::sleep(self.delay);
        let r = FakeDoctor::report_with(self.clear, *self.disk_checked.lock().unwrap());
        {
            let mut applied = self.applied.lock().unwrap();
            for (id, _) in picks {
                if let Some(f) = r.findings.iter().find(|f| f.id == *id) {
                    applied[tab_of(f).index()] = true;
                }
            }
        }
        let outs: Vec<Outcome> = picks.iter().map(|(id, k)| FakeDoctor::outcome(id, *k, force)).collect();
        // Like the real one: what closed or ended no longer counts as "could be freed".
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
    fn undo_last(&self, tab: Tab) -> Vec<Outcome> {
        std::thread::sleep(self.delay);
        self.applied.lock().unwrap()[tab.index()] = false;
        Vec::new()
    }
    fn can_undo(&self, tab: Tab) -> bool {
        self.applied.lock().unwrap()[tab.index()]
    }
    fn reclaimable_after_apply(&self) -> Option<f64> {
        self.after_apply.lock().unwrap().take()
    }
    fn disk_checkup(&self) -> Result<(), String> {
        std::thread::sleep(self.delay);
        *self.disk_checked.lock().unwrap() = true;
        Ok(())
    }
}

// ---------------------------------------------------------------- the window

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// The tab strip: show this tab.
    Tab(Tab),
    Check(usize),
    Choice(usize),
    EndAnyway(usize),
    HowToFix(usize),
    CheckAgain,
    /// Run the disk checkup (the Disk space tab's empty state, "Count again").
    DiskCheckup,
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
    /// One tab of the strip: its title, the count of its rows, whether it is showing.
    Tab { label: String, count: Option<usize>, active: bool },
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
    /// "Checking…" (or what is being counted) over the indeterminate bar.
    Checking(String),
    Message(Vec<String>),
    /// Small left-aligned lines outside the rows (the Disk space tab's checkup line, errors).
    Info { lines: Vec<String>, color: Rgb },
    Button(String),
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

/// Can each tab undo its last cleanup: [Memory, Disk].
type Undoable = [bool; 2];

fn undoable(s: &dyn DoctorSource) -> Undoable {
    [s.can_undo(Tab::Memory), s.can_undo(Tab::Disk)]
}

enum Job {
    /// A scan: of everything (`None`) or for one tab, whose rows it replaces.
    Scanned(Report, Undoable, Option<Tab>),
    Applied(Vec<Outcome>, Undoable, Option<f64>),
    /// The disk checkup ran (or why not), then a scan for the Disk space tab.
    DiskChecked(Result<(), String>, Report, Undoable),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    /// "End it anyway" on a row that stayed open.
    End(usize),
    /// FREE MEMORY with rows that end processes checked: these rows, before anything runs.
    CleanUp(Vec<usize>),
    /// FREE DISK SPACE: the checked drive rows, what goes, how, and the predicted frees.
    Disk(Vec<usize>),
}

/// One drive row in the confirmation: (title, the chosen action, what it frees).
pub type DiskPick<'a> = (&'a str, ActionKind, &'a DiskRow, &'a str);

/// The drive confirmation's words: (title, body, OK button). Lists every item under what
/// happens to it, with the predicted frees; says what can't be undone and that the space is
/// measured afterwards. Never adds the items' numbers up (they can overlap).
pub fn disk_confirm_words(picks: &[DiskPick]) -> (String, String, String) {
    let mut lines = Vec::new();
    for (title, kind, row, tool_label) in picks {
        lines.push(match kind {
            ActionKind::DiskBin => format!("Move to the bin: {} (frees nothing until the bin is emptied)", title),
            ActionKind::DiskTool => format!("{}: {} (frees {} predicted; can't be undone)", tool_label, title, row.frees_words()),
            _ => format!("Delete for good: {} on {} (frees {} predicted)", title, row.volume, row.frees_words()),
        });
    }
    let permanent = picks.iter().any(|p| p.1 != ActionKind::DiskBin);
    lines.push(if permanent {
        "Deleting can't be undone. The space is measured afterwards and you'll get a notification.".to_string()
    } else {
        "Undo with \"Undo last cleanup\". The bin empties itself after a day.".to_string()
    });
    let n = picks.len();
    let title = if n == 1 { "Clean up 1 item on your drives?".to_string() } else { format!("Clean up {} items on your drives?", n) };
    (title, lines.join("\n"), if permanent { "Delete".into() } else { "Move to the bin".into() })
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

/// The Disk space tab's words before any disk checkup ran.
pub const NO_DISK_CHECKUP: &str = "No disk checkup yet. It finds build files, caches and logs on your drives and what removing each would really free. It counts in the background for up to a minute and deletes nothing.";

pub struct DoctorWindow {
    source: Arc<Mutex<Box<dyn DoctorSource>>>,
    pub model: Option<DoctorModel>,
    pub confirm: Option<Confirm>,
    /// The tab showing.
    tab: Tab,
    scanning: bool,
    /// What the running scan is for: everything (`None`) or one tab.
    scan_scope: Option<Tab>,
    /// The disk checkup is counting.
    counting: bool,
    /// Why the last disk checkup from this window didn't run.
    disk_error: Option<String>,
    working: bool,
    can_undo: Undoable,
    rx: Option<Receiver<Job>>,
    hwnd: isize,
    started: bool,
    scale: Scale,
    size: (i32, i32),
    mouse: Option<Pt>,
    pressed: Option<Act>,
    focus: Option<Act>,
    scroll: Scroll,
    /// Each tab's scroll position while the other one shows.
    tab_scroll: [i32; 2],
    anim: f32,
    /// The header (hero and tabs, fixed) first, then the scrolling content.
    items: Vec<Item>,
    /// How many of `items` are the header's.
    head_n: usize,
    /// The header's area (above `scroll.view`).
    head: Rect,
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
            tab: Tab::Memory,
            scanning: false,
            scan_scope: None,
            counting: false,
            disk_error: None,
            working: false,
            can_undo: [false; 2],
            rx: None,
            hwnd: 0,
            started: false,
            scale: Scale(1.0),
            size: (760, 720),
            mouse: None,
            pressed: None,
            focus: None,
            scroll: Scroll::default(),
            tab_scroll: [0; 2],
            anim: 0.3,
            items: Vec::new(),
            head_n: 0,
            head: Rect::default(),
            menu_row: None,
        }
    }

    /// Send commands from rows ("Show me how") to `commands`.
    pub fn with_commands(mut self, commands: Box<dyn Fn(Command)>) -> DoctorWindow {
        self.commands = commands;
        self
    }

    /// Open on `tab`.
    pub fn with_tab(mut self, tab: Tab) -> DoctorWindow {
        self.tab = tab;
        self
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn is_scanning(&self) -> bool {
        self.scanning
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// Show `tab`. Each tab keeps its own checks, results and scroll position. An open
    /// confirmation is cancelled (nothing has run yet).
    pub fn set_tab(&mut self, tab: Tab) {
        if tab == self.tab {
            return;
        }
        self.tab_scroll[self.tab.index()] = self.scroll.y;
        self.tab = tab;
        self.scroll.y = self.tab_scroll[tab.index()];
        self.confirm = None;
        self.menu_row = None;
        if !matches!(self.focus, Some(Act::Tab(_))) {
            self.focus = None;
        }
        if self.hwnd != 0 {
            let _ = super::ui_state::save_str(KIND, "tab", tab.key());
        }
    }

    /// Is the showing tab waiting for a scan (or the disk checkup)?
    fn tab_busy(&self) -> bool {
        self.model.is_none() || (self.scanning && self.scan_scope.is_none_or(|t| t == self.tab))
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

    /// Scan everything. Results replace everything shown.
    pub fn scan(&mut self, host: Option<&mut Host>) {
        self.scan_for(None, host);
    }

    /// Scan (again) for one tab: its rows are replaced, the other tab's stay as they are.
    fn scan_for(&mut self, scope: Option<Tab>, host: Option<&mut Host>) {
        self.scanning = true;
        self.scan_scope = scope;
        self.working = true;
        self.confirm = None;
        self.run(move |s| Job::Scanned(s.scan(), undoable(s), scope), host);
    }

    /// Run the disk checkup (its own process; deletes nothing), then show its rows.
    pub fn disk_checkup(&mut self, host: Option<&mut Host>) {
        if self.working {
            return;
        }
        self.scanning = true;
        self.scan_scope = Some(Tab::Disk);
        self.counting = true;
        self.working = true;
        self.disk_error = None;
        self.confirm = None;
        self.run(
            |s| {
                let r = s.disk_checkup();
                Job::DiskChecked(r, s.scan(), undoable(s))
            },
            host,
        );
    }

    /// FREE MEMORY / FREE DISK SPACE: run the checked rows of the showing tab, never the other
    /// tab's. Drive rows always ask first (what goes, how, the predicted frees). Ending
    /// processes always needs a second, explicit confirmation (docs/DESIGN.md), so when a
    /// checked row's pick is "End it" or "End them" this only opens the in-window
    /// confirmation listing them; its "End them" runs everything (`confirm_ok`), Cancel runs
    /// nothing.
    pub fn clean_up(&mut self, host: Option<&mut Host>) {
        let tab = self.tab;
        let Some(m) = self.model.as_ref() else { return };
        if self.working || !m.any_checked(tab) {
            return;
        }
        match tab {
            Tab::Disk => {
                let disk = m.disk_rows();
                if !disk.is_empty() {
                    self.confirm = Some(Confirm::Disk(disk));
                }
            }
            Tab::Memory => {
                let ending = m.ending_rows();
                if ending.is_empty() {
                    self.run_clean_up(Tab::Memory, host);
                } else {
                    self.confirm = Some(Confirm::CleanUp(ending));
                }
            }
        }
    }

    /// Run every checked row's picks of `tab` now (after any confirmation).
    fn run_clean_up(&mut self, tab: Tab, host: Option<&mut Host>) {
        let Some(m) = self.model.as_mut() else { return };
        if self.working || !m.any_checked(tab) {
            return;
        }
        let picks = m.begin_apply(tab);
        let force = picks.iter().any(|(_, k)| *k == ActionKind::End);
        self.working = true;
        self.run(
            move |s| {
                let outs = s.apply(&picks, force);
                Job::Applied(outs, undoable(s), s.reclaimable_after_apply())
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
                Job::Applied(outs, undoable(s), s.reclaimable_after_apply())
            },
            host,
        );
    }

    /// "Undo last cleanup": the showing tab's last cleanup only, then a scan for that tab.
    pub fn undo(&mut self, host: Option<&mut Host>) {
        let tab = self.tab;
        if !self.can_undo[tab.index()] || self.working {
            return;
        }
        self.working = true;
        self.scanning = true;
        self.scan_scope = Some(tab);
        self.run(
            move |s| {
                s.undo_last(tab);
                Job::Scanned(s.scan(), undoable(s), Some(tab))
            },
            host,
        );
    }

    fn on_scanned(&mut self, r: Report, scope: Option<Tab>) {
        match (scope, self.model.as_mut()) {
            (Some(t), Some(m)) => m.replace_tab(t, r),
            _ => self.model = Some(DoctorModel::new(r)),
        }
        if scope.is_none_or(|t| t == self.tab) {
            self.scroll.y = 0;
            self.focus = None;
        }
        for t in Tab::ALL {
            if scope.is_none_or(|s| s == t) && t != self.tab {
                self.tab_scroll[t.index()] = 0;
            }
        }
        self.scanning = false;
        self.scan_scope = None;
        self.counting = false;
        self.working = false;
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::Scanned(r, can_undo, scope) => {
                self.can_undo = can_undo;
                self.on_scanned(r, scope);
            }
            Job::DiskChecked(res, r, can_undo) => {
                self.can_undo = can_undo;
                self.disk_error = res.err();
                self.on_scanned(r, Some(Tab::Disk));
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

    /// The hero's right side for the showing tab: (big words, caption).
    fn hero_result(&self, m: &DoctorModel) -> Option<(String, Option<&'static str>)> {
        match self.tab {
            Tab::Memory if m.all_clear(Tab::Memory) => Some(("ALL CLEAR".into(), None)),
            Tab::Memory => Some((format!("{:.1} GB", m.report.reclaimable_ram_gb), Some("COULD BE FREED"))),
            Tab::Disk if m.report.disk_checked.is_none() => None,
            Tab::Disk => {
                let n = m.sections(Tab::Disk).iter().filter(|s| s.kind == SectionKind::Disk).map(|s| s.rows.iter().filter(|&&i| !choices(&m.report.findings[i]).is_empty()).count()).sum::<usize>();
                match n {
                    0 => Some(("ALL CLEAR".into(), None)),
                    1 => Some(("1 ITEM".into(), Some("COULD GO FROM YOUR DRIVES"))),
                    n => Some((format!("{} ITEMS", n), Some("COULD GO FROM YOUR DRIVES"))),
                }
            }
        }
    }

    /// Lay the page out; fills `self.items`: the fixed header (hero and tab strip), then the
    /// showing tab's content, which scrolls under it.
    pub fn layout(&mut self, c: &Canvas) {
        let mut items = Vec::new();
        let w = c.width();
        let fh = Self::footer_h(c);
        let content_w = (w - 2 * c.s(32.0)).min(c.s(1040.0));
        let left = (w - content_w) / 2;
        let right = left + content_w;
        let mut y = c.s(24.0);

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
        if !self.tab_busy() {
            if let Some(m) = &self.model {
                let res = self.hero_result(m);
                let rh = c.line_height(F_RESULT);
                let ch = c.line_height(F_CAPTION);
                let lh = c.line_height(F_HINT);
                let block = res.as_ref().map_or(0, |(_, cap)| rh + cap.map_or(0, |_| ch + c.s(2.0)) + c.s(6.0)) + lh;
                let by = y + (ls - block) / 2;
                let mut yy = by;
                if let Some((big, cap)) = res {
                    let rw = c.measure(&big, F_RESULT).0 + c.s(4.0);
                    items.push(Item::new(Rect::new(right - rw, by, rw, rh), Paint::Result(big)));
                    yy += rh;
                    if let Some(cap) = cap {
                        let cw = c.measure_spaced(cap, F_CAPTION, c.s(1.3)).0 + c.s(2.0);
                        items.push(Item::new(Rect::new(right - cw, yy + c.s(2.0), cw, ch), Paint::Caption(cap.into())));
                        yy += ch + c.s(2.0);
                    }
                    yy += c.s(6.0);
                }
                let lw = c.measure("Check again", F_HINT).0 + c.s(2.0);
                if !self.working {
                    items.push(Item::new(Rect::new(right - lw, yy, lw, lh), Paint::Link { label: "Check again".into(), color: MUTED }).act(Act::CheckAgain));
                }
                hero_bottom = hero_bottom.max(yy + lh);
            }
        }
        y = hero_bottom + c.s(14.0);

        // The tab strip.
        let tab_h = c.s(38.0);
        let mut x = left;
        for t in Tab::ALL {
            let count = self.model.as_ref().filter(|_| !self.scanning || self.scan_scope.is_some_and(|s| s != t)).map(|m| m.actionable(t)).filter(|n| *n > 0);
            let tw = tab_width(c, t.title(), count);
            items.push(Item::new(Rect::new(x, y, tw, tab_h), Paint::Tab { label: t.title().into(), count, active: t == self.tab }).act(Act::Tab(t)));
            x += tw + c.s(8.0);
        }
        y += tab_h;
        items.push(Item::new(Rect::new(left, y, content_w, c.px1(1.0)), Paint::Hairline(BORDER)));
        y += c.px1(1.0);
        let head_n = items.len();
        self.head = Rect::new(0, 0, w, y);
        self.scroll.view = Rect::new(0, y, w, (c.height() - fh - y).max(0));

        // The showing tab, scrolled.
        let top = y - self.scroll.y;
        let mut y = top + c.s(4.0);
        if self.tab_busy() {
            y += c.s(28.0);
            let what = if self.counting && self.tab == Tab::Disk { "Counting what's on your drives… (up to a minute)" } else { "Checking…" };
            items.push(Item::new(Rect::new(left, y, content_w, c.s(40.0)), Paint::Checking(what.into())));
            y += c.s(40.0);
        } else if let Some(m) = self.model.clone() {
            let sections = m.sections(self.tab);
            let msg_w = content_w.min(c.s(600.0));
            let body_lh = c.line_height(F_BODY) + c.s(2.0);
            let message = |items: &mut Vec<Item>, y: &mut i32, text: &str| {
                let lines = c.wrap_lines(text, F_BODY, msg_w, 4);
                let h = lines.len() as i32 * body_lh;
                items.push(Item::new(Rect::new(left, *y, msg_w, h), Paint::Message(lines)));
                *y += h;
            };
            match self.tab {
                Tab::Memory if sections.is_empty() => {
                    y += c.s(28.0);
                    message(&mut items, &mut y, "Nothing here needs your attention. Every app that's running is one Unreal work needs, or too small to matter.");
                }
                Tab::Memory => {}
                Tab::Disk => {
                    y += c.s(20.0);
                    match &m.report.disk_checked {
                        None => {
                            message(&mut items, &mut y, NO_DISK_CHECKUP);
                            y += c.s(14.0);
                            let b = Button::new("Run the disk checkup", ButtonStyle::Normal);
                            let (bw, bh) = b.size(c);
                            items.push(Item::new(Rect::new(left, y, bw, bh), Paint::Button(b.label.clone())).act(Act::DiskCheckup));
                            y += bh;
                        }
                        Some(time) => {
                            let line = format!("From the disk checkup of {}.", time);
                            let lh = c.line_height(F_HINT);
                            let lw = (c.measure(&line, F_HINT).0 + c.s(2.0)).min(content_w - c.s(120.0));
                            items.push(Item::new(Rect::new(left, y, lw, lh), Paint::Info { lines: vec![line], color: MUTED }));
                            let cw = c.measure("Count again", F_HINT).0 + c.s(2.0);
                            if !self.working {
                                items.push(Item::new(Rect::new(left + lw + c.s(10.0), y, cw, lh), Paint::Link { label: "Count again".into(), color: MUTED }).act(Act::DiskCheckup));
                            }
                            y += lh;
                            if !sections.iter().any(|s| s.kind == SectionKind::Disk) {
                                y += c.s(14.0);
                                message(&mut items, &mut y, "Nothing on your drives needs your attention: the last disk checkup found nothing that can go.");
                            }
                        }
                    }
                    if let Some(e) = &self.disk_error {
                        y += c.s(10.0);
                        let lines = wrap_hard(c, &format!("The disk checkup didn't run: {}", e), F_HINT, msg_w, 3);
                        let h = lines.len() as i32 * c.line_height(F_HINT);
                        items.push(Item::new(Rect::new(left, y, msg_w, h), Paint::Info { lines, color: ERROR }));
                        y += h;
                    }
                }
            }
            for s in &sections {
                let lh = c.line_height(F_SECTION);
                let sh = c.s(26.0) + lh + c.s(8.0);
                let gb = if s.ram_mb > 0 { format_gb(s.ram_mb as f64 / 1024.0) } else { String::new() };
                let gw = if gb.is_empty() { 0 } else { c.measure(&gb, F_LABEL).0 + c.s(2.0) };
                items.push(Item::new(Rect::new(left, y + c.s(26.0), content_w - gw - c.s(16.0), lh), Paint::Section(s.title.clone())));
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
        self.scroll.content = y - top;
        self.scroll.clamp();
        self.items = items;
        self.head_n = head_n;
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
            // The section header names the drive.
            SectionKind::Disk => (f.disk.as_ref().map(|d| format!("frees {} (predicted)", d.frees_words())).unwrap_or_default(), f.detail.clone()),
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

    fn undo_shown(&self) -> bool {
        self.can_undo[self.tab.index()] && !self.working
    }

    /// Footer pieces: (text rect, undo link rect, the tab's button rect).
    pub fn footer_layout(&self, c: &Canvas) -> (Rect, Option<Rect>, Rect) {
        let fh = Self::footer_h(c);
        let y = c.height() - fh;
        let w = c.width();
        let pad = c.s(32.0);
        let b = Button::new(self.tab.button(), ButtonStyle::Primary);
        let (bw, bh) = b.size(c);
        let bw = bw + c.s(16.0);
        let br = Rect::new(w - pad - bw, y + (fh - bh) / 2, bw, bh);
        let undo = if self.undo_shown() {
            let lw = c.measure("Undo last cleanup", F_HINT).0 + c.s(2.0);
            let lh = c.line_height(F_HINT);
            Some(Rect::new(br.x - c.s(20.0) - lw, y + (fh - lh) / 2, lw, lh))
        } else {
            None
        };
        let tr = Rect::new(pad, y, undo.map(|u| u.x).unwrap_or(br.x) - pad - c.s(16.0), fh);
        (tr, undo, br)
    }

    /// The showing tab's running total; empty while it is checking or has nothing to check.
    pub fn footer_text(&self) -> String {
        match &self.model {
            Some(m) if !self.tab_busy() && m.has_selectable(self.tab) => match self.tab {
                Tab::Memory => memory_footer_words(m.selected_gb()),
                Tab::Disk => disk_footer_words(&m.disk_selection()),
            },
            _ => String::new(),
        }
    }

    fn can_clean(&self) -> bool {
        !self.working && !self.tab_busy() && self.model.as_ref().map(|m| m.any_checked(self.tab)).unwrap_or(false)
    }

    // ------------------------------------------------------------ painting

    /// Is `p` where `r` shows: the header for header items, the scrolling view for the rest.
    fn shows_at(&self, r: Rect, p: Pt) -> bool {
        r.contains(p) && (self.scroll.view.contains(p) || (self.head.contains(p) && r.bottom() <= self.head.bottom()))
    }

    fn state_for(&self, act: Option<Act>, r: Rect) -> WState {
        let over = self.confirm.is_none() && act.is_some() && self.mouse.map(|m| self.shows_at(r, m)).unwrap_or(false);
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
            Paint::Tab { label, count, active } => paint_tab(c, r, label, *count, *active, &st),
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
            Paint::Info { lines, color } => {
                let hh = c.line_height(F_HINT);
                for (k, line) in lines.iter().enumerate() {
                    c.text_top(Rect::new(r.x, r.y + k as i32 * hh, r.w, hh), line, F_HINT, *color, Align::Left);
                }
            }
            Paint::Checking(what) => {
                let th = c.line_height(F_BODY);
                c.text(Rect::new(r.x, r.y, r.w, th), what, F_BODY, TEXT_DIM, Align::Left);
                indeterminate_bar(c, Rect::new(r.x, r.y + th + c.s(12.0), r.w, c.s(3.0).max(2)), self.anim);
            }
            Paint::Message(lines) => {
                let lh = c.line_height(F_BODY) + c.s(2.0);
                for (k, l) in lines.iter().enumerate() {
                    c.text(Rect::new(r.x, r.y + k as i32 * lh, r.w, lh), l, F_BODY, TEXT_DIM, Align::Left);
                }
            }
            Paint::Button(label) => {
                let mut st = st;
                st.disabled = self.working;
                Button::new(label, ButtonStyle::Normal).paint(c, r, &st);
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
        let label = self.tab.button();
        let mut st = self.state_for(Some(Act::CleanUp), br);
        if enabled {
            Button::new(label, ButtonStyle::Primary).paint(c, br, &st);
        } else {
            st.disabled = true;
            st.hover = false;
            Button::new(label, ButtonStyle::Normal).paint(c, br, &st);
        }
    }

    fn dialog(&self) -> Option<Dialog> {
        let m = self.model.as_ref()?;
        let (title, body, ok) = match self.confirm.as_ref()? {
            Confirm::Disk(rows) => {
                let none = DiskRow::default();
                let picks: Vec<(String, ActionKind, DiskRow, String)> = rows
                    .iter()
                    .filter_map(|&i| {
                        let f = m.report.findings.get(i)?;
                        let c = m.chosen(i)?;
                        Some((f.title.clone(), *c.picks.first()?, f.disk.clone().unwrap_or_else(|| none.clone()), c.label.clone()))
                    })
                    .collect();
                let refs: Vec<DiskPick> = picks.iter().map(|(t, k, r, l)| (t.as_str(), *k, r, l.as_str())).collect();
                disk_confirm_words(&refs)
            }
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
        let n = self.head_n.min(self.items.len());
        if self.head.contains(p) {
            return self.items[..n].iter().rev().find(|i| i.act.is_some() && i.rect.contains(p)).and_then(|i| i.act);
        }
        if !self.scroll.view.contains(p) {
            return None;
        }
        self.items[n..].iter().rev().find(|i| i.act.is_some() && i.rect.contains(p)).and_then(|i| i.act)
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
            Act::Tab(t) => self.set_tab(t),
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
                    self.scan_for(Some(self.tab), Some(host));
                }
            }
            Act::DiskCheckup => self.disk_checkup(Some(host)),
            Act::CleanUp => self.clean_up(Some(host)),
            Act::Undo => self.undo(Some(host)),
        }
        host.invalidate();
    }

    fn focusables(&self) -> Vec<Act> {
        let mut out: Vec<Act> = self.items.iter().filter_map(|i| i.act).collect();
        if self.undo_shown() {
            out.push(Act::Undo);
        }
        if self.can_clean() {
            out.push(Act::CleanUp);
        }
        out
    }

    fn confirm_ok(&mut self, host: &mut Host) {
        match self.confirm.take() {
            Some(Confirm::Disk(_)) => self.run_clean_up(Tab::Disk, Some(host)),
            Some(Confirm::End(i)) => self.end_anyway(i, Some(host)),
            Some(Confirm::CleanUp(_)) => self.run_clean_up(Tab::Memory, Some(host)),
            None => {}
        }
    }
}

/// A tab's width: its title and, when there is one, the count badge.
fn tab_width(c: &Canvas, label: &str, count: Option<usize>) -> i32 {
    let pad = c.s(12.0);
    let badge = count.map_or(0, |n| c.s(8.0) + badge_width(c, n));
    pad + c.measure(label, F_GROUP).0 + badge + pad
}

fn badge_width(c: &Canvas, n: usize) -> i32 {
    (c.measure(&n.to_string(), F_HINT).0 + c.s(12.0)).max(c.s(22.0))
}

/// One tab of the strip: the title (TEXT when showing, MUTED otherwise), a count badge, and a
/// lime underline under the showing one.
fn paint_tab(c: &mut Canvas, r: Rect, label: &str, count: Option<usize>, active: bool, st: &WState) {
    let pad = c.s(12.0);
    if st.hover && !active {
        c.fill_round_rect(r.inset(0, c.s(3.0)), c.s(RADIUS), SELECTED);
    }
    let lw = c.measure(label, F_GROUP).0;
    let col = if active { TEXT } else if st.hover { TEXT_DIM } else { MUTED };
    c.text(Rect::new(r.x + pad, r.y, lw + c.s(2.0), r.h), label, F_GROUP, col, Align::Left);
    if let Some(n) = count {
        let bw = badge_width(c, n);
        let bh = c.s(18.0);
        let b = Rect::new(r.x + pad + lw + c.s(8.0), r.cy() - bh / 2, bw, bh);
        c.fill_round_rect(b, bh / 2, if active { ACCENT } else { RAISED });
        c.text(b, &n.to_string(), F_HINT, if active { ON_ACCENT } else { MUTED }, Align::Center);
    }
    if active {
        let uh = c.s(2.0).max(2);
        c.fill_rect(Rect::new(r.x, r.bottom() - uh, r.w, uh), ACCENT);
    }
    if st.focused {
        super::kit::focus_ring(c, r.inset(0, c.s(3.0)), c.s(RADIUS));
    }
}

impl Surface for DoctorWindow {
    fn paint(&mut self, c: &mut Canvas) {
        self.scale = c.scale;
        self.size = (c.width(), c.height());
        self.layout(c);
        c.fill_rect(c.bounds(), BG);
        let view = self.scroll.view;
        let items = std::mem::take(&mut self.items);
        let n = self.head_n.min(items.len());
        c.push_clip(view);
        for it in &items[n..] {
            if it.rect.intersects(&view) {
                self.paint_item(c, it);
            }
        }
        c.pop_clip();
        for it in &items[..n] {
            self.paint_item(c, it);
        }
        self.items = items;
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
            Event::Timer(TIMER_TAB_MEMORY) => {
                self.set_tab(Tab::Memory);
                host.invalidate();
            }
            Event::Timer(TIMER_TAB_DISK) => {
                self.set_tab(Tab::Disk);
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
                    // Ctrl+Tab and Ctrl+Shift+Tab switch tabs (two tabs: both go to the other).
                    VK_TAB if m.ctrl => {
                        let next = self.tab.next();
                        self.set_tab(next);
                        host.invalidate();
                    }
                    // Left and Right on a focused tab move to the other tab and show it.
                    VK_LEFT | VK_RIGHT if matches!(self.focus, Some(Act::Tab(_))) => {
                        let next = self.tab.next();
                        self.set_tab(next);
                        self.focus = Some(Act::Tab(next));
                        host.invalidate();
                    }
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
                            let content = self.items.iter().skip(self.head_n).find(|it| it.act == Some(list[next])).map(|it| it.rect);
                            if let Some(r) = content {
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
                if self.hwnd != 0 {
                    let _ = super::ui_state::save_str(KIND, "tab", self.tab.key());
                }
            }
            _ => return false,
        }
        true
    }
}

/// The tab the window showed last time (ui-state.toml `[doctor] tab`).
pub fn last_tab() -> Option<Tab> {
    super::ui_state::load_str(KIND, "tab").as_deref().and_then(Tab::from_key)
}

/// Open the checkup window on `tab` (the last one shown when `None`; it scans right away), or
/// bring the open one to the front and show `tab` there. Call on the UI thread. `commands`
/// receives what rows ask for ("Show me how").
pub fn open_doctor(source: Box<dyn DoctorSource>, commands: Box<dyn Fn(Command)>, tab: Option<Tab>) -> HWND {
    if let Some(h) = window::find(KIND) {
        window::bring_to_front(h);
        if let Some(t) = tab {
            wake(h as isize, if t == Tab::Disk { TIMER_TAB_DISK } else { TIMER_TAB_MEMORY });
        }
        return h;
    }
    let tab = tab.or_else(last_tab).unwrap_or_default();
    let spec = WindowSpec { kind: KIND, title: TITLE.into(), size: (760, 720), min_size: (620, 480), placement: super::ui_state::load(KIND), resizable: true };
    window::open(spec, Box::new(DoctorWindow::new(source).with_commands(commands).with_tab(tab)))
}

/// Paint the window at `w` (96-DPI px) wide and `h` high, or the whole page with `h == None`.
pub fn render(win: &mut DoctorWindow, scale: Scale, w: i32, h: Option<i32>) -> Canvas {
    let pw = scale.s(w as f32);
    let ph = match h {
        Some(h) => scale.s(h as f32),
        None => {
            let probe = Canvas::new(pw, scale.s(720.0), scale);
            win.layout(&probe);
            win.head.h + win.scroll.content + DoctorWindow::footer_h(&probe)
        }
    };
    let mut c = Canvas::new(pw, ph, scale);
    win.paint(&mut c);
    c
}

#[cfg(test)]
mod frees_words_tests {
    use super::DiskRow;
    fn row(gb: Option<f64>, conf: &str, max: Option<f64>) -> DiskRow {
        DiskRow { volume: "D:".into(), frees_gb: gb, confidence: conf.into(), frees_max_gb: max }
    }
    #[test]
    fn a_lower_bound_of_nothing_says_up_to_and_not_counted_says_so() {
        assert_eq!(row(Some(0.0), "lower_bound", Some(23.7)).frees_words(), "up to 23.7 GB");
        assert_eq!(row(Some(5.0), "lower_bound", Some(23.7)).frees_words(), "at least 5.0 GB");
        assert_eq!(row(Some(0.0), "lower_bound", None).frees_words(), "at least 0 MB");
        assert_eq!(row(None, "unknown", None).frees_words(), "an amount not counted");
        assert_eq!(row(Some(3.0), "estimate", Some(3.0)).frees_words(), "about 3.0 GB");
    }
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

    fn ids(m: &DoctorModel, s: &Section) -> Vec<String> {
        s.rows.iter().map(|&i| m.report.findings[i].id.clone()).collect()
    }

    #[test]
    fn sections_follow_the_spec_order_and_hide_empty_ones() {
        let m = model();
        let mem = m.sections(Tab::Memory);
        let kinds: Vec<SectionKind> = mem.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, SectionKind::ORDER.iter().copied().filter(|k| *k != SectionKind::Disk).collect::<Vec<_>>(), "no drive section on Memory");
        let not_needed = &mem[0];
        assert_eq!(not_needed.title, "NOT NEEDED FOR UNREAL");
        assert_eq!(not_needed.rows.len(), 5);
        assert_eq!(not_needed.ram_mb, 1270 + 980 + 640 + 410 + 560);
        let this_pc = mem.last().unwrap();
        assert_eq!(this_pc.kind, SectionKind::ThisPc);
        assert_eq!(this_pc.ram_mb, 0, "THIS PC shows no memory");
        assert_eq!(ids(&m, this_pc), ["sys:pagefile", "sys:ram"], "only the RAM-side notices");

        // Disk space: one section per drive in letter order, then the drive notices.
        let disk = m.sections(Tab::Disk);
        let titles: Vec<&str> = disk.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["ON C:", "ON D:", "THIS PC"]);
        assert_eq!(ids(&m, &disk[0]), ["disk:d3"]);
        assert_eq!(ids(&m, &disk[1]), ["disk:d1", "disk:d2"]);
        assert_eq!(ids(&m, &disk[2]), ["disk:low:d", "dev:perf_mode_off"]);
        assert!(disk.iter().all(|s| s.ram_mb == 0), "drive rows show no memory");

        let clear = DoctorModel::new(FakeDoctor::report(true));
        assert!(clear.sections(Tab::Memory).is_empty() && clear.sections(Tab::Disk).is_empty());
        assert!(clear.all_clear(Tab::Memory) && clear.all_clear(Tab::Disk));
        assert!(!m.all_clear(Tab::Memory) && !m.all_clear(Tab::Disk));
    }

    #[test]
    fn each_finding_goes_to_one_tab_by_where_it_came_from() {
        let m = model();
        let tab = |id: &str| tab_of(&m.report.findings[idx(&m, id)]);
        for id in ["app:steam", "startup:hkcu:Steam", "startup:hklm:iCUE", "app:blender", "app:nwsync", "app:discord", "ue:orphans", "sys:pagefile", "sys:ram"] {
            assert_eq!(tab(id), Tab::Memory, "{}", id);
        }
        for id in ["disk:d1", "disk:d2", "disk:d3", "disk:low:d", "dev:perf_mode_off"] {
            assert_eq!(tab(id), Tab::Disk, "{}", id);
        }
        // By origin, not by the words: a THIS PC row without the drive mark stays on Memory
        // whatever its title says, and one with it goes to Disk space.
        let mut f = m.report.findings[idx(&m, "sys:pagefile")].clone();
        f.title = "Your Dev Drive is almost full".into();
        assert_eq!(tab_of(&f), Tab::Memory);
        f.drive_notice = true;
        assert_eq!(tab_of(&f), Tab::Disk);
        // Every row is on exactly one tab, and the counts add up to every row shown.
        let shown = m.report.findings.iter().filter(|f| section_of(f).is_some()).count();
        assert_eq!(m.count(Tab::Memory) + m.count(Tab::Disk), shown);
        assert_eq!(m.count(Tab::Disk), 5);
        assert_eq!(m.count(Tab::Memory), shown - 5);
        for i in 0..m.rows.len() {
            assert!(m.in_tab(i, Tab::Memory) != m.in_tab(i, Tab::Disk));
        }
    }

    #[test]
    fn totals_and_picks_stay_on_their_tab() {
        let mut m = model();
        let (build, npm, steam) = (idx(&m, "disk:d1"), idx(&m, "disk:d3"), idx(&m, "app:steam"));
        let mem_gb = m.selected_gb();
        let mem_picks = m.picks(Tab::Memory);
        assert!(!m.any_checked(Tab::Disk), "nothing on the drives starts checked");
        m.toggle(build);
        m.toggle(npm);
        // Checking drive rows changes nothing on Memory.
        assert_eq!(m.selected_gb(), mem_gb);
        assert_eq!(m.picks(Tab::Memory), mem_picks);
        assert_eq!(m.picks(Tab::Disk), vec![("disk:d1".to_string(), ActionKind::DiskNow), ("disk:d3".to_string(), ActionKind::DiskNow)]);
        assert!(m.picks(Tab::Memory).iter().all(|(id, _)| !id.starts_with("disk:")));
        assert!(m.picks(Tab::Disk).iter().all(|(id, _)| id.starts_with("disk:")));
        assert!(m.ending_rows().iter().all(|&i| m.in_tab(i, Tab::Memory)));
        assert_eq!(m.disk_rows(), vec![build, npm]);
        // Running one tab leaves the other's checks alone.
        let picks = m.begin_apply(Tab::Disk);
        assert_eq!(picks.len(), 2);
        assert!(m.rows[steam].checked && !m.rows[steam].busy, "Memory picks stay checked");
        assert!(m.rows[build].busy && !m.rows[build].checked);
        assert_eq!(m.picks(Tab::Memory), mem_picks);
    }

    #[test]
    fn footer_words_for_rows_only_sized_on_a_dev_drive() {
        let lb = |v: &str, g: f64, m: f64| DiskRow { volume: v.into(), frees_gb: Some(g), confidence: "lower_bound".into(), frees_max_gb: Some(m) };
        assert_eq!(disk_footer_words(&[(ActionKind::DiskNow, lb("D:", 0.0, 14.9)), (ActionKind::DiskNow, lb("D:", 0.0, 24.0))]), "Selected: frees up to 38.9 GB on D: (predicted)");
        assert_eq!(disk_footer_words(&[(ActionKind::DiskNow, lb("D:", 5.0, 24.0))]), "Selected: frees at least 5.0 GB on D: (predicted)");
        let unk = DiskRow { volume: "D:".into(), frees_gb: None, confidence: "unknown".into(), frees_max_gb: None };
        assert_eq!(disk_footer_words(&[(ActionKind::DiskNow, unk)]), "Selected: frees an amount not counted on D: (predicted)");
    }

    #[test]
    fn footer_words_per_tab() {
        assert_eq!(memory_footer_words(1680.0 / 1024.0), "Selected: frees about 1.6 GB of RAM");
        assert_eq!(memory_footer_words(0.0), "Selected: frees about 0.0 GB of RAM");
        let row = |v: &str, gb: Option<f64>| DiskRow { volume: v.into(), frees_gb: gb, confidence: "exact".into(), frees_max_gb: None };
        assert_eq!(disk_footer_words(&[]), "Selected: nothing yet");
        assert_eq!(disk_footer_words(&[(ActionKind::DiskNow, row("D:", Some(16.4)))]), "Selected: frees about 16.4 GB on D: (predicted)");
        // Per drive, drives in letter order; a tool's clean counts, bin moves don't.
        assert_eq!(
            disk_footer_words(&[
                (ActionKind::DiskNow, row("D:", Some(16.4))),
                (ActionKind::DiskTool, row("C:", Some(3.1))),
                (ActionKind::DiskNow, row("d:", Some(1.6))),
            ]),
            "Selected: frees about 3.1 GB on C: and about 18.0 GB on D: (predicted)"
        );
        assert_eq!(
            disk_footer_words(&[(ActionKind::DiskNow, row("D:", Some(16.4))), (ActionKind::DiskBin, row("D:", Some(0.4)))]),
            "Selected: frees about 16.4 GB on D: (predicted) · 1 item to the bin"
        );
        assert_eq!(
            disk_footer_words(&[(ActionKind::DiskBin, row("D:", Some(0.4))), (ActionKind::DiskBin, row("C:", Some(2.0)))]),
            "Selected: 2 items to the bin (frees nothing until it's emptied)"
        );
        // Not measured: never a made-up number.
        assert_eq!(disk_footer_words(&[(ActionKind::DiskNow, row("E:", None))]), "Selected: frees an amount not counted on E: (predicted)");
        assert_eq!(
            disk_footer_words(&[(ActionKind::DiskNow, row("E:", None)), (ActionKind::DiskNow, row("E:", Some(2.0)))]),
            "Selected: frees at least 2.0 GB on E: (predicted)"
        );

        // The window says the showing tab's total, with its own button.
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        w.scan(None);
        assert_eq!(w.footer_text(), "Selected: frees about 1.6 GB of RAM");
        assert_eq!(w.tab().button(), "FREE MEMORY");
        w.set_tab(Tab::Disk);
        assert_eq!(w.footer_text(), "Selected: nothing yet");
        assert_eq!(w.tab().button(), "FREE DISK SPACE");
        let m = w.model.as_mut().unwrap();
        let (build, npm) = (idx(m, "disk:d1"), idx(m, "disk:d3"));
        m.toggle(build);
        m.toggle(npm);
        assert_eq!(w.footer_text(), "Selected: frees about 3.1 GB on C: and about 16.4 GB on D: (predicted)");
        w.set_tab(Tab::Memory);
        assert_eq!(w.footer_text(), "Selected: frees about 1.6 GB of RAM", "drive picks never count as RAM");
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
        assert!(m.picks(Tab::Memory).iter().all(|(id, _)| id != "startup:hklm:iCUE" && id != "sys:pagefile"));
        let notice = idx(&m, "disk:low:d");
        assert!(!m.selectable(notice), "drive notices are notes, never picks");
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
        assert!(m.picks(Tab::Memory).contains(&("app:spotify".into(), ActionKind::BlockWhileUnreal)));
        assert!(m.picks(Tab::Memory).contains(&("app:spotify".into(), ActionKind::Close)));
        // Unchecking everything: nothing to clean up.
        for i in 0..m.rows.len() {
            m.rows[i].checked = false;
        }
        assert!(!m.any_checked(Tab::Memory));
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
        let picks = m.begin_apply(Tab::Memory);
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
        assert_eq!(w.can_undo, [false, false]);
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.can_undo, [true, false], "a memory cleanup undoes on Memory only");
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
        assert!(!w.can_undo[0]);
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
        assert!(!w.can_undo[0]);

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
        assert!(w.can_undo[0]);
    }

    #[test]
    fn drive_rows_list_what_goes_and_ask_first() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        let m = w.model.as_mut().unwrap();
        let (build, logs, npm) = (idx(m, "disk:d1"), idx(m, "disk:d2"), idx(m, "disk:d3"));
        assert!(!m.rows[build].checked && !m.rows[logs].checked, "deleting files is never pre-checked");
        let labels = |m: &DoctorModel, i: usize| choices(&m.report.findings[i]).into_iter().map(|c| c.label).collect::<Vec<_>>();
        assert_eq!(labels(m, build), ["Delete it", "Move to the bin"], "regenerable: delete first");
        assert_eq!(labels(m, logs), ["Move to the bin", "Delete it"], "disposable: the bin first");
        assert_eq!(labels(m, npm), ["Delete it", "Move to the bin", "Clean with npm"]);
        for r in &mut m.rows {
            r.checked = false;
        }
        m.toggle(build);
        m.toggle(logs);
        m.toggle(npm);
        m.set_choice(npm, 2);
        // On the Memory tab FREE MEMORY has nothing to run: the drive rows are another tab's.
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.confirm, None);
        w.activate(Act::Tab(Tab::Disk), &mut host);
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.confirm, Some(Confirm::Disk(vec![build, logs, npm])));
        let d = w.dialog().unwrap();
        assert_eq!(d.title, "Clean up 3 items on your drives?");
        assert_eq!(
            d.body,
            "Delete for good: Build files for S2 on D: (frees 16.4 GB predicted)\n\
             Move to the bin: Logs for S2 (frees nothing until the bin is emptied)\n\
             Clean with npm: npm cache (frees about 3.1 GB predicted; can't be undone)\n\
             Deleting can't be undone. The space is measured afterwards and you'll get a notification."
        );
        assert_eq!(d.ok.label, "Delete");
        assert!(w.model.as_ref().unwrap().rows.iter().all(|r| r.outcomes.is_empty()), "nothing ran before the answer");
        // Cancel: nothing ran.
        w.event(Event::Key(VK_ESCAPE, Default::default()), &mut host);
        assert!(w.model.as_ref().unwrap().rows.iter().all(|r| r.outcomes.is_empty()));
        // OK: the rows turn into their results; the bin move can be undone.
        w.activate(Act::CleanUp, &mut host);
        w.confirm_ok(&mut host);
        let m = w.model.as_ref().unwrap();
        assert_eq!(outcome_text(&m.rows[build].outcomes).0, "Deleted · frees 16.4 GB (predicted); measured later");
        assert_eq!(outcome_text(&m.rows[logs].outcomes).0, "Moved to the bin · frees nothing until it's emptied");
        assert_eq!(w.can_undo, [false, true], "the bin moves undo on Disk space only");
        // Only bin moves: the confirmation says so and offers the bin.
        let (t, b, ok) = disk_confirm_words(&[("Logs", ActionKind::DiskBin, &DiskRow::default(), "")]);
        assert_eq!(t, "Clean up 1 item on your drives?");
        assert!(b.ends_with("Undo with \"Undo last cleanup\". The bin empties itself after a day."));
        assert_eq!(ok, "Move to the bin");
    }

    #[test]
    fn memory_and_drive_picks_never_run_together() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        let m = w.model.as_mut().unwrap();
        let (build, orphans, steam) = (idx(m, "disk:d1"), idx(m, "ue:orphans"), idx(m, "app:steam"));
        m.toggle(build);
        m.toggle(orphans);
        // FREE MEMORY: only the memory question, only memory rows run.
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.confirm, Some(Confirm::CleanUp(vec![orphans])));
        w.confirm_ok(&mut host);
        let m = w.model.as_ref().unwrap();
        assert!(!m.rows[orphans].outcomes.is_empty() && !m.rows[steam].outcomes.is_empty());
        assert!(m.rows[build].outcomes.is_empty() && m.rows[build].checked, "the drive row didn't run and is still checked");
        assert_eq!(w.can_undo, [true, false]);
        // FREE DISK SPACE: only the drive question, only the drive row runs.
        w.set_tab(Tab::Disk);
        w.activate(Act::CleanUp, &mut host);
        assert_eq!(w.confirm, Some(Confirm::Disk(vec![build])));
        w.confirm_ok(&mut host);
        assert_eq!(w.confirm, None);
        assert!(!w.model.as_ref().unwrap().rows[build].outcomes.is_empty());
        assert_eq!(w.can_undo, [true, true]);
        // Undo on Disk space puts the drive cleanup back and leaves Memory's results and undo.
        w.activate(Act::Undo, &mut host);
        assert_eq!(w.can_undo, [true, false]);
        let m = w.model.as_ref().unwrap();
        assert!(!m.rows[idx(m, "app:steam")].outcomes.is_empty(), "Memory's results stay");
        assert!(m.rows[idx(m, "disk:d1")].outcomes.is_empty(), "Disk space was scanned again");
        // Undo on Memory: its own.
        w.set_tab(Tab::Memory);
        w.activate(Act::Undo, &mut host);
        assert_eq!(w.can_undo, [false, false]);
    }

    #[test]
    fn undo_link_shows_on_its_own_tab() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        w.set_tab(Tab::Disk);
        let m = w.model.as_mut().unwrap();
        let logs = idx(m, "disk:d2");
        m.toggle(logs);
        w.activate(Act::CleanUp, &mut host);
        w.confirm_ok(&mut host);
        let c = Canvas::new(760, 720, Scale(1.0));
        assert!(w.footer_layout(&c).1.is_some(), "Disk space can undo its bin move");
        assert!(w.focusables().contains(&Act::Undo));
        w.set_tab(Tab::Memory);
        assert!(w.footer_layout(&c).1.is_none(), "Memory has nothing to undo");
        assert!(!w.focusables().contains(&Act::Undo));
        // Undo on Memory does nothing to the drive cleanup.
        w.activate(Act::Undo, &mut host);
        assert_eq!(w.can_undo, [false, true]);
    }

    #[test]
    fn disk_tab_without_a_disk_checkup_explains_and_runs_one() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::without_disk_checkup())).with_tab(Tab::Disk);
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        // Memory picks made before, kept through the disk checkup.
        let m = w.model.as_mut().unwrap();
        let spotify = idx(m, "app:spotify");
        m.toggle(spotify);
        assert!(m.report.disk_checked.is_none());
        assert_eq!(m.sections(Tab::Disk).iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["THIS PC"], "the drive notices show anyway");
        let c = Canvas::new(760, 3000, Scale(1.0));
        w.layout(&c);
        let msg: Vec<String> = w.items().iter().filter_map(|it| if let Paint::Message(l) = &it.paint { Some(l.join(" ")) } else { None }).collect();
        assert_eq!(msg, [NO_DISK_CHECKUP.to_string()]);
        let run = w.items().iter().find(|it| it.act == Some(Act::DiskCheckup)).unwrap();
        assert_eq!(run.paint, Paint::Button("Run the disk checkup".into()));
        assert_eq!(w.footer_text(), "", "nothing to pick yet");
        assert!(!w.items().iter().any(|it| matches!(it.paint, Paint::Result(_))), "no number before a checkup");
        w.activate(Act::DiskCheckup, &mut host);
        let m = w.model.as_ref().unwrap();
        assert_eq!(m.report.disk_checked.as_deref(), Some("2026-11-14 13:40"));
        assert_eq!(m.sections(Tab::Disk).iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["ON C:", "ON D:", "THIS PC"]);
        assert!(m.rows[idx(m, "app:spotify")].checked, "the Memory tab's checks stay");
        w.layout(&c);
        assert!(w.items().iter().any(|it| it.paint == Paint::Result("3 ITEMS".into())));
        assert!(w.items().iter().any(|it| it.paint == Paint::Link { label: "Count again".into(), color: MUTED }));
        assert_eq!(w.footer_text(), "Selected: nothing yet");
    }

    #[test]
    fn a_failed_disk_checkup_says_why() {
        struct NoCli(FakeDoctor);
        impl DoctorSource for NoCli {
            fn scan(&self) -> Report {
                self.0.scan()
            }
            fn apply(&self, p: &[(String, ActionKind)], f: bool) -> Vec<Outcome> {
                self.0.apply(p, f)
            }
            fn undo_last(&self, t: Tab) -> Vec<Outcome> {
                self.0.undo_last(t)
            }
            fn can_undo(&self, t: Tab) -> bool {
                self.0.can_undo(t)
            }
        }
        let mut w = DoctorWindow::new(Box::new(NoCli(FakeDoctor::without_disk_checkup()))).with_tab(Tab::Disk);
        w.scan(None);
        w.disk_checkup(None);
        let c = Canvas::new(760, 3000, Scale(1.0));
        w.layout(&c);
        assert!(w.items().iter().any(|it| matches!(&it.paint, Paint::Info { lines, color } if *color == ERROR && lines[0].starts_with("The disk checkup didn't run: "))));
    }

    #[test]
    fn tabs_switch_by_click_keyboard_and_command() {
        let mut w = DoctorWindow::new(Box::new(FakeDoctor::new()));
        let mut host = Host::detached((760, 720), Scale(1.0));
        w.scan(None);
        let c = Canvas::new(760, 720, Scale(1.0));
        w.layout(&c);
        // The strip: both tabs with their counts, Memory showing.
        let tabs: Vec<Paint> = w.items().iter().filter(|it| matches!(it.paint, Paint::Tab { .. })).map(|it| it.paint.clone()).collect();
        let m = w.model.as_ref().unwrap();
        assert_eq!(
            tabs,
            [
                Paint::Tab { label: "Memory".into(), count: Some(m.actionable(Tab::Memory)), active: true },
                Paint::Tab { label: "Disk space".into(), count: Some(3), active: false },
            ]
        );
        // A title counts what can be acted on, not notices: Disk space's 5 rows include 2 notices.
        assert_eq!(m.count(Tab::Disk), 5);
        assert!(m.actionable(Tab::Memory) < m.count(Tab::Memory));
        // Clicking the tab.
        w.size = (760, 720);
        let r = w.items().iter().find(|it| it.act == Some(Act::Tab(Tab::Disk))).unwrap().rect;
        w.event(Event::MouseDown(Pt::new(r.cx(), r.cy()), MouseButton::Left), &mut host);
        w.event(Event::MouseUp(Pt::new(r.cx(), r.cy()), MouseButton::Left), &mut host);
        assert_eq!(w.tab(), Tab::Disk);
        // Ctrl+Tab.
        let ctrl = window::Mods { ctrl: true, ..Default::default() };
        w.event(Event::Key(VK_TAB, ctrl), &mut host);
        assert_eq!(w.tab(), Tab::Memory);
        // Tab walks the header in order (Check again, then the tabs); Right shows the other
        // tab, Enter on a tab shows it.
        w.focus = None;
        w.event(Event::Key(VK_TAB, Default::default()), &mut host);
        assert_eq!(w.focus, Some(Act::CheckAgain));
        w.event(Event::Key(VK_TAB, Default::default()), &mut host);
        assert_eq!(w.focus, Some(Act::Tab(Tab::Memory)));
        w.event(Event::Key(VK_RIGHT, Default::default()), &mut host);
        assert_eq!((w.tab(), w.focus), (Tab::Disk, Some(Act::Tab(Tab::Disk))));
        w.focus = Some(Act::Tab(Tab::Memory));
        w.event(Event::Key(VK_RETURN, Default::default()), &mut host);
        assert_eq!(w.tab(), Tab::Memory);
        // The tray asking an open window for a tab.
        w.event(Event::Timer(TIMER_TAB_DISK), &mut host);
        assert_eq!(w.tab(), Tab::Disk);
        w.event(Event::Timer(TIMER_TAB_MEMORY), &mut host);
        assert_eq!(w.tab(), Tab::Memory);
        // Switching away cancels an open question (nothing has run).
        let m = w.model.as_mut().unwrap();
        let orphans = idx(m, "ue:orphans");
        m.toggle(orphans);
        w.activate(Act::CleanUp, &mut host);
        assert!(w.confirm.is_some());
        w.set_tab(Tab::Disk);
        assert_eq!(w.confirm, None);
        assert!(w.model.as_ref().unwrap().rows[orphans].outcomes.is_empty());
        assert_eq!(Tab::from_key(Tab::Disk.key()), Some(Tab::Disk));
        assert_eq!(Tab::from_key("nonsense"), None);
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
        let h = w.head.h + w.scroll.content + DoctorWindow::footer_h(&probe);
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
        assert!(tr.w >= c.measure("Selected: frees about 10.0 GB of RAM", F_VALUE).0, "{ctx}: footer text cut");
        let n = w.head_n;
        assert!(items[..n].iter().all(|i| i.rect.bottom() <= w.head.bottom()), "{ctx}: header outside its area");
        assert!(items[n..].iter().all(|i| i.rect.y >= w.head.bottom()), "{ctx}: content under the header at the top");
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
        w.set_tab(Tab::Disk);
        w.paint(&mut c);
        let mut none = DoctorWindow::new(Box::new(FakeDoctor::without_disk_checkup())).with_tab(Tab::Disk);
        none.paint(&mut c);
        none.scan(None);
        none.paint(&mut c);
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
                w.set_tab(Tab::Disk);
                check_layout(&mut w, scale, width, "disk space");
                let m = w.model.as_mut().unwrap();
                let (build, npm) = (idx(m, "disk:d1"), idx(m, "disk:d3"));
                m.toggle(build);
                m.toggle(npm);
                w.clean_up(None);
                w.confirm_ok(&mut Host::detached((width, 720), Scale(scale)));
                check_layout(&mut w, scale, width, "disk results");
                let mut clear = DoctorWindow::new(Box::new(FakeDoctor::all_clear()));
                clear.scan(None);
                check_layout(&mut clear, scale, width, "all clear");
                clear.set_tab(Tab::Disk);
                check_layout(&mut clear, scale, width, "disk all clear");
                let mut none = DoctorWindow::new(Box::new(FakeDoctor::without_disk_checkup())).with_tab(Tab::Disk);
                none.scan(None);
                check_layout(&mut none, scale, width, "no disk checkup");
            }
        }
    }
}

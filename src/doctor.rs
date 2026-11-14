//! The doctor: what is using memory on this PC that Unreal work doesn't need, and what
//! should be done about it.
//!
//! `scan` looks and explains; it changes nothing. `apply` runs only the actions the person
//! picked, re-checking every "never act on" rule at that moment, and logs each change to
//! `doctor-undo.json` in the data folder so `undo` can reverse it. `Guard` keeps the apps
//! on `[doctor] close_while_unreal` closed while Unreal is running.
//!
//! The pure parts (catalogue matching, verdicts, the report, refusal rules, the undo log,
//! settings edits, the guard) work on plain data and are unit-tested. The Win32 calls are
//! in `doctor_win`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::windows::process::CommandExt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{any_matches, name_matches, Config};
use crate::doctor_win;
use crate::engine::{self, is_protected, SystemStatus, UeProcess};
use crate::startup::{self, Location};
use crate::win::{self, Proc, SysInfo};
use crate::paths;

const MB: u64 = 1024 * 1024;
/// How long `close` waits for an app to exit after asking it to.
pub const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// Runs kept in the undo log.
const UNDO_KEEP_RUNS: usize = 30;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

fn gb(mb: u64) -> f64 {
    (mb as f64 / 1024.0 * 10.0).round() / 10.0
}

/// "640 MB" or "1.2 GB".
pub fn mem_label(mb: u64) -> String {
    if mb < 1024 {
        format!("{} MB", mb)
    } else {
        format!("{:.1} GB", mb as f64 / 1024.0)
    }
}

// ---------------------------------------------------------------- report types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Not needed for Unreal work; closing it is safe and frees real memory.
    Close,
    /// Wanted, but shouldn't compete with Unreal: put it in a MemGuard group.
    HoldBack,
    /// Part of Unreal work or the system; never proposed.
    Keep,
    /// Unknown and large, or known but it depends: shown so the person can decide.
    Review,
    /// A big creative or virtualisation tool that is probably deliberate.
    HeavyTool,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Close => "close",
            Verdict::HoldBack => "hold_back",
            Verdict::Keep => "keep",
            Verdict::Review => "review",
            Verdict::HeavyTool => "heavy_tool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    App,
    Startup,
    Unreal,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Ask the app to close (WM_CLOSE to its windows), wait up to 10 s. Never forced.
    Close,
    /// End the processes. Only with `force`.
    End,
    /// Switch the startup entry off through `StartupApproved`.
    DisableStartup,
    /// Add the app to the matching MemGuard group.
    HoldBack,
    /// Add the app to `[doctor] close_while_unreal`.
    BlockWhileUnreal,
    /// Add the app to `[doctor] keep`, so the checkup stops showing it.
    LeaveIt,
}

impl ActionKind {
    pub const ALL: [ActionKind; 6] = [
        ActionKind::Close,
        ActionKind::End,
        ActionKind::DisableStartup,
        ActionKind::HoldBack,
        ActionKind::BlockWhileUnreal,
        ActionKind::LeaveIt,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ActionKind::Close => "close",
            ActionKind::End => "end",
            ActionKind::DisableStartup => "disable_startup",
            ActionKind::HoldBack => "hold_back",
            ActionKind::BlockWhileUnreal => "block_while_unreal",
            ActionKind::LeaveIt => "leave_it",
        }
    }

    pub fn from_name(s: &str) -> Option<ActionKind> {
        ActionKind::ALL.into_iter().find(|a| a.name().eq_ignore_ascii_case(s))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub kind: ActionKind,
    /// Button text: "Close", "Don't start with Windows", ...
    pub label: String,
    /// How it is undone, in plain words ("Start the app again", "None").
    pub undo: String,
    /// Runs only with `force` (an explicit second confirmation).
    pub needs_force: bool,
}

impl Action {
    pub fn new(kind: ActionKind, group: Option<&str>) -> Action {
        let (label, undo, needs_force) = match kind {
            ActionKind::Close => ("Close".to_string(), "Start the app again".to_string(), false),
            ActionKind::End => ("End it".to_string(), "None: anything unsaved in it is lost".to_string(), true),
            ActionKind::DisableStartup => ("Don't start with Windows".to_string(), "Switch it back on".to_string(), false),
            ActionKind::HoldBack => (
                format!("Hold back (add to {})", group.unwrap_or("a group")),
                "Remove it from the group".to_string(),
                false,
            ),
            ActionKind::BlockWhileUnreal => {
                ("Close whenever Unreal is running".to_string(), "Remove it from the list".to_string(), false)
            }
            ActionKind::LeaveIt => ("Leave it".to_string(), "Remove it from the keep list".to_string(), false),
        };
        Action { kind, label, undo, needs_force }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable: "app:steam", "startup:hkcu:Steam", "ue:orphans", "sys:pagefile".
    pub id: String,
    pub kind: FindingKind,
    pub verdict: Verdict,
    /// Plain words, app voice.
    pub title: String,
    pub detail: String,
    pub processes: usize,
    pub ram_mb: u64,
    pub committed_mb: u64,
    /// Proposed, most recommended first.
    pub actions: Vec<Action>,
    /// True = included in "fix the safe ones".
    pub recommended: bool,
    /// Catalogue category ("Game launcher"), or "" when unknown.
    #[serde(default)]
    pub category: String,
    /// Executable names (without `.exe`) this finding covers.
    #[serde(default)]
    pub names: Vec<String>,
    /// Process ids at scan time. `apply` re-checks each one before acting.
    #[serde(default)]
    pub pids: Vec<u32>,
    /// Has a window on screen.
    #[serde(default)]
    pub has_window: bool,
    /// The MemGuard group `hold_back` adds it to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The startup entry, for `Startup` findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<startup::Entry>,
    /// Company from the exe's version info, for unknown apps under "Worth a look"; may be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub company: String,
}

impl Finding {
    fn new(id: String, kind: FindingKind, verdict: Verdict, title: String, detail: String) -> Finding {
        Finding {
            id,
            kind,
            verdict,
            title,
            detail,
            processes: 0,
            ram_mb: 0,
            committed_mb: 0,
            actions: Vec::new(),
            recommended: false,
            category: String::new(),
            names: Vec::new(),
            pids: Vec::new(),
            has_window: false,
            group: None,
            startup: None,
            company: String::new(),
        }
    }

    pub fn first_action(&self) -> Option<ActionKind> {
        self.actions.first().map(|a| a.kind)
    }

    pub fn offers(&self, kind: ActionKind) -> bool {
        self.actions.iter().any(|a| a.kind == kind)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub time: String,
    pub system: SystemStatus,
    /// The editor, a build or a cook is running.
    #[serde(default)]
    pub unreal_active: bool,
    pub findings: Vec<Finding>,
    /// What closing the "not needed" apps and Unreal leftovers would free.
    pub reclaimable_ram_gb: f64,
    pub reclaimable_commit_gb: f64,
}

impl Report {
    pub fn finding(&self, id: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.id == id)
    }

    /// The picks "fix the safe ones" runs: the first action of every recommended finding.
    pub fn recommended_picks(&self) -> Vec<(String, ActionKind)> {
        self.findings
            .iter()
            .filter(|f| f.recommended)
            .filter_map(|f| f.first_action().map(|a| (f.id.clone(), a)))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    /// It happened.
    Done,
    /// Asked to close; still running after the wait.
    StillOpen,
    /// Not done because a rule forbids it.
    Refused,
    /// Tried and failed.
    Failed,
    /// Dry run: this is what would happen.
    WouldDo,
    /// An undo that worked.
    Undone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub finding_id: String,
    pub action: ActionKind,
    pub result: OutcomeKind,
    /// "Closed", "Still open — it may be asking you to save", "Switched off", ...
    pub message: String,
    /// Id in the undo log, when it can be undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_id: Option<String>,
}

impl Outcome {
    fn new(finding_id: &str, action: ActionKind, result: OutcomeKind, message: impl Into<String>) -> Outcome {
        Outcome { finding_id: finding_id.to_string(), action, result, message: message.into(), undo_id: None }
    }
}

// ---------------------------------------------------------------- catalogue

pub const CATALOG_TOML: &str = include_str!("doctor_catalog.toml");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogApp {
    pub name: String,
    pub verdict: Verdict,
    pub category: String,
    pub processes: Vec<String>,
    pub description: String,
    #[serde(default)]
    pub group: Option<String>,
    /// A family of separate apps (music players, other browsers): each app is its own
    /// finding, titled by its executable name, rather than one finding for the entry.
    #[serde(default)]
    pub family: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    app: Vec<CatalogApp>,
}

pub fn parse_catalog(text: &str) -> Result<Vec<CatalogApp>, String> {
    let file: CatalogFile = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(file.app)
}

/// The built-in catalogue.
pub fn catalog() -> &'static [CatalogApp] {
    static CATALOG: OnceLock<Vec<CatalogApp>> = OnceLock::new();
    CATALOG.get_or_init(|| parse_catalog(CATALOG_TOML).expect("built-in doctor catalogue is valid"))
}

/// How well `pattern` matches `name`: exact names beat wildcards, longer wildcards beat
/// shorter ones. `None` = no match.
fn specificity(pattern: &str, name: &str) -> Option<usize> {
    if !name_matches(pattern, name) {
        return None;
    }
    Some(if pattern.ends_with('*') { pattern.len() - 1 } else { 100_000 })
}

/// The catalogue entry for a process name. On a tie, `keep` wins (the safe answer).
pub fn catalog_match<'a>(cat: &'a [CatalogApp], name: &str) -> Option<&'a CatalogApp> {
    let mut best: Option<(&CatalogApp, usize)> = None;
    for app in cat {
        let Some(score) = app.processes.iter().filter_map(|p| specificity(p, name)).max() else { continue };
        match best {
            Some((b, s)) if s > score || (s == score && (b.verdict == Verdict::Keep || app.verdict != Verdict::Keep)) => {}
            _ => best = Some((app, score)),
        }
    }
    best.map(|(a, _)| a)
}

// ---------------------------------------------------------------- settings ([doctor] in settings.toml)

/// `[doctor]` settings. Read straight from settings.toml; a missing file or key means the
/// defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DoctorSettings {
    /// Never proposed, whatever the catalogue says.
    pub keep: Vec<String>,
    /// Treated as "not needed for Unreal".
    pub close: Vec<String>,
    /// Asked to close whenever the editor, a build or a cook is running.
    pub close_while_unreal: Vec<String>,
    /// Apps using less committed memory than this are left out (unless they start with Windows).
    pub ignore_below_mb: u64,
    /// Unknown apps above this much committed memory are shown under "Worth a look".
    pub review_above_mb: u64,
    /// The tray suggests a checkup when Unreal work starts and at least this much could be freed.
    pub nudge_above_gb: f64,
}

impl Default for DoctorSettings {
    fn default() -> Self {
        DoctorSettings {
            keep: Vec::new(),
            close: Vec::new(),
            close_while_unreal: Vec::new(),
            ignore_below_mb: 150,
            review_above_mb: 1024,
            nudge_above_gb: 4.0,
        }
    }
}

impl DoctorSettings {
    /// The `[doctor]` table of a settings file's text.
    pub fn from_settings_text(text: &str) -> Result<DoctorSettings, String> {
        let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut ds = match table.get("doctor") {
            None => DoctorSettings::default(),
            Some(v) => v.clone().try_into::<DoctorSettings>().map_err(|e| format!("[doctor]: {}", e))?,
        };
        for list in [&mut ds.keep, &mut ds.close, &mut ds.close_while_unreal] {
            for p in list.iter_mut() {
                if p.to_ascii_lowercase().ends_with(".exe") {
                    p.truncate(p.len() - 4);
                }
            }
        }
        Ok(ds)
    }

    /// From settings.toml in the data folder; defaults if it's missing or broken.
    pub fn load() -> DoctorSettings {
        fs::read_to_string(paths::config_file())
            .ok()
            .and_then(|t| DoctorSettings::from_settings_text(&t).ok())
            .unwrap_or_default()
    }
}

/// The verdict for one process name: `[doctor] keep`, then `[doctor] close`, then the
/// catalogue. `None` = unknown.
pub fn classify<'a>(ds: &DoctorSettings, cat: &'a [CatalogApp], name: &str) -> (Option<Verdict>, Option<&'a CatalogApp>) {
    let entry = catalog_match(cat, name);
    if any_matches(&ds.keep, name) {
        return (Some(Verdict::Keep), entry);
    }
    if any_matches(&ds.close, name) {
        return (Some(Verdict::Close), entry);
    }
    (entry.map(|e| e.verdict), entry)
}

// ---------------------------------------------------------------- never act on

/// Security software: never closed, ended or switched off, whatever the settings say.
const SECURITY: &[&str] = &[
    "MsMpEng", "MpDefenderCoreService", "NisSrv", "SecurityHealthService", "SecurityHealthSystray", "smartscreen",
    "DefenderSessionHelper", "MpCmdRun", "MsSense", "SenseNdr", "SgrmBroker", "avp", "avpui", "AvastUI", "AvastSvc",
    "AVGUI", "AVGSvc", "bdagent", "bdservicehost", "ekrn", "egui", "mbam", "MBAMService", "mbamtray", "Norton*",
    "mcshield", "McUICnt", "CSFalcon*", "SentinelAgent", "SentinelUI", "Sophos*", "WRSA", "ccSvcHst",
];

/// Windows itself: closing these logs you out or breaks the desktop.
const WINDOWS_CORE: &[&str] = &[
    "explorer", "dwm", "csrss", "winlogon", "wininit", "services", "lsass", "smss", "svchost", "sihost", "fontdrvhost",
    "ctfmon", "taskhostw", "RuntimeBroker", "dllhost", "conhost", "ShellExperienceHost", "StartMenuExperienceHost",
    "SearchHost", "SearchApp", "TextInputHost", "LockApp", "LogonUI", "userinit", "audiodg", "System", "Registry",
    "Memory Compression", "Secure System", "spoolsv", "WmiPrvSE", "ApplicationFrameHost", "ShellHost", "WUDFHost",
    "consent", "TrustedInstaller", "msiexec",
];

/// Why a process may never be touched, regardless of settings.
pub fn hard_never(name: &str) -> Option<&'static str> {
    let any = |list: &[&str]| list.iter().any(|p| name_matches(p, name));
    if name.to_ascii_lowercase().contains("memguard") {
        Some("It's Ather MemGuard itself.")
    } else if any(SECURITY) {
        Some("It's security software, which is never closed.")
    } else if any(WINDOWS_CORE) {
        Some("It's part of Windows.")
    } else {
        None
    }
}

/// Who a process belongs to, as far as the rules care.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcFacts {
    pub owned: bool,
    pub same_session: bool,
}

/// MemGuard and every process it was started from (its parent, grandparent, ...). Acting
/// on these would close the agent or terminal that ran `memguard doctor --fix`.
pub fn self_tree(procs: &[Proc], me: u32) -> HashSet<u32> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut tree = HashSet::from([me]);
    let mut cur = me;
    while let Some(p) = by_pid.get(&cur) {
        let Some(parent) = by_pid.get(&p.ppid) else { break };
        // A parent created after its child is a recycled PID, not the real parent.
        if parent.pid == 0 || tree.contains(&parent.pid) || (p.created != 0 && parent.created > p.created) {
            break;
        }
        tree.insert(parent.pid);
        cur = parent.pid;
    }
    tree
}

/// Everything the "never act on" rules need, gathered at action time.
pub struct ActContext<'a> {
    pub cfg: &'a Config,
    pub ds: &'a DoctorSettings,
    pub self_tree: HashSet<u32>,
    pub facts: HashMap<u32, ProcFacts>,
}

/// Why this process must not be closed or ended, or `None` if it may be. `orphan` relaxes
/// only the Unreal protect list, for leftover Unreal helpers whose parent is gone.
pub fn refusal(ctx: &ActContext, p: &Proc, orphan: bool) -> Option<String> {
    if p.pid == win::current_pid() {
        return Some("It's Ather MemGuard itself.".into());
    }
    if let Some(why) = hard_never(&p.name) {
        return Some(why.into());
    }
    if ctx.self_tree.contains(&p.pid) {
        return Some("Ather MemGuard was started from it, so it won't close it.".into());
    }
    if !orphan && is_protected(ctx.cfg, p) {
        return Some("It's part of Unreal work and is protected.".into());
    }
    let facts = ctx.facts.get(&p.pid).copied().unwrap_or_default();
    if !facts.owned {
        return Some("It belongs to another user or to Windows.".into());
    }
    if !facts.same_session {
        return Some("It runs in another session.".into());
    }
    if !orphan && classify(ctx.ds, catalog(), &p.name).0 == Some(Verdict::Keep) {
        return Some("It's on the keep list.".into());
    }
    None
}

// ---------------------------------------------------------------- scanning

/// Window facts per process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowFacts {
    /// App windows on screen.
    pub visible: usize,
    /// Hidden but titled main windows (tray apps), which WM_CLOSE can reach.
    pub candidates: usize,
}

/// Everything `scan` reads from the system, so the report itself is a pure function.
pub struct ScanInput {
    pub time: String,
    pub sys: SysInfo,
    pub procs: Vec<Proc>,
    pub facts: HashMap<u32, ProcFacts>,
    pub windows: HashMap<u32, WindowFacts>,
    pub startup: Vec<startup::Entry>,
    pub ue: Vec<UeProcess>,
    pub unreal_active: bool,
    pub memguard_running: bool,
    pub memguard_paused: bool,
    pub self_tree: HashSet<u32>,
}

/// Look at this PC and say what could be freed. Read-only.
pub fn scan(cfg: &Config) -> Report {
    let ds = DoctorSettings::load();
    let input = gather(cfg);
    let mut report = build_report(cfg, &ds, &input, &mut describe_pid);
    // The RAM check reads firmware tables, so it runs once per process (`check_cached`).
    // Only reading here: the "Show me how" page is written when the person clicks it.
    add_ram_findings(&mut report, cfg.general.check_ram_speed, crate::ramcheck::check_cached);
    report
}

/// Is this a THIS PC row from the RAM and BIOS check?
pub fn is_ram_finding(id: &str) -> bool {
    id == "sys:ram" || id.starts_with("sys:ram:")
}

/// Append the RAM check's THIS PC rows when `on` (`general.check_ram_speed`). `ram` is only
/// called when it is on. System rows sort last, so appending keeps the section order.
pub fn add_ram_findings<'a>(report: &mut Report, on: bool, ram: impl FnOnce() -> &'a crate::ramcheck::RamReport) {
    if on {
        report.findings.extend(ram_findings(ram()));
    }
}

/// THIS PC rows from the RAM and BIOS check (`ramcheck`). Pure. When the memory runs below its
/// rated speed: `sys:ram`, the report's headline over its summary. Every other finding the
/// check calls a problem (old Intel microcode, both sticks on one channel) gets its own
/// `sys:ram:<what>` row, also when the speed is fine. Nothing when nothing needs attention.
pub fn ram_findings(r: &crate::ramcheck::RamReport) -> Vec<Finding> {
    use crate::ramcheck::{needs_attention, Severity, Verdict as RamVerdict};
    let mut out = Vec::new();
    if !needs_attention(r) {
        return out;
    }
    let row = |id: String, title: &str, detail: &str| {
        // The report's "the steps below" points at steps this row doesn't show; its link does.
        let detail = detail.replace(" The steps below say how.", "").trim().to_string();
        Finding::new(id, FindingKind::System, Verdict::Review, title.trim().to_string(), detail)
    };
    if r.verdict == RamVerdict::BelowRated {
        out.push(row("sys:ram".into(), &r.headline, &r.summary));
    }
    for f in r.findings.iter().filter(|f| f.severity == Severity::Problem && f.id != "ram:speed") {
        let what = slug(f.id.strip_prefix("ram:").unwrap_or(&f.id));
        let id = format!("sys:ram:{}", what);
        if !out.iter().any(|o: &Finding| o.id == id) {
            out.push(row(id, &f.title, &f.detail));
        }
    }
    out
}

fn describe_pid(pid: u32) -> Option<(String, String)> {
    doctor_win::image_path(pid).and_then(|p| doctor_win::version_strings(&p))
}

fn window_facts() -> HashMap<u32, WindowFacts> {
    let mut map: HashMap<u32, WindowFacts> = HashMap::new();
    for w in doctor_win::top_level_windows() {
        let e = map.entry(w.pid).or_default();
        if w.visible {
            e.visible += 1;
        }
        if w.main_candidate {
            e.candidates += 1;
        }
    }
    map
}

fn facts_for(procs: &[Proc]) -> HashMap<u32, ProcFacts> {
    let me = doctor_win::current_user_sid();
    let session = doctor_win::current_session();
    procs
        .iter()
        .filter(|p| p.pid != 0)
        .map(|p| {
            let o = doctor_win::ownership(p.pid, me.as_deref(), session);
            (p.pid, ProcFacts { owned: o.owned, same_session: o.same_session })
        })
        .collect()
}

fn memguard_state() -> (bool, bool) {
    let running = win::instance_running(paths::mutex());
    let paused = running
        && fs::read_to_string(paths::status_file())
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["paused"].as_bool())
            .unwrap_or(false);
    (running, paused)
}

fn gather(cfg: &Config) -> ScanInput {
    let snap = engine::snapshot();
    let st = engine::analyze(cfg, &snap);
    let (memguard_running, memguard_paused) = memguard_state();
    ScanInput {
        time: st.time.clone(),
        sys: snap.sys,
        facts: facts_for(&snap.procs),
        windows: window_facts(),
        startup: startup::list(),
        ue: st.ue.processes.clone(),
        unreal_active: st.activities.any(),
        memguard_running,
        memguard_paused,
        self_tree: self_tree(&snap.procs, win::current_pid()),
        procs: snap.procs,
    }
}

/// Unreal helper processes whose parent has exited. Same logic as `ops::find_orphans`
/// (copied so `ops.rs` stays with its owner).
pub fn find_orphans<'a>(cfg: &Config, procs: &'a [Proc]) -> Vec<&'a Proc> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    procs
        .iter()
        .filter(|p| {
            let lower = p.name.to_ascii_lowercase();
            let candidate = any_matches(&cfg.ue.workers, &p.name)
                || lower.starts_with("unrealeditor-cmd")
                || lower.starts_with("crashreportclient");
            // One we can't open we couldn't end either (and 0.2.0 never saw its parent link).
            if !candidate || !p.queryable || p.created == 0 {
                return false;
            }
            match by_pid.get(&p.ppid) {
                None => true,
                Some(parent) => parent.created > p.created,
            }
        })
        .collect()
}

#[derive(Default)]
struct AppAcc<'a> {
    title: String,
    entry: Option<&'a CatalogApp>,
    verdict: Option<Verdict>,
    names: BTreeSet<String>,
    pids: Vec<u32>,
    ram: u64,
    committed: u64,
    visible: usize,
    candidates: usize,
    in_self_tree: bool,
    members: Vec<&'a Proc>,
}

fn combine(a: Option<Verdict>, b: Option<Verdict>) -> Option<Verdict> {
    match (a, b) {
        (Some(Verdict::Keep), _) | (_, Some(Verdict::Keep)) => Some(Verdict::Keep),
        (Some(v), _) => Some(v),
        (None, v) => v,
    }
}

fn section_rank(f: &Finding) -> u8 {
    match (f.kind, f.verdict) {
        (FindingKind::App, Verdict::Close) => 0,
        (FindingKind::Startup, _) => 1,
        (FindingKind::App, Verdict::HeavyTool) => 2,
        (FindingKind::App, Verdict::Review) => 3,
        (FindingKind::App, _) => 4,
        (FindingKind::Unreal, _) => 5,
        (FindingKind::System, _) => 6,
    }
}

/// Lowercase id part: letters, digits, `.`, `-` and `_`; anything else becomes `-`.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c.to_ascii_lowercase() } else { '-' })
        .collect()
}

fn group_has(cfg: &Config, name: &str) -> bool {
    cfg.groups.iter().any(|g| any_matches(&g.processes, name))
}

/// Build the report from what `gather` read. Pure apart from `describe`, which looks up an
/// unknown app's file description and company (only called for "Worth a look" apps).
pub fn build_report(
    cfg: &Config,
    ds: &DoctorSettings,
    input: &ScanInput,
    describe: &mut dyn FnMut(u32) -> Option<(String, String)>,
) -> Report {
    let cat = catalog();
    let mut findings = Vec::new();

    // ---- apps
    let mut apps: BTreeMap<String, AppAcc> = BTreeMap::new();
    for p in &input.procs {
        if p.pid == 0 || is_protected(cfg, p) {
            continue;
        }
        let f = input.facts.get(&p.pid).copied().unwrap_or_default();
        if !f.owned || !f.same_session {
            continue;
        }
        let (verdict, entry) = classify(ds, cat, &p.name);
        let key = match entry {
            Some(e) if !e.family => format!("app:{}", slug(&e.name)),
            _ => format!("app:{}", slug(&p.name)),
        };
        let acc = apps.entry(key).or_default();
        if acc.title.is_empty() {
            acc.title = match entry {
                Some(e) if !e.family => e.name.clone(),
                _ => p.name.clone(),
            };
            acc.entry = entry;
            acc.verdict = verdict;
        } else {
            acc.verdict = combine(acc.verdict, verdict);
        }
        acc.names.insert(p.name.clone());
        acc.pids.push(p.pid);
        acc.ram += p.ram;
        acc.committed += p.committed;
        let w = input.windows.get(&p.pid).copied().unwrap_or_default();
        acc.visible += w.visible;
        acc.candidates += w.candidates;
        acc.in_self_tree |= input.self_tree.contains(&p.pid);
        acc.members.push(p);
    }

    let enabled_startup: Vec<&startup::Entry> = input.startup.iter().filter(|e| e.enabled).collect();
    let starts_with_windows =
        |names: &BTreeSet<String>| enabled_startup.iter().any(|e| names.iter().any(|n| n.eq_ignore_ascii_case(&e.exe)));

    for (id, acc) in &apps {
        let mut committed_mb = acc.committed / MB;
        let mut ram_mb = acc.ram / MB;
        let mut names: Vec<String> = acc.names.iter().cloned().collect();
        let mut pids = acc.pids.clone();
        let auto = acc.verdict.is_none();
        let verdict = match acc.verdict {
            Some(Verdict::Keep) => continue,
            Some(v) => v,
            None if committed_mb >= ds.review_above_mb => Verdict::Review,
            None => continue,
        };
        let starts = starts_with_windows(&acc.names);
        let worth_switching_off = starts && matches!(verdict, Verdict::Close | Verdict::HeavyTool);
        if committed_mb < ds.ignore_below_mb && !worth_switching_off {
            continue;
        }
        if verdict == Verdict::HoldBack {
            // Only the processes no group holds back yet compete with Unreal.
            let loose: Vec<&Proc> = acc.members.iter().copied().filter(|p| !group_has(cfg, &p.name)).collect();
            committed_mb = loose.iter().map(|p| p.committed).sum::<u64>() / MB;
            if loose.is_empty() || committed_mb < ds.ignore_below_mb {
                continue;
            }
            ram_mb = loose.iter().map(|p| p.ram).sum::<u64>() / MB;
            names = loose.iter().map(|p| p.name.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            pids = loose.iter().map(|p| p.pid).collect();
        }
        let desc = acc.entry.map(|e| e.description.clone()).unwrap_or_default();
        let described = if verdict == Verdict::Review && auto { acc.pids.first().and_then(|&pid| describe(pid)) } else { None };
        let company = described.as_ref().map(|(_, c)| c.trim().to_string()).unwrap_or_default();
        let mut detail = match verdict {
            Verdict::Review if auto => match described {
                Some((d, c)) if !d.is_empty() && !c.is_empty() => {
                    format!("{} from {}. Ather MemGuard doesn't know this app; check what it is before you close it.", d, c)
                }
                Some((d, c)) => format!(
                    "{}. Ather MemGuard doesn't know this app; check what it is before you close it.",
                    if d.is_empty() { c } else { d }
                ),
                None => "Ather MemGuard doesn't know this app, and it's using a lot of memory. Check what it is before you close it."
                    .to_string(),
            },
            Verdict::HoldBack => {
                format!("{} It isn't in a MemGuard group yet, so it competes with Unreal for memory.", desc)
            }
            _ => desc,
        };
        if verdict == Verdict::Close && acc.visible == 0 {
            detail.push_str(" It's running in the background with no window open.");
        }
        if starts {
            detail.push_str(" It starts with Windows.");
        }
        let group = match verdict {
            Verdict::HoldBack => Some(acc.entry.and_then(|e| e.group.clone()).unwrap_or_else(|| "Background".into())),
            Verdict::Review => Some("Background".to_string()),
            _ => None,
        };
        let mut actions = Vec::new();
        let blocked_already = names.iter().all(|n| any_matches(&ds.close_while_unreal, n));
        match verdict {
            Verdict::Close | Verdict::HeavyTool if !acc.in_self_tree => {
                actions.push(Action::new(ActionKind::Close, None));
                if !blocked_already {
                    actions.push(Action::new(ActionKind::BlockWhileUnreal, None));
                }
                if verdict == Verdict::Close && acc.visible == 0 && acc.candidates == 0 {
                    actions.push(Action::new(ActionKind::End, None));
                }
            }
            Verdict::Review if !acc.in_self_tree => {
                actions.push(Action::new(ActionKind::Close, None));
                actions.push(Action::new(ActionKind::HoldBack, group.as_deref()));
                actions.push(Action::new(ActionKind::LeaveIt, None));
            }
            Verdict::Review => actions.push(Action::new(ActionKind::LeaveIt, None)),
            Verdict::HoldBack => actions.push(Action::new(ActionKind::HoldBack, group.as_deref())),
            _ => {}
        }
        if acc.in_self_tree && verdict != Verdict::HoldBack {
            detail.push_str(" Ather MemGuard was started from this app, so it won't close it.");
        }
        let recommended = verdict == Verdict::Close
            && !acc.in_self_tree
            && acc.visible == 0
            && acc.candidates > 0
            && actions.first().map(|a| a.kind) == Some(ActionKind::Close);
        let mut f = Finding::new(id.clone(), FindingKind::App, verdict, acc.title.clone(), detail.trim().to_string());
        f.processes = pids.len();
        f.ram_mb = ram_mb;
        f.committed_mb = committed_mb;
        f.actions = actions;
        f.recommended = recommended;
        f.category = acc.entry.map(|e| e.category.clone()).unwrap_or_default();
        f.names = names;
        f.pids = pids;
        f.has_window = acc.visible > 0;
        f.group = group;
        f.company = company;
        findings.push(f);
    }

    // ---- startup entries
    for e in &enabled_startup {
        if e.location == Location::HkcuRun && e.name.eq_ignore_ascii_case(paths::RUN_VALUE) {
            continue;
        }
        let (verdict, entry) = classify(ds, cat, &e.exe);
        let verdict = match verdict {
            Some(v @ (Verdict::Close | Verdict::HeavyTool)) => v,
            _ => continue,
        };
        if hard_never(&e.exe).is_some() {
            continue;
        }
        let running: Vec<&AppAcc> =
            apps.values().filter(|a| a.names.iter().any(|n| n.eq_ignore_ascii_case(&e.exe))).collect();
        let ram: u64 = running.iter().map(|a| a.ram).sum();
        let committed: u64 = running.iter().map(|a| a.committed).sum();
        let mut detail = String::from("Starts every time you sign in.");
        if let Some(entry) = entry {
            detail.push(' ');
            detail.push_str(&entry.description);
        }
        if committed > 0 {
            detail.push_str(&format!(" Uses about {}.", mem_label(committed / MB)));
        }
        let switchable = e.location.switchable();
        if !switchable {
            detail.push_str(" Changing it needs an administrator.");
        }
        let title = entry.map_or_else(|| e.name.trim_end_matches(".lnk").to_string(), |a| a.name.clone());
        let mut f = Finding::new(
            format!("startup:{}:{}", e.location.id(), e.name),
            FindingKind::Startup,
            verdict,
            title,
            detail,
        );
        f.processes = running.iter().map(|a| a.pids.len()).sum();
        f.ram_mb = ram / MB;
        f.committed_mb = committed / MB;
        if switchable {
            f.actions.push(Action::new(ActionKind::DisableStartup, None));
        }
        f.recommended = switchable && verdict == Verdict::Close;
        f.category = entry.map(|a| a.category.clone()).unwrap_or_default();
        f.names = vec![e.exe.clone()];
        f.startup = Some((*e).clone());
        findings.push(f);
    }

    // ---- Unreal leftovers
    let orphans: Vec<&Proc> = find_orphans(cfg, &input.procs)
        .into_iter()
        .filter(|p| input.facts.get(&p.pid).map_or(false, |f| f.owned && f.same_session))
        .collect();
    if !orphans.is_empty() {
        let mut f = Finding::new(
            "ue:orphans".into(),
            FindingKind::Unreal,
            Verdict::Close,
            format!("{} leftover Unreal helper{}", orphans.len(), if orphans.len() == 1 { "" } else { "s" }),
            if orphans.len() == 1 {
                "Still running after the editor, build or cook that started it closed. Nothing is using it.".into()
            } else {
                "Still running after the editor, build or cook that started them closed. Nothing is using them.".into()
            },
        );
        f.processes = orphans.len();
        f.ram_mb = orphans.iter().map(|p| p.ram).sum::<u64>() / MB;
        f.committed_mb = orphans.iter().map(|p| p.committed).sum::<u64>() / MB;
        f.actions.push(Action::new(ActionKind::End, None));
        f.names = orphans.iter().map(|p| p.name.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        f.pids = orphans.iter().map(|p| p.pid).collect();
        findings.push(f);
    }
    let editors: Vec<&UeProcess> = input.ue.iter().filter(|p| p.kind == "editor").collect();
    if editors.len() > 1 {
        let committed_gb: f64 = editors.iter().map(|p| p.committed_gb).sum();
        let mut f = Finding::new(
            "ue:editors".into(),
            FindingKind::Unreal,
            Verdict::Review,
            format!("{} Unreal editors are open", editors.len()),
            format!("Together they hold {:.0} GB. Close the ones you aren't using.", committed_gb),
        );
        f.processes = editors.len();
        f.committed_mb = (committed_gb * 1024.0) as u64;
        f.ram_mb = (editors.iter().map(|p| p.ram_gb).sum::<f64>() * 1024.0) as u64;
        f.pids = editors.iter().map(|p| p.pid).collect();
        findings.push(f);
    }
    for p in &editors {
        if p.committed_gb >= cfg.ue.editor_restart_hint_gb {
            let mut f = Finding::new(
                format!("ue:restart:{}", p.pid),
                FindingKind::Unreal,
                Verdict::Review,
                format!("Restart {} to get memory back", p.name),
                format!(
                    "It holds {:.0} GB after {} h {} min. Save your work and restart it to free that memory.",
                    p.committed_gb,
                    p.uptime_min / 60,
                    p.uptime_min % 60
                ),
            );
            f.processes = 1;
            f.committed_mb = (p.committed_gb * 1024.0) as u64;
            f.ram_mb = (p.ram_gb * 1024.0) as u64;
            f.pids = vec![p.pid];
            findings.push(f);
        }
    }

    // ---- this PC
    let system = SystemStatus::from(&input.sys);
    let mut sys_finding = |id: &str, title: String, detail: String| {
        findings.push(Finding::new(format!("sys:{}", id), FindingKind::System, Verdict::Review, title, detail));
    };
    if input.sys.commit_limit > 0 && (input.sys.commit_limit as f64) < input.sys.ram_total as f64 * 1.4 {
        sys_finding(
            "pagefile",
            "The page file is small for this much RAM".into(),
            format!(
                "Windows can promise apps {:.0} GB on {:.0} GB of RAM. A larger fixed page file gives Unreal room to allocate.",
                system.commit_limit_gb, system.ram_total_gb
            ),
        );
    }
    if input.sys.commit_limit > 0 && system.commit_free_pct < cfg.pressure.elevated_commit_free_pct {
        sys_finding(
            "commit",
            "Windows is close to its memory limit".into(),
            format!(
                "Only {:.1} GB of commit is free. When it runs out, apps fail even if RAM is free.",
                system.commit_free_gb
            ),
        );
    }
    if input.sys.nonpaged_pool > 4 * 1024 * MB {
        sys_finding(
            "nonpaged",
            "A driver is holding a lot of memory".into(),
            format!(
                "The non-paged pool is {:.1} GB. That usually means a driver is leaking; updating or restarting fixes it.",
                system.nonpaged_pool_gb
            ),
        );
    }
    let git = input.procs.iter().filter(|p| p.name.eq_ignore_ascii_case("git")).count();
    if git >= 20 {
        sys_finding(
            "git",
            format!("{} git processes are running", git),
            "That usually means a stuck poller is piling them up. Close the app that keeps starting them.".into(),
        );
    }
    if !input.memguard_running {
        sys_finding(
            "memguard",
            "Ather MemGuard isn't running".into(),
            "Nothing is holding apps back for Unreal. Start Ather MemGuard from the Start menu.".into(),
        );
    } else if input.memguard_paused {
        sys_finding(
            "memguard",
            "Ather MemGuard is paused".into(),
            "Apps aren't being held back for Unreal. Resume it from the tray menu.".into(),
        );
    }

    findings.sort_by(|a, b| section_rank(a).cmp(&section_rank(b)).then(b.committed_mb.cmp(&a.committed_mb)));
    let (ram, committed) = reclaimable(&findings);
    Report {
        time: input.time.clone(),
        system,
        unreal_active: input.unreal_active,
        findings,
        reclaimable_ram_gb: gb(ram),
        reclaimable_commit_gb: gb(committed),
    }
}

/// RAM and committed memory (MB) that closing the "not needed" apps and ending Unreal
/// leftovers would free. Startup entries aren't counted again (their app is), and
/// heavy tools and unknown apps aren't counted (they're probably deliberate).
pub fn reclaimable(findings: &[Finding]) -> (u64, u64) {
    findings
        .iter()
        .filter(|f| (f.kind == FindingKind::App && f.verdict == Verdict::Close) || f.id == "ue:orphans")
        .fold((0, 0), |(r, c), f| (r + f.ram_mb, c + f.committed_mb))
}

// ---------------------------------------------------------------- picks

/// Parse `<id>[:action]`. Ids contain colons (`startup:hkcu:Steam`), so the action is
/// recognised only when the last part is an action name. Without one, the finding's first
/// proposed action is used.
pub fn parse_pick(report: &Report, s: &str) -> Result<(String, ActionKind), String> {
    let (id, action) = match s.rsplit_once(':') {
        Some((id, a)) if ActionKind::from_name(a).is_some() && report.finding(id).is_some() => {
            (id.to_string(), ActionKind::from_name(a))
        }
        _ => (s.to_string(), None),
    };
    let f = report.finding(&id).ok_or_else(|| format!("No finding \"{}\" in this checkup.", id))?;
    match action.or_else(|| f.first_action()) {
        Some(a) => Ok((id, a)),
        None => Err(format!("\"{}\" has nothing to do; it's for information.", id)),
    }
}

// ---------------------------------------------------------------- settings edits

/// Add names to a `[doctor]` list. Returns the names actually added.
pub fn doc_add_to_list(doc: &mut toml_edit::DocumentMut, key: &str, names: &[String]) -> Result<Vec<String>, String> {
    let table = doc.entry("doctor").or_insert(toml_edit::table()).as_table_mut().ok_or("[doctor] is not a table")?;
    let arr = table
        .entry(key)
        .or_insert(toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .ok_or_else(|| format!("doctor.{} is not a list", key))?;
    let mut added = Vec::new();
    for n in names {
        let present: Vec<String> = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        if !any_matches(&present, n) {
            arr.push(n.as_str());
            added.push(n.clone());
        }
    }
    Ok(added)
}

/// Remove exact names (case-insensitive) from a `[doctor]` list. A list left empty is
/// removed, and so is a `[doctor]` table left empty, so undo leaves the file as it was.
pub fn doc_remove_from_list(doc: &mut toml_edit::DocumentMut, key: &str, names: &[String]) {
    let Some(table) = doc.get_mut("doctor").and_then(|t| t.as_table_like_mut()) else { return };
    let mut now_empty = false;
    if let Some(arr) = table.get_mut(key).and_then(|a| a.as_array_mut()) {
        arr.retain(|v| !v.as_str().map_or(false, |s| names.iter().any(|n| n.eq_ignore_ascii_case(s))));
        now_empty = arr.is_empty();
    }
    if now_empty {
        table.remove(key);
    }
    if table.is_empty() {
        doc.remove("doctor");
    }
}

/// Add names to the `[[group]]` called `group`. If the file has no such group, one is
/// appended with `base` (the group's current process list) plus the names, so the result
/// works whether groups merge by name or replace.
pub fn doc_add_to_group(
    doc: &mut toml_edit::DocumentMut,
    group: &str,
    names: &[String],
    base: &[String],
) -> Result<Vec<String>, String> {
    if doc.get("group").is_none() {
        doc.insert("group", toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    }
    let groups = doc.get_mut("group").and_then(|g| g.as_array_of_tables_mut()).ok_or("group is not a list of [[group]] tables")?;
    let idx = groups.iter().position(|t| t.get("name").and_then(|v| v.as_str()) == Some(group));
    let table = match idx {
        Some(i) => groups.get_mut(i).ok_or("group vanished")?,
        None => {
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value(group));
            let mut arr = toml_edit::Array::new();
            for b in base {
                arr.push(b.as_str());
            }
            t.insert("processes", toml_edit::value(arr));
            if base.is_empty() {
                t.insert("priority", toml_edit::value("low"));
            }
            groups.push(t);
            groups.get_mut(groups.len() - 1).ok_or("group vanished")?
        }
    };
    let arr = table
        .entry("processes")
        .or_insert(toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .ok_or_else(|| format!("group {}: processes is not a list", group))?;
    let mut added = Vec::new();
    for n in names {
        let present: Vec<String> = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        if !any_matches(&present, n) {
            arr.push(n.as_str());
            added.push(n.clone());
        }
    }
    Ok(added)
}

/// What asking one app to close did (the flyout's "Close {app}").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseApp {
    /// The checkup doesn't offer to close it: Windows or Unreal may need it, or it's kept.
    NotOffered,
    /// The checkup's message for each outcome ("Closed", "Still open — it may be asking you to
    /// save", or why not).
    Closed(String),
    StillOpen(String),
    NotClosed(String),
}

/// Ask the app `exe` (executable name, no `.exe`) to close through the checkup's rules: only
/// an app finding that offers Close, never forced, logged for Undo. Scans first.
pub fn close_app(exe: &str) -> CloseApp {
    let report = scan(&load_config_readonly());
    let found = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::App && f.offers(ActionKind::Close) && f.names.iter().any(|n| n.eq_ignore_ascii_case(exe)));
    let Some(f) = found else { return CloseApp::NotOffered };
    match apply(&report, &[(f.id.clone(), ActionKind::Close)], false).into_iter().next() {
        Some(o) if o.result == OutcomeKind::Done => CloseApp::Closed(o.message),
        Some(o) if o.result == OutcomeKind::StillOpen => CloseApp::StillOpen(o.message),
        Some(o) => CloseApp::NotClosed(o.message),
        None => CloseApp::NotClosed("Nothing happened".into()),
    }
}

/// What ending the Unreal leftovers did (the flyout's "End leftover Unreal helpers").
#[derive(Debug, Clone, PartialEq)]
pub enum EndLeftovers {
    NoneFound,
    Ended { processes: usize, ram_gb: f64 },
    /// Some are still running: why.
    NotAll(String),
}

/// End the checkup's "ue:orphans" finding: Unreal helpers whose editor, build or cook has
/// exited. Forced, so the caller asks the person first. Scans first.
pub fn end_leftovers() -> EndLeftovers {
    let report = scan(&load_config_readonly());
    let Some(f) = report.findings.iter().find(|f| f.id == "ue:orphans") else { return EndLeftovers::NoneFound };
    let outs = apply(&report, &[(f.id.clone(), ActionKind::End)], true);
    match outs.into_iter().find(|o| o.result != OutcomeKind::Done) {
        None => EndLeftovers::Ended { processes: f.processes, ram_gb: f.ram_mb as f64 / 1024.0 },
        Some(o) => EndLeftovers::NotAll(o.message),
    }
}

/// Add `app` (executable name, no `.exe`) to the app group `group` in settings.toml: the
/// flyout's "Hold it back with". Returns false when it was already in the group.
pub fn add_to_group(app: &str, group: &str) -> Result<bool, String> {
    let cfg = load_config_readonly();
    let base: Vec<String> = cfg.groups.iter().find(|g| g.name == group).map(|g| g.processes.clone()).unwrap_or_default();
    edit_settings(|doc| doc_add_to_group(doc, group, &[app.to_string()], &base)).map(|added| !added.is_empty())
}

pub fn doc_remove_from_group(doc: &mut toml_edit::DocumentMut, group: &str, names: &[String]) {
    let Some(groups) = doc.get_mut("group").and_then(|g| g.as_array_of_tables_mut()) else { return };
    for t in groups.iter_mut() {
        if t.get("name").and_then(|v| v.as_str()) == Some(group) {
            if let Some(arr) = t.get_mut("processes").and_then(|a| a.as_array_mut()) {
                arr.retain(|v| !v.as_str().map_or(false, |s| names.iter().any(|n| n.eq_ignore_ascii_case(s))));
            }
        }
    }
}

/// Remove the user's `[[group]]` entry for `group` if all it holds is a name and the same
/// process list the lower layers already give (or an empty list). Such an entry was created
/// by a hold-back and changes nothing; leaving it would pin the list against later updates.
pub fn doc_drop_redundant_group(doc: &mut toml_edit::DocumentMut, group: &str, below: &[String]) {
    let Some(groups) = doc.get_mut("group").and_then(|g| g.as_array_of_tables_mut()) else { return };
    let redundant = |t: &toml_edit::Table| {
        if t.get("name").and_then(|v| v.as_str()) != Some(group) {
            return false;
        }
        let only_ours = t.iter().all(|(k, _)| k == "name" || k == "processes" || (below.is_empty() && k == "priority"));
        let listed: Vec<String> = t
            .get("processes")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        only_ours && (listed.is_empty() || listed == below)
    };
    let before = groups.len();
    groups.retain(|t| !redundant(t));
    if before > 0 && groups.is_empty() {
        doc.remove("group");
    }
}

/// Check an edited settings text before writing it: valid TOML, a valid `[doctor]` table,
/// and everything else still accepted by `Config`.
pub fn validate_settings(text: &str) -> Result<(), String> {
    let mut table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    DoctorSettings::from_settings_text(text)?;
    // `[doctor]` is read by the doctor itself; until the integrator adds it to `Config`'s
    // schema, check the rest without it.
    table.remove("doctor");
    let rest = toml::to_string(&table).map_err(|e| e.to_string())?;
    Config::parse(&rest).map(|_| ())
}

/// Read settings.toml, apply `edit`, validate, and write it back atomically. Comments and
/// layout are kept.
fn edit_settings<T>(edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, String>) -> Result<T, String> {
    let path = paths::config_file();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::config::DEFAULT_TOML.to_string(),
        Err(e) => return Err(format!("{}: {}", path.display(), e)),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {}", path.display(), e))?;
    let result = edit(&mut doc)?;
    let new_text = doc.to_string();
    if new_text == text {
        return Ok(result);
    }
    validate_settings(&new_text).map_err(|e| format!("the change would break the settings file: {}", e))?;
    fs::create_dir_all(paths::data_dir()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.doctor-tmp");
    fs::write(&tmp, &new_text).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(result)
}

// ---------------------------------------------------------------- undo log

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relaunch {
    pub exe: String,
    /// The full command line it was started with, when it could be read.
    #[serde(default)]
    pub command_line: String,
}

/// What undoing one applied action takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoStep {
    /// Start the closed app again.
    Relaunch { apps: Vec<Relaunch> },
    /// Put the startup entry's `StartupApproved` value back.
    Startup { location: Location, name: String, previous: Option<Vec<u8>> },
    /// Remove names added to a MemGuard group.
    Group { group: String, added: Vec<String> },
    /// Remove names added to a `[doctor]` list.
    DoctorList { key: String, added: Vec<String> },
    /// Can't be undone (ended processes).
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UndoEntry {
    /// `<run id>-<n>`.
    pub id: String,
    pub finding_id: String,
    pub title: String,
    pub action: ActionKind,
    pub step: UndoStep,
    #[serde(default)]
    pub undone: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UndoRun {
    pub id: String,
    pub time: String,
    pub entries: Vec<UndoEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UndoLog {
    pub runs: Vec<UndoRun>,
}

impl UndoLog {
    pub fn path() -> std::path::PathBuf {
        paths::data_dir().join("doctor-undo.json")
    }

    pub fn parse(text: &str) -> UndoLog {
        serde_json::from_str(text).unwrap_or_default()
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{\"runs\":[]}".into())
    }

    pub fn load() -> UndoLog {
        fs::read_to_string(UndoLog::path()).map(|t| UndoLog::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = UndoLog::path();
        fs::create_dir_all(paths::data_dir()).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.to_text()).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    pub fn push_run(&mut self, run: UndoRun) {
        if run.entries.is_empty() {
            return;
        }
        self.runs.push(run);
        if self.runs.len() > UNDO_KEEP_RUNS {
            let extra = self.runs.len() - UNDO_KEEP_RUNS;
            self.runs.drain(..extra);
        }
    }

    /// The entries an undo should reverse, as (run, entry) indexes, newest first. One entry
    /// by id, or every not-yet-undone entry of the latest run that has any.
    pub fn select(&self, last_run: bool, action_id: Option<&str>) -> Vec<(usize, usize)> {
        if let Some(id) = action_id {
            for (r, run) in self.runs.iter().enumerate() {
                if let Some(e) = run.entries.iter().position(|e| e.id == id) {
                    return vec![(r, e)];
                }
            }
            return Vec::new();
        }
        if !last_run {
            return Vec::new();
        }
        for (r, run) in self.runs.iter().enumerate().rev() {
            let open: Vec<(usize, usize)> = run
                .entries
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, e)| !e.undone && e.step != UndoStep::Nothing)
                .map(|(e, _)| (r, e))
                .collect();
            if !open.is_empty() {
                return open;
            }
        }
        Vec::new()
    }

    /// Is there anything `undo(true, None)` would reverse?
    pub fn can_undo(&self) -> bool {
        !self.select(true, None).is_empty()
    }
}

fn run_id() -> String {
    let t = win::local_time_string(); // "2026-11-14 15:30:12"
    let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("{}-{}", digits, win::now_filetime() % 1000)
}

// ---------------------------------------------------------------- apply

struct PendingClose {
    outcome_index: usize,
    handles: Vec<win::Handle>,
    title: String,
    undo: UndoEntry,
}

/// Run the picked actions. Every "never act on" rule is checked again here, against the
/// processes running now. `end` needs `force`. Applied actions go to the undo log.
pub fn apply(report: &Report, picks: &[(String, ActionKind)], force: bool) -> Vec<Outcome> {
    run_picks(report, picks, force, false)
}

/// What `apply` would do, with the same checks, without doing it.
pub fn dry_run(report: &Report, picks: &[(String, ActionKind)], force: bool) -> Vec<Outcome> {
    run_picks(report, picks, force, true)
}

/// Settings for the "never act on" checks, read without writing anything (unlike
/// `Config::load`, which creates a missing settings file). Broken or missing = built-in.
pub fn load_config_readonly() -> Config {
    // The effective settings: built-in defaults, then the team file, then the user's file.
    // Read without `Config::load`, which would create or migrate the settings file.
    let user = fs::read_to_string(paths::config_file()).unwrap_or_default();
    let team = fs::read_to_string(crate::config::team_file()).ok();
    crate::config::config_from_layers(team.as_deref(), &user).map(|(cfg, _)| cfg).unwrap_or_else(|_| Config::builtin())
}

fn act_context<'a>(cfg: &'a Config, ds: &'a DoctorSettings, procs: &[Proc]) -> ActContext<'a> {
    ActContext { cfg, ds, self_tree: self_tree(procs, win::current_pid()), facts: facts_for(procs) }
}

/// Is this finding's action allowed at all (before looking at processes)?
fn check_pick(f: &Finding, action: ActionKind, force: bool) -> Result<(), String> {
    if f.verdict == Verdict::Keep {
        return Err("It's on the keep list.".into());
    }
    let listed = f.offers(action);
    let end_anyway = action == ActionKind::End && f.kind == FindingKind::App && f.offers(ActionKind::Close);
    if !listed && !end_anyway {
        return Err(format!("\"{}\" isn't offered for this item.", action.name()));
    }
    if action == ActionKind::End && !force {
        return Err("Ending needs --force: anything unsaved in it is lost.".into());
    }
    if action == ActionKind::BlockWhileUnreal && !matches!(f.verdict, Verdict::Close | Verdict::HeavyTool) {
        return Err("Only apps Unreal doesn't need can be kept closed.".into());
    }
    Ok(())
}

/// The finding's processes that are still running now, and the reasons any are refused.
fn live_targets<'a>(
    f: &Finding,
    procs: &'a [Proc],
    ctx: &ActContext,
    cfg: &Config,
) -> (Vec<&'a Proc>, Vec<String>) {
    let orphan_pids: HashSet<u32> =
        if f.id == "ue:orphans" { find_orphans(cfg, procs).iter().map(|p| p.pid).collect() } else { HashSet::new() };
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    for &pid in &f.pids {
        let Some(p) = procs.iter().find(|p| p.pid == pid) else { continue };
        if !f.names.iter().any(|n| n.eq_ignore_ascii_case(&p.name)) {
            continue; // PID reused by something else
        }
        if f.id == "ue:orphans" && !orphan_pids.contains(&pid) {
            refused.push(format!("{} (PID {}) has a parent again.", p.name, pid));
            continue;
        }
        match refusal(ctx, p, f.id == "ue:orphans") {
            Some(why) => refused.push(why),
            None => ok.push(p),
        }
    }
    (ok, refused)
}

/// The command lines to start an app again: its root processes (whose parent isn't part
/// of the same app).
fn relaunch_info(targets: &[&Proc]) -> Vec<Relaunch> {
    let pids: HashSet<u32> = targets.iter().map(|p| p.pid).collect();
    let mut out: Vec<Relaunch> = Vec::new();
    for p in targets.iter().filter(|p| !pids.contains(&p.ppid)) {
        if let Some(exe) = doctor_win::image_path(p.pid) {
            let command_line = win::command_line(p.pid).unwrap_or_default();
            if !out.iter().any(|r| r.exe.eq_ignore_ascii_case(&exe) && r.command_line == command_line) {
                out.push(Relaunch { exe, command_line });
            }
        }
    }
    out
}

fn run_picks(report: &Report, picks: &[(String, ActionKind)], force: bool, dry: bool) -> Vec<Outcome> {
    let cfg = load_config_readonly();
    let ds = DoctorSettings::load();
    let procs = win::list_processes();
    let ctx = act_context(&cfg, &ds, &procs);
    let windows = doctor_win::top_level_windows();
    let run = run_id();
    let mut entries: Vec<UndoEntry> = Vec::new();
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut pending: Vec<PendingClose> = Vec::new();

    for (id, action) in picks {
        let action = *action;
        let Some(f) = report.finding(id) else {
            outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, format!("No finding \"{}\" in this checkup.", id)));
            continue;
        };
        if let Err(why) = check_pick(f, action, force) {
            outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, why));
            continue;
        }
        let entry_id = format!("{}-{}", run, entries.len() + pending.len() + 1);
        let entry = |step: UndoStep| UndoEntry {
            id: entry_id.clone(),
            finding_id: f.id.clone(),
            title: f.title.clone(),
            action,
            step,
            undone: false,
        };
        match action {
            ActionKind::Close | ActionKind::End => {
                let (targets, refused) = live_targets(f, &procs, &ctx, &cfg);
                if !refused.is_empty() {
                    // One refused process refuses the whole app: closing half an app helps nobody.
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, refused[0].clone()));
                    continue;
                }
                if targets.is_empty() {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Done, "Already closed"));
                    continue;
                }
                if action == ActionKind::End {
                    if dry {
                        outcomes.push(Outcome::new(id, action, OutcomeKind::WouldDo, format!("Would end {} processes", targets.len())));
                        continue;
                    }
                    let ended = targets.iter().filter(|p| win::terminate(p.pid)).count();
                    let kind = if ended == targets.len() { OutcomeKind::Done } else { OutcomeKind::Failed };
                    let msg = if ended == targets.len() {
                        "Ended".to_string()
                    } else {
                        format!("Ended {} of {} processes", ended, targets.len())
                    };
                    crate::log::write(&format!("doctor: ended {} ({} of {} processes)", f.title, ended, targets.len()));
                    let mut o = Outcome::new(id, action, kind, msg);
                    if ended > 0 {
                        o.undo_id = None;
                        entries.push(entry(UndoStep::Nothing));
                    }
                    outcomes.push(o);
                    continue;
                }
                let pids: HashSet<u32> = targets.iter().map(|p| p.pid).collect();
                let visible: Vec<isize> = windows.iter().filter(|w| pids.contains(&w.pid) && w.visible).map(|w| w.hwnd).collect();
                let hwnds = if visible.is_empty() {
                    windows.iter().filter(|w| pids.contains(&w.pid) && w.main_candidate).map(|w| w.hwnd).collect()
                } else {
                    visible
                };
                if hwnds.is_empty() {
                    outcomes.push(Outcome::new(
                        id,
                        action,
                        OutcomeKind::StillOpen,
                        "It has no window to ask, so it can't close by itself. End it with --force; anything unsaved is lost.",
                    ));
                    continue;
                }
                if dry {
                    outcomes.push(Outcome::new(
                        id,
                        action,
                        OutcomeKind::WouldDo,
                        format!("Would ask {} to close ({} window{})", f.title, hwnds.len(), if hwnds.len() == 1 { "" } else { "s" }),
                    ));
                    continue;
                }
                let relaunch = relaunch_info(&targets);
                let handles: Vec<win::Handle> = targets.iter().filter_map(|p| doctor_win::open_for_wait(p.pid)).collect();
                for h in &hwnds {
                    doctor_win::post_close(*h);
                }
                crate::log::write(&format!("doctor: asked {} to close ({} windows)", f.title, hwnds.len()));
                outcomes.push(Outcome::new(id, action, OutcomeKind::StillOpen, "Asked to close"));
                pending.push(PendingClose {
                    outcome_index: outcomes.len() - 1,
                    handles,
                    title: f.title.clone(),
                    undo: entry(UndoStep::Relaunch { apps: relaunch }),
                });
            }
            ActionKind::DisableStartup => {
                let Some(e) = &f.startup else {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, "This isn't a startup entry."));
                    continue;
                };
                if !e.location.switchable() {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, "Needs an administrator"));
                    continue;
                }
                if e.location == Location::HkcuRun && e.name.eq_ignore_ascii_case(paths::RUN_VALUE) {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, "It's Ather MemGuard itself."));
                    continue;
                }
                if let Some(why) = hard_never(&e.exe) {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, why));
                    continue;
                }
                if classify(&ds, catalog(), &e.exe).0 == Some(Verdict::Keep) {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, "It's on the keep list."));
                    continue;
                }
                if dry {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::WouldDo, "Would switch it off"));
                    continue;
                }
                match startup::set_enabled(e.location, &e.name, false) {
                    Ok(previous) => {
                        crate::log::write(&format!("doctor: switched off startup entry {} ({})", e.name, e.location.id()));
                        let mut o = Outcome::new(id, action, OutcomeKind::Done, "Switched off");
                        o.undo_id = Some(entry_id.clone());
                        entries.push(entry(UndoStep::Startup { location: e.location, name: e.name.clone(), previous }));
                        outcomes.push(o);
                    }
                    Err(why) => outcomes.push(Outcome::new(id, action, OutcomeKind::Failed, why)),
                }
            }
            ActionKind::HoldBack => {
                let group = f.group.clone().unwrap_or_else(|| "Background".into());
                let names: Vec<String> = f.names.iter().filter(|n| hard_never(n).is_none()).cloned().collect();
                if names.is_empty() {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, "Nothing to add."));
                    continue;
                }
                if dry {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::WouldDo, format!("Would add to {}", group)));
                    continue;
                }
                let base: Vec<String> =
                    cfg.groups.iter().find(|g| g.name == group).map(|g| g.processes.clone()).unwrap_or_default();
                match edit_settings(|doc| doc_add_to_group(doc, &group, &names, &base)) {
                    Ok(added) => {
                        let mut o = Outcome::new(id, action, OutcomeKind::Done, format!("Added to {}", group));
                        if !added.is_empty() {
                            o.undo_id = Some(entry_id.clone());
                            entries.push(entry(UndoStep::Group { group: group.clone(), added }));
                        }
                        outcomes.push(o);
                    }
                    Err(why) => outcomes.push(Outcome::new(id, action, OutcomeKind::Failed, why)),
                }
            }
            ActionKind::BlockWhileUnreal | ActionKind::LeaveIt => {
                let key = if action == ActionKind::LeaveIt { "keep" } else { "close_while_unreal" };
                let names: Vec<String> = f.names.iter().filter(|n| hard_never(n).is_none()).cloned().collect();
                if names.is_empty() {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::Refused, "Nothing to add."));
                    continue;
                }
                let done_msg =
                    if action == ActionKind::LeaveIt { "Left alone from now on" } else { "Closes whenever Unreal is running" };
                if dry {
                    outcomes.push(Outcome::new(id, action, OutcomeKind::WouldDo, format!("Would add to [doctor] {}", key)));
                    continue;
                }
                match edit_settings(|doc| doc_add_to_list(doc, key, &names)) {
                    Ok(added) => {
                        let mut o = Outcome::new(id, action, OutcomeKind::Done, done_msg);
                        if !added.is_empty() {
                            o.undo_id = Some(entry_id.clone());
                            entries.push(entry(UndoStep::DoctorList { key: key.to_string(), added }));
                        }
                        outcomes.push(o);
                    }
                    Err(why) => outcomes.push(Outcome::new(id, action, OutcomeKind::Failed, why)),
                }
            }
        }
    }

    // Wait for the apps asked to close, all together, up to CLOSE_WAIT.
    if !pending.is_empty() {
        let start = Instant::now();
        while start.elapsed() < CLOSE_WAIT && !pending.iter().all(|p| p.handles.iter().all(doctor_win::has_exited)) {
            std::thread::sleep(Duration::from_millis(200));
        }
        for p in pending {
            let gone = p.handles.iter().all(doctor_win::has_exited);
            let o = &mut outcomes[p.outcome_index];
            if gone {
                o.result = OutcomeKind::Done;
                o.message = "Closed".into();
                o.undo_id = Some(p.undo.id.clone());
                crate::log::write(&format!("doctor: {} closed", p.title));
                entries.push(p.undo);
            } else {
                o.result = OutcomeKind::StillOpen;
                o.message = "Still open — it may be asking you to save, or it went to the tray".into();
            }
        }
    }

    if !dry && !entries.is_empty() {
        let n = |e: &UndoEntry| e.id.rsplit('-').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        entries.sort_by_key(n);
        let mut log = UndoLog::load();
        log.push_run(UndoRun { id: run, time: win::local_time_string(), entries });
        if let Err(e) = log.save() {
            crate::log::write(&format!("doctor: could not write the undo log: {}", e));
        }
    }
    outcomes
}

// ---------------------------------------------------------------- undo

/// Split a command line into the program and the rest, Windows style.
fn split_command_line(cmd: &str) -> (String, String) {
    let cmd = cmd.trim_start();
    if let Some(rest) = cmd.strip_prefix('"') {
        match rest.find('"') {
            Some(i) => (rest[..i].to_string(), rest[i + 1..].trim_start().to_string()),
            None => (rest.to_string(), String::new()),
        }
    } else {
        match cmd.find(char::is_whitespace) {
            Some(i) => (cmd[..i].to_string(), cmd[i..].trim_start().to_string()),
            None => (cmd.to_string(), String::new()),
        }
    }
}

fn relaunch(r: &Relaunch) -> Result<(), String> {
    let (_, args) = split_command_line(&r.command_line);
    let mut cmd = std::process::Command::new(&r.exe);
    if !args.is_empty() {
        cmd.raw_arg(&args);
    }
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ()).map_err(|e| format!("could not start {}: {}", r.exe, e))
}

fn undo_step(step: &UndoStep) -> Result<String, String> {
    match step {
        UndoStep::Relaunch { apps } => {
            if apps.is_empty() {
                return Err("Ather MemGuard doesn't know how to start it; open it yourself.".into());
            }
            for r in apps {
                relaunch(r)?;
            }
            Ok("Started again".into())
        }
        UndoStep::Startup { location, name, previous } => {
            startup::restore(*location, name, previous.as_deref())?;
            Ok("Switched back on".into())
        }
        UndoStep::Group { group, added } => {
            // What the group lists without the user's file: if the user's entry only
            // repeats that, the entry was ours and goes away with the undo.
            let team = fs::read_to_string(crate::config::team_file()).ok();
            let below: Vec<String> = crate::config::config_from_layers(team.as_deref(), "")
                .ok()
                .and_then(|(cfg, _)| cfg.groups.into_iter().find(|g| &g.name == group).map(|g| g.processes))
                .unwrap_or_default();
            edit_settings(|doc| {
                doc_remove_from_group(doc, group, added);
                doc_drop_redundant_group(doc, group, &below);
                Ok(())
            })?;
            Ok(format!("Removed from {}", group))
        }
        UndoStep::DoctorList { key, added } => {
            edit_settings(|doc| {
                doc_remove_from_list(doc, key, added);
                Ok(())
            })?;
            Ok("Removed from the list".into())
        }
        UndoStep::Nothing => Err("Ended apps can't be brought back; start it again yourself.".into()),
    }
}

/// Reverse the most recent run (`last_run`), or one action by its undo id.
pub fn undo(last_run: bool, action_id: Option<&str>) -> Vec<Outcome> {
    let mut log = UndoLog::load();
    let picked = log.select(last_run, action_id);
    if picked.is_empty() {
        let (id, msg) = match action_id {
            Some(id) => (id.to_string(), format!("Nothing to undo with id \"{}\".", id)),
            None => (String::new(), "Nothing to undo.".to_string()),
        };
        return vec![Outcome::new(&id, ActionKind::Close, OutcomeKind::Refused, msg)];
    }
    let mut outcomes = Vec::new();
    for (r, e) in picked {
        let entry = log.runs[r].entries[e].clone();
        if entry.undone {
            let mut o = Outcome::new(&entry.finding_id, entry.action, OutcomeKind::Refused, "Already undone");
            o.undo_id = Some(entry.id.clone());
            outcomes.push(o);
            continue;
        }
        let mut o = match undo_step(&entry.step) {
            Ok(msg) => {
                log.runs[r].entries[e].undone = true;
                crate::log::write(&format!("doctor: undid {} on {}: {}", entry.action.name(), entry.title, msg));
                Outcome::new(&entry.finding_id, entry.action, OutcomeKind::Undone, msg)
            }
            Err(why) => Outcome::new(&entry.finding_id, entry.action, OutcomeKind::Failed, why),
        };
        o.undo_id = Some(entry.id.clone());
        outcomes.push(o);
    }
    if let Err(e) = log.save() {
        crate::log::write(&format!("doctor: could not write the undo log: {}", e));
    }
    outcomes
}

// ---------------------------------------------------------------- guard (close_while_unreal)

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuardEvent {
    /// An app on `close_while_unreal` is running while Unreal is: ask it to close
    /// (`guard_close`) and tell the person ("Closed {app}", button "Allow this time").
    Close { app: String, pids: Vec<u32> },
}

/// Watches for `[doctor] close_while_unreal` apps while the editor, a build or a cook runs.
/// Each appearance is proposed once; an app the person allowed stays allowed until Unreal
/// stops. The tray calls `on_tick` every tick and `guard_close` for each event.
#[derive(Debug, Default)]
pub struct Guard {
    patterns: Vec<String>,
    handled: HashSet<u32>,
    allowed: HashSet<String>,
}

impl Guard {
    pub fn new(patterns: Vec<String>) -> Guard {
        Guard { patterns, ..Guard::default() }
    }

    /// From `[doctor] close_while_unreal` in settings.toml.
    pub fn from_settings() -> Guard {
        Guard::new(DoctorSettings::load().close_while_unreal)
    }

    /// Settings changed.
    pub fn set_patterns(&mut self, patterns: Vec<String>) {
        self.patterns = patterns;
    }

    /// "Allow this time": leave this app alone until Unreal stops.
    pub fn allow(&mut self, app: &str) {
        self.allowed.insert(app.to_ascii_lowercase());
    }

    pub fn on_tick(&mut self, procs: &[Proc], unreal_active: bool) -> Vec<GuardEvent> {
        if !unreal_active || self.patterns.is_empty() {
            self.handled.clear();
            self.allowed.clear();
            return Vec::new();
        }
        let alive: HashSet<u32> = procs.iter().map(|p| p.pid).collect();
        self.handled.retain(|pid| alive.contains(pid));
        let mut found: BTreeMap<String, (String, Vec<u32>)> = BTreeMap::new();
        for p in procs {
            if p.pid == 0 || self.handled.contains(&p.pid) || hard_never(&p.name).is_some() {
                continue;
            }
            if !any_matches(&self.patterns, &p.name) {
                continue;
            }
            let key = p.name.to_ascii_lowercase();
            if self.allowed.contains(&key) {
                continue;
            }
            found.entry(key).or_insert_with(|| (p.name.clone(), Vec::new())).1.push(p.pid);
        }
        found
            .into_values()
            .map(|(app, pids)| {
                self.handled.extend(pids.iter().copied());
                GuardEvent::Close { app, pids }
            })
            .collect()
    }
}

/// Ask a guarded app to close: WM_CLOSE to its windows, without waiting. Every "never act
/// on" rule is checked first.
pub fn guard_close(ev: &GuardEvent) -> Outcome {
    let GuardEvent::Close { app, pids } = ev;
    let id = format!("guard:{}", slug(app));
    let cfg = load_config_readonly();
    let ds = DoctorSettings::load();
    let procs = win::list_processes();
    let ctx = act_context(&cfg, &ds, &procs);
    let mut targets = HashSet::new();
    for &pid in pids {
        let Some(p) = procs.iter().find(|p| p.pid == pid && p.name.eq_ignore_ascii_case(app)) else { continue };
        if let Some(why) = refusal(&ctx, p, false) {
            return Outcome::new(&id, ActionKind::Close, OutcomeKind::Refused, why);
        }
        targets.insert(pid);
    }
    if targets.is_empty() {
        return Outcome::new(&id, ActionKind::Close, OutcomeKind::Done, "Already closed");
    }
    let windows = doctor_win::top_level_windows();
    let mut hwnds: Vec<isize> = windows.iter().filter(|w| targets.contains(&w.pid) && w.visible).map(|w| w.hwnd).collect();
    if hwnds.is_empty() {
        hwnds = windows.iter().filter(|w| targets.contains(&w.pid) && w.main_candidate).map(|w| w.hwnd).collect();
    }
    if hwnds.is_empty() {
        return Outcome::new(&id, ActionKind::Close, OutcomeKind::StillOpen, "It has no window to ask.");
    }
    for h in &hwnds {
        doctor_win::post_close(*h);
    }
    crate::log::write(&format!("doctor guard: asked {} to close while Unreal is running", app));
    Outcome::new(&id, ActionKind::Close, OutcomeKind::Done, format!("Asked {} to close", app))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MB;

    fn proc(pid: u32, ppid: u32, name: &str, committed_mb: u64) -> Proc {
        Proc { pid, ppid, name: name.into(), ram: committed_mb * MB / 2, private: 0, committed: committed_mb * MB, created: 1000 + pid as u64, queryable: true }
    }

    fn input(procs: Vec<Proc>) -> ScanInput {
        let facts = procs.iter().map(|p| (p.pid, ProcFacts { owned: true, same_session: true })).collect();
        ScanInput {
            time: "2026-11-14 12:00:00".into(),
            sys: SysInfo {
                ram_total: 64 * GIB,
                ram_avail: 30 * GIB,
                commit_limit: 128 * GIB,
                commit_used: 40 * GIB,
                ..Default::default()
            },
            procs,
            facts,
            windows: HashMap::new(),
            startup: Vec::new(),
            ue: Vec::new(),
            unreal_active: false,
            memguard_running: true,
            memguard_paused: false,
            self_tree: HashSet::new(),
        }
    }

    fn report(inp: &ScanInput, ds: &DoctorSettings) -> Report {
        build_report(&Config::builtin(), ds, inp, &mut |_| Some(("Mystery Tool".into(), "Contoso".into())))
    }

    fn verdict_of(name: &str) -> Option<Verdict> {
        classify(&DoctorSettings::default(), catalog(), name).0
    }

    // ---- catalogue

    #[test]
    fn catalogue_parses_and_is_big_enough() {
        let cat = catalog();
        let patterns: usize = cat.iter().map(|a| a.processes.len()).sum();
        assert!(patterns >= 150, "only {} patterns", patterns);
        for v in [Verdict::Close, Verdict::HoldBack, Verdict::Keep, Verdict::Review, Verdict::HeavyTool] {
            assert!(cat.iter().any(|a| a.verdict == v), "no {:?} entries", v);
        }
        for a in cat {
            assert!(!a.description.is_empty() && !a.category.is_empty(), "{}", a.name);
            assert!(!a.processes.is_empty(), "{}", a.name);
            assert_eq!(a.verdict == Verdict::HoldBack, a.group.is_some(), "{}: group only for hold_back", a.name);
            for p in &a.processes {
                assert!(!p.to_ascii_lowercase().ends_with(".exe"), "{}: {}", a.name, p);
            }
        }
    }

    #[test]
    fn hold_back_groups_exist_in_builtin_settings() {
        let cfg = Config::builtin();
        for a in catalog().iter().filter(|a| a.verdict == Verdict::HoldBack) {
            let g = a.group.as_deref().unwrap();
            assert!(cfg.groups.iter().any(|x| x.name == g), "{} -> unknown group {}", a.name, g);
        }
    }

    #[test]
    fn things_the_user_needs_are_never_close() {
        for name in [
            "claude", "Claude", "codex", "codex-computer-use-swift", "ChatGPT", "chrome", "Slack", "Wispr Flow",
            "Wispr Flow Helper", "RustDesk", "parsecd", "tailscale-ipn", "tailscaled", "git", "git-lfs", "dotnet",
            "devenv", "ServiceHub.Host.dotnet.x64", "MSBuild", "vctip", "explorer", "Code", "rider64", "p4v",
            "WindowsTerminal", "powershell", "MsMpEng", "AtherScreenshot", "AtherMemGuard", "msedgewebview2", "node",
            "python", "UnrealGameSync", "zenserver", "EpicWebHelper", "SnippingTool", "agents-connector",
        ] {
            let v = verdict_of(name);
            assert_ne!(v, Some(Verdict::Close), "{} must never be close", name);
            assert_ne!(v, Some(Verdict::HeavyTool), "{} must never be heavy_tool", name);
        }
        for (name, v) in [
            ("claude", Verdict::HoldBack),
            ("codex", Verdict::HoldBack),
            ("chrome", Verdict::HoldBack),
            ("Slack", Verdict::HoldBack),
            ("RustDesk", Verdict::Keep),
            ("parsecd", Verdict::Keep),
            ("tailscale-ipn", Verdict::Keep),
            ("git", Verdict::Keep),
            ("devenv", Verdict::Keep),
            ("explorer", Verdict::Keep),
        ] {
            assert_eq!(verdict_of(name), Some(v), "{}", name);
        }
    }

    #[test]
    fn known_extras_are_close_or_heavy() {
        for name in ["steam", "steamwebhelper", "EpicGamesLauncher", "Spotify", "Widgets", "copilotapp", "PhoneExperienceHost", "iCUE", "ArmouryCrate", "Razer Synapse 3"] {
            assert_eq!(verdict_of(name), Some(Verdict::Close), "{}", name);
        }
        for name in ["blender", "Photoshop", "Docker Desktop", "vmmemWSL", "Unity", "obs64"] {
            assert_eq!(verdict_of(name), Some(Verdict::HeavyTool), "{}", name);
        }
        assert_eq!(verdict_of("lghub"), Some(Verdict::Review));
        assert_eq!(verdict_of("totally-unknown-app"), None);
    }

    #[test]
    fn matcher_prefers_exact_then_longest_wildcard_then_keep() {
        let cat = parse_catalog(
            r#"
            [[app]]
            name = "Wide"
            verdict = "close"
            category = "x"
            processes = ["Foo*"]
            description = "d"
            [[app]]
            name = "Narrow"
            verdict = "heavy_tool"
            category = "x"
            processes = ["FooBar*"]
            description = "d"
            [[app]]
            name = "Exact"
            verdict = "review"
            category = "x"
            processes = ["foobarbaz"]
            description = "d"
            [[app]]
            name = "TieClose"
            verdict = "close"
            category = "x"
            processes = ["tie"]
            description = "d"
            [[app]]
            name = "TieKeep"
            verdict = "keep"
            category = "x"
            processes = ["tie"]
            description = "d"
            "#,
        )
        .unwrap();
        assert_eq!(catalog_match(&cat, "fooxyz").unwrap().name, "Wide");
        assert_eq!(catalog_match(&cat, "FOOBARx").unwrap().name, "Narrow");
        assert_eq!(catalog_match(&cat, "FooBarBaz").unwrap().name, "Exact");
        assert_eq!(catalog_match(&cat, "tie").unwrap().name, "TieKeep");
        assert!(catalog_match(&cat, "fo").is_none());
    }

    #[test]
    fn user_overrides_win_keep_over_close_over_catalogue() {
        let mut ds = DoctorSettings::default();
        ds.close = vec!["mystery*".into(), "Spotify".into(), "chrome".into()];
        ds.keep = vec!["Spotify".into()];
        let cat = catalog();
        assert_eq!(classify(&ds, cat, "MysteryApp").0, Some(Verdict::Close));
        assert_eq!(classify(&ds, cat, "Spotify").0, Some(Verdict::Keep)); // keep beats close
        assert_eq!(classify(&ds, cat, "chrome").0, Some(Verdict::Close)); // close beats catalogue
        assert_eq!(classify(&ds, cat, "steam").0, Some(Verdict::Close));
        ds.keep.push("steam*".into());
        assert_eq!(classify(&ds, cat, "steamwebhelper").0, Some(Verdict::Keep));
    }

    #[test]
    fn settings_are_read_from_the_doctor_table() {
        let ds = DoctorSettings::from_settings_text("interval_sec = 10\n").unwrap();
        assert_eq!(ds, DoctorSettings::default());
        assert_eq!((ds.ignore_below_mb, ds.review_above_mb, ds.nudge_above_gb), (150, 1024, 4.0));
        let ds = DoctorSettings::from_settings_text(
            "[doctor]\nkeep = [\"Foo.exe\"]\nclose_while_unreal = [\"Spotify\"]\nignore_below_mb = 300\n",
        )
        .unwrap();
        assert_eq!(ds.keep, vec!["Foo".to_string()]);
        assert_eq!(ds.close_while_unreal, vec!["Spotify".to_string()]);
        assert_eq!(ds.ignore_below_mb, 300);
        assert_eq!(ds.review_above_mb, 1024);
        assert!(DoctorSettings::from_settings_text("[doctor]\nkeep = 3\n").is_err());
    }

    // ---- verdicts and report

    #[test]
    fn report_groups_apps_and_applies_thresholds() {
        let mut inp = input(vec![
            proc(10, 1, "steam", 300),
            proc(11, 10, "steamwebhelper", 500),
            proc(12, 10, "steamwebhelper", 400),
            proc(20, 1, "Spotify", 100), // below ignore_below_mb
            proc(30, 1, "MysteryApp", 2048),
            proc(31, 1, "SmallUnknown", 600),
            proc(40, 1, "explorer", 900),
            proc(50, 1, "UnrealEditor", 20000),
            proc(60, 1, "blender", 3000),
            proc(70, 1, "Notion", 700),
            proc(80, 1, "chrome", 4000), // already in the Browsers group
        ]);
        inp.windows.insert(60, WindowFacts { visible: 1, candidates: 1 });
        inp.windows.insert(10, WindowFacts { visible: 0, candidates: 1 });
        let r = report(&inp, &DoctorSettings::default());
        let ids: Vec<&str> = r.findings.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"app:steam"), "{:?}", ids);
        assert!(!ids.iter().any(|i| i.contains("spotify")), "{:?}", ids);
        assert!(!ids.iter().any(|i| i.contains("explorer") || i.contains("windows")), "{:?}", ids);
        assert!(!ids.iter().any(|i| i.contains("unreal")), "{:?}", ids);
        assert!(!ids.iter().any(|i| i.contains("chrome")), "{:?}", ids);
        assert!(!ids.contains(&"app:smallunknown"), "{:?}", ids);

        let steam = r.finding("app:steam").unwrap();
        assert_eq!((steam.verdict, steam.processes, steam.committed_mb), (Verdict::Close, 3, 1200));
        assert_eq!(steam.title, "Steam");
        assert!(steam.detail.contains("no window"), "{}", steam.detail);
        assert_eq!(steam.first_action(), Some(ActionKind::Close));
        assert!(steam.offers(ActionKind::BlockWhileUnreal));

        let mystery = r.finding("app:mysteryapp").unwrap();
        assert_eq!(mystery.verdict, Verdict::Review);
        assert!(mystery.detail.contains("Mystery Tool from Contoso"), "{}", mystery.detail);
        assert_eq!(mystery.company, "Contoso");
        assert!(!mystery.recommended);
        assert!(mystery.offers(ActionKind::LeaveIt) && !mystery.offers(ActionKind::BlockWhileUnreal));

        let blender = r.finding("app:blender").unwrap();
        assert_eq!(blender.verdict, Verdict::HeavyTool);
        assert!(!blender.recommended);

        let notion = r.finding("app:notion").unwrap();
        assert_eq!(notion.verdict, Verdict::HoldBack);
        assert_eq!(notion.group.as_deref(), Some("Chat"));
        assert_eq!(notion.actions.iter().map(|a| a.kind).collect::<Vec<_>>(), vec![ActionKind::HoldBack]);

        // Sections in order: close, heavy, review, hold back.
        let pos = |id: &str| ids.iter().position(|i| *i == id).unwrap();
        assert!(pos("app:steam") < pos("app:blender"));
        assert!(pos("app:blender") < pos("app:mysteryapp"));
        assert!(pos("app:mysteryapp") < pos("app:notion"));
    }

    #[test]
    fn hold_back_counts_only_processes_no_group_holds() {
        let inp = input(vec![
            proc(10, 1, "chrome", 4000),             // in Browsers
            proc(11, 10, "chrome-native-host", 5),   // not in a group, but tiny
            proc(20, 1, "codex", 300),               // in Agents
            proc(21, 20, "codex-computer-use-swift", 400), // not in a group
            proc(30, 1, "agents-connector", 430),    // family entry: its own finding
            proc(31, 1, "Perplexity", 200),
        ]);
        let r = report(&inp, &DoctorSettings::default());
        assert!(r.finding("app:google-chrome").is_none());
        let codex = r.finding("app:codex").unwrap();
        assert_eq!((codex.processes, codex.committed_mb), (1, 400));
        assert_eq!(codex.names, vec!["codex-computer-use-swift".to_string()]);
        assert_eq!(codex.group.as_deref(), Some("Agents"));
        let a = r.finding("app:agents-connector").unwrap();
        assert_eq!(a.title, "agents-connector");
        assert_eq!(a.category, "Agent app");
        assert!(r.finding("app:perplexity").is_some());
    }

    #[test]
    fn other_users_and_sessions_are_left_out() {
        let mut inp = input(vec![proc(10, 1, "steam", 900), proc(11, 1, "Spotify", 900), proc(12, 1, "Battle.net", 900)]);
        inp.facts.insert(10, ProcFacts { owned: false, same_session: true });
        inp.facts.insert(11, ProcFacts { owned: true, same_session: false });
        let r = report(&inp, &DoctorSettings::default());
        assert!(r.finding("app:steam").is_none());
        assert!(r.finding("app:spotify").is_none());
        assert!(r.finding("app:battle.net").is_some());
    }

    #[test]
    fn recommended_only_for_windowless_close_and_startup_off() {
        let mut inp = input(vec![
            proc(10, 1, "steam", 900),          // hidden window only -> recommended close
            proc(20, 1, "Spotify", 900),        // visible window -> not recommended
            proc(30, 1, "EpicGamesLauncher", 900), // no window at all -> close can't work, not recommended
            proc(40, 1, "blender", 900),        // heavy tool, hidden -> not recommended
        ]);
        inp.windows.insert(10, WindowFacts { visible: 0, candidates: 2 });
        inp.windows.insert(20, WindowFacts { visible: 1, candidates: 1 });
        inp.windows.insert(40, WindowFacts { visible: 0, candidates: 1 });
        inp.startup = vec![
            startup::Entry { location: Location::HkcuRun, name: "Steam".into(), command: "steam.exe".into(), exe: "steam".into(), enabled: true },
            startup::Entry { location: Location::HklmRun, name: "Spotify".into(), command: "Spotify.exe".into(), exe: "Spotify".into(), enabled: true },
            startup::Entry { location: Location::HkcuRun, name: "Blender".into(), command: "blender.exe".into(), exe: "blender".into(), enabled: true },
            startup::Entry { location: Location::UserFolder, name: "Old.lnk".into(), command: "x".into(), exe: "steam".into(), enabled: false },
            startup::Entry { location: Location::HkcuRun, name: "Git".into(), command: "git.exe".into(), exe: "git".into(), enabled: true },
        ];
        let r = report(&inp, &DoctorSettings::default());
        assert!(r.finding("app:steam").unwrap().recommended);
        assert!(!r.finding("app:spotify").unwrap().recommended);
        let epic = r.finding("app:epic-games-launcher").unwrap();
        assert!(!epic.recommended);
        assert!(epic.offers(ActionKind::End));
        assert!(epic.actions.iter().find(|a| a.kind == ActionKind::End).unwrap().needs_force);
        assert!(!r.finding("app:blender").unwrap().recommended);

        let s = r.finding("startup:hkcu:Steam").unwrap();
        assert!(s.recommended);
        assert_eq!(s.first_action(), Some(ActionKind::DisableStartup));
        assert!(s.detail.contains("Uses about"), "{}", s.detail);
        let hklm = r.finding("startup:hklm:Spotify").unwrap();
        assert!(!hklm.recommended && hklm.actions.is_empty());
        assert!(hklm.detail.contains("administrator"));
        let b = r.finding("startup:hkcu:Blender").unwrap();
        assert!(!b.recommended && b.offers(ActionKind::DisableStartup));
        assert!(r.finding("startup:folder:Old.lnk").is_none(), "disabled entries are not proposed");
        assert!(r.finding("startup:hkcu:Git").is_none(), "keep entries are not proposed");

        let picks = r.recommended_picks();
        assert!(picks.contains(&("app:steam".into(), ActionKind::Close)));
        assert!(picks.contains(&("startup:hkcu:Steam".into(), ActionKind::DisableStartup)));
        assert_eq!(picks.len(), 2, "{:?}", picks);
    }

    #[test]
    fn small_apps_with_a_startup_entry_are_still_shown() {
        let mut inp = input(vec![proc(10, 1, "Spotify", 40)]);
        inp.startup = vec![startup::Entry {
            location: Location::HkcuRun,
            name: "Spotify".into(),
            command: "Spotify.exe".into(),
            exe: "Spotify".into(),
            enabled: true,
        }];
        let r = report(&inp, &DoctorSettings::default());
        assert!(r.finding("app:spotify").is_some());
        assert!(r.finding("app:spotify").unwrap().detail.contains("starts with Windows"));
    }

    #[test]
    fn self_tree_apps_get_no_close_actions() {
        let mut inp = input(vec![proc(10, 1, "steam", 900)]);
        inp.self_tree.insert(10);
        inp.windows.insert(10, WindowFacts { visible: 0, candidates: 1 });
        let r = report(&inp, &DoctorSettings::default());
        let f = r.finding("app:steam").unwrap();
        assert!(f.actions.is_empty() && !f.recommended);
        assert!(f.detail.contains("started from this app"));
    }

    #[test]
    fn reclaimable_counts_close_apps_and_leftovers_only() {
        let mut inp = input(vec![
            proc(10, 1, "steam", 1024),
            proc(20, 1, "blender", 4096),
            proc(30, 1, "MysteryApp", 2048),
            proc(40, 99999, "ShaderCompileWorker", 512), // orphan: parent 99999 doesn't exist
            proc(50, 1, "Notion", 700),
        ]);
        inp.startup = vec![startup::Entry {
            location: Location::HkcuRun,
            name: "Steam".into(),
            command: "steam.exe".into(),
            exe: "steam".into(),
            enabled: true,
        }];
        let r = report(&inp, &DoctorSettings::default());
        let orphans = r.finding("ue:orphans").unwrap();
        assert_eq!(orphans.processes, 1);
        assert_eq!(orphans.first_action(), Some(ActionKind::End));
        assert!(!orphans.recommended);
        // steam 1024 + orphan 512 committed; RAM is half of committed in the fixture.
        assert_eq!(r.reclaimable_commit_gb, 1.5);
        assert_eq!(r.reclaimable_ram_gb, 0.8);
        assert_eq!(reclaimable(&r.findings), (768, 1536));
    }

    #[test]
    fn unreal_and_system_findings() {
        let mut inp = input(vec![]);
        for i in 0..20 {
            inp.procs.push(proc(100 + i, 1, "git", 5));
        }
        inp.ue = vec![
            UeProcess { pid: 1, name: "UnrealEditor".into(), kind: "editor", committed_gb: 50.0, ram_gb: 30.0, uptime_min: 125, created: 0 },
            UeProcess { pid: 2, name: "UnrealEditor".into(), kind: "editor", committed_gb: 10.0, ram_gb: 8.0, uptime_min: 5, created: 0 },
        ];
        inp.sys.commit_limit = 70 * GIB; // < 1.4 x 64 GB RAM
        inp.sys.commit_used = 66 * GIB; // ~5.7% free
        inp.sys.nonpaged_pool = 5 * GIB;
        inp.memguard_running = false;
        let r = report(&inp, &DoctorSettings::default());
        for id in ["ue:editors", "ue:restart:1", "sys:pagefile", "sys:commit", "sys:nonpaged", "sys:git", "sys:memguard"] {
            let f = r.finding(id).unwrap_or_else(|| panic!("missing {}", id));
            assert!(f.actions.is_empty(), "{} is information only", id);
            assert!(!f.recommended);
        }
        assert!(r.finding("ue:restart:2").is_none());
        assert!(r.finding("ue:restart:1").unwrap().detail.contains("2 h 5 min"));
        inp.memguard_running = true;
        inp.memguard_paused = true;
        assert!(report(&inp, &DoctorSettings::default()).finding("sys:memguard").unwrap().title.contains("paused"));
    }

    #[test]
    fn user_close_list_turns_unknowns_into_close() {
        let inp = input(vec![proc(10, 1, "MysteryApp", 400)]);
        let mut ds = DoctorSettings::default();
        assert!(report(&inp, &ds).finding("app:mysteryapp").is_none()); // under review_above_mb
        ds.close = vec!["mystery*".into()];
        let r = report(&inp, &ds);
        assert_eq!(r.finding("app:mysteryapp").unwrap().verdict, Verdict::Close);
        ds.keep = vec!["MysteryApp".into()];
        assert!(report(&inp, &ds).finding("app:mysteryapp").is_none());
    }

    #[test]
    fn picks_parse_ids_with_colons() {
        let mut inp = input(vec![proc(10, 1, "steam", 900)]);
        inp.startup = vec![startup::Entry {
            location: Location::HkcuRun,
            name: "Steam".into(),
            command: "steam.exe".into(),
            exe: "steam".into(),
            enabled: true,
        }];
        let r = report(&inp, &DoctorSettings::default());
        assert_eq!(parse_pick(&r, "app:steam").unwrap(), ("app:steam".into(), ActionKind::Close));
        assert_eq!(parse_pick(&r, "app:steam:block_while_unreal").unwrap(), ("app:steam".into(), ActionKind::BlockWhileUnreal));
        assert_eq!(parse_pick(&r, "startup:hkcu:Steam").unwrap(), ("startup:hkcu:Steam".into(), ActionKind::DisableStartup));
        assert_eq!(
            parse_pick(&r, "startup:hkcu:Steam:disable_startup").unwrap(),
            ("startup:hkcu:Steam".into(), ActionKind::DisableStartup)
        );
        assert!(parse_pick(&r, "app:nope").is_err());
        assert!(parse_pick(&r, "sys:memguard").is_err());
    }

    // ---- never act on

    fn ctx_for<'a>(cfg: &'a Config, ds: &'a DoctorSettings, procs: &[Proc]) -> ActContext<'a> {
        ActContext {
            cfg,
            ds,
            self_tree: HashSet::new(),
            facts: procs.iter().map(|p| (p.pid, ProcFacts { owned: true, same_session: true })).collect(),
        }
    }

    #[test]
    fn never_act_on_rules() {
        let cfg = Config::builtin();
        let ds = DoctorSettings { close: vec!["explorer".into(), "MsMpEng".into(), "git".into()], ..Default::default() };
        let procs = vec![
            proc(10, 1, "steam", 1),
            proc(11, 1, "UnrealEditor", 1),
            proc(12, 1, "MsMpEng", 1),
            proc(13, 1, "explorer", 1),
            proc(14, 1, "memguard", 1),
            proc(15, 1, "Spotify", 1),
            proc(16, 1, "Battle.net", 1),
            proc(17, 1, "devenv", 1),
            proc(18, 1, "ShaderCompileWorker", 1),
            proc(19, 1, "RustDesk", 1),
        ];
        let mut ctx = ctx_for(&cfg, &ds, &procs);
        ctx.facts.insert(15, ProcFacts { owned: false, same_session: true });
        ctx.facts.insert(16, ProcFacts { owned: true, same_session: false });
        let me = proc(win::current_pid(), 1, "athermemguard-test", 1);
        let r = |p: &Proc| refusal(&ctx, p, false);
        assert_eq!(r(&procs[0]), None);
        assert!(r(&procs[1]).unwrap().contains("protected"));
        assert!(r(&procs[2]).unwrap().contains("security"), "user close can't override security");
        assert!(r(&procs[3]).unwrap().contains("Windows"), "user close can't override Windows itself");
        assert!(r(&procs[4]).unwrap().contains("MemGuard"));
        assert!(r(&procs[5]).unwrap().contains("another user"));
        assert!(r(&procs[6]).unwrap().contains("another session"));
        assert!(r(&procs[7]).unwrap().contains("keep"));
        assert!(r(&procs[9]).unwrap().contains("keep"));
        assert!(r(&me).unwrap().contains("MemGuard itself"));
        // Leftover Unreal helpers may be ended, but only as orphans.
        assert!(r(&procs[8]).is_some());
        assert_eq!(refusal(&ctx, &procs[8], true), None);
        // Self tree.
        ctx.self_tree.insert(10);
        assert!(refusal(&ctx, &procs[0], false).unwrap().contains("started from it"));
    }

    #[test]
    fn self_tree_walks_parents_and_stops_at_recycled_pids() {
        let procs = vec![
            Proc { pid: 1, ppid: 0, name: "root".into(), ram: 0, private: 0, committed: 0, created: 10, queryable: true },
            Proc { pid: 5, ppid: 1, name: "claude".into(), ram: 0, private: 0, committed: 0, created: 20, queryable: true },
            Proc { pid: 9, ppid: 5, name: "bash".into(), ram: 0, private: 0, committed: 0, created: 30, queryable: true },
            Proc { pid: 12, ppid: 9, name: "memguard".into(), ram: 0, private: 0, committed: 0, created: 40, queryable: true },
            Proc { pid: 13, ppid: 5, name: "sibling".into(), ram: 0, private: 0, committed: 0, created: 41, queryable: true },
            Proc { pid: 20, ppid: 21, name: "child-of-new".into(), ram: 0, private: 0, committed: 0, created: 50, queryable: true },
            Proc { pid: 21, ppid: 1, name: "recycled".into(), ram: 0, private: 0, committed: 0, created: 60, queryable: true },
        ];
        assert_eq!(self_tree(&procs, 12), HashSet::from([12, 9, 5, 1]));
        assert_eq!(self_tree(&procs, 20), HashSet::from([20]));
        assert_eq!(self_tree(&procs, 777), HashSet::from([777]));
    }

    #[test]
    fn check_pick_rules() {
        let mut f = Finding::new("app:x".into(), FindingKind::App, Verdict::Close, "X".into(), String::new());
        f.actions = vec![Action::new(ActionKind::Close, None)];
        assert!(check_pick(&f, ActionKind::Close, false).is_ok());
        assert!(check_pick(&f, ActionKind::End, false).unwrap_err().contains("--force"));
        assert!(check_pick(&f, ActionKind::End, true).is_ok(), "end anyway after close");
        assert!(check_pick(&f, ActionKind::DisableStartup, true).is_err());
        f.verdict = Verdict::Keep;
        assert!(check_pick(&f, ActionKind::Close, true).is_err());
        let mut h = Finding::new("app:y".into(), FindingKind::App, Verdict::HoldBack, "Y".into(), String::new());
        h.actions = vec![Action::new(ActionKind::HoldBack, Some("Chat")), Action::new(ActionKind::BlockWhileUnreal, None)];
        assert!(check_pick(&h, ActionKind::BlockWhileUnreal, false).is_err(), "never for hold_back apps");
        assert!(check_pick(&h, ActionKind::End, true).is_err());
    }

    // ---- undo log

    #[test]
    fn undo_log_round_trip_and_selection() {
        let mut log = UndoLog::default();
        let e = |id: &str, step: UndoStep, undone: bool| UndoEntry {
            id: id.into(),
            finding_id: "app:steam".into(),
            title: "Steam".into(),
            action: ActionKind::Close,
            step,
            undone,
        };
        log.push_run(UndoRun { id: "r1".into(), time: "t1".into(), entries: vec![] }); // empty runs are dropped
        assert!(log.runs.is_empty());
        log.push_run(UndoRun {
            id: "r1".into(),
            time: "t1".into(),
            entries: vec![
                e("r1-1", UndoStep::Startup { location: Location::HkcuRun, name: "Steam".into(), previous: Some(vec![2; 12]) }, false),
                e("r1-2", UndoStep::Group { group: "Chat".into(), added: vec!["Notion".into()] }, false),
            ],
        });
        log.push_run(UndoRun {
            id: "r2".into(),
            time: "t2".into(),
            entries: vec![
                e("r2-1", UndoStep::Relaunch { apps: vec![Relaunch { exe: r"C:\a.exe".into(), command_line: "\"C:\\a.exe\" -x".into() }] }, false),
                e("r2-2", UndoStep::Nothing, false),
                e("r2-3", UndoStep::DoctorList { key: "keep".into(), added: vec!["X".into()] }, true),
            ],
        });
        let text = log.to_text();
        let back = UndoLog::parse(&text);
        assert_eq!(back, log);
        assert_eq!(UndoLog::parse("not json"), UndoLog::default());

        assert_eq!(log.select(true, None), vec![(1, 0)], "latest run, skipping ended and undone entries");
        assert_eq!(log.select(false, Some("r1-2")), vec![(0, 1)]);
        assert_eq!(log.select(false, Some("nope")), Vec::<(usize, usize)>::new());
        assert!(log.select(false, None).is_empty());
        log.runs[1].entries[0].undone = true;
        assert_eq!(log.select(true, None), vec![(0, 1), (0, 0)], "falls back to the previous run, newest first");
        assert!(log.can_undo());
        for _ in 0..40 {
            log.push_run(UndoRun { id: "x".into(), time: "t".into(), entries: vec![e("x-1", UndoStep::Nothing, false)] });
        }
        assert_eq!(log.runs.len(), UNDO_KEEP_RUNS);
    }

    #[test]
    fn command_lines_split() {
        assert_eq!(split_command_line(r#""C:\Program Files\A\a.exe" -silent --x"#), (r"C:\Program Files\A\a.exe".into(), "-silent --x".into()));
        assert_eq!(split_command_line(r"C:\a.exe /bg"), (r"C:\a.exe".into(), "/bg".into()));
        assert_eq!(split_command_line(r#""C:\a.exe""#), (r"C:\a.exe".into(), String::new()));
        assert_eq!(split_command_line(""), (String::new(), String::new()));
    }

    // ---- settings edits

    #[test]
    fn settings_edits_keep_comments_and_round_trip() {
        // The built-in defaults have the Chat group; the user template is mostly comments.
        let text = format!("{}\n# my note\n", crate::config::BUILTIN_TOML);
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        let added = doc_add_to_list(&mut doc, "close_while_unreal", &["Spotify".into(), "steam".into()]).unwrap();
        assert_eq!(added.len(), 2);
        assert!(doc_add_to_list(&mut doc, "close_while_unreal", &["spotify".into()]).unwrap().is_empty(), "no duplicates");
        let added = doc_add_to_group(&mut doc, "Chat", &["Notion".into(), "Slack".into()], &[]).unwrap();
        assert_eq!(added, vec!["Notion".to_string()], "Slack is already in Chat");
        let out = doc.to_string();
        assert!(out.contains("# my note") && out.contains("# Ather MemGuard built-in defaults"));
        validate_settings(&out).unwrap();
        let ds = DoctorSettings::from_settings_text(&out).unwrap();
        assert_eq!(ds.close_while_unreal, vec!["Spotify".to_string(), "steam".to_string()]);
        let cfg = Config::parse(&{
            let mut t: toml::Table = toml::from_str(&out).unwrap();
            t.remove("doctor");
            toml::to_string(&t).unwrap()
        })
        .unwrap();
        assert!(cfg.groups.iter().find(|g| g.name == "Chat").unwrap().processes.contains(&"Notion".to_string()));

        doc_remove_from_list(&mut doc, "close_while_unreal", &["SPOTIFY".into()]);
        doc_remove_from_group(&mut doc, "Chat", &["Notion".into()]);
        let back = doc.to_string();
        let ds = DoctorSettings::from_settings_text(&back).unwrap();
        assert_eq!(ds.close_while_unreal, vec!["steam".to_string()]);
        assert!(!back.contains("\"Notion\""));
        assert!(back.contains("\"Slack\""));
    }

    #[test]
    fn group_is_created_when_the_file_lacks_it() {
        let mut doc: toml_edit::DocumentMut = "[general]\npreset = \"balanced\"\n".parse().unwrap();
        doc_add_to_group(&mut doc, "Browsers", &["Arc".into()], &["chrome".into(), "msedge".into()]).unwrap();
        let t: toml::Table = toml::from_str(&doc.to_string()).unwrap();
        let g = &t["group"].as_array().unwrap()[0];
        assert_eq!(g["name"].as_str(), Some("Browsers"));
        let procs: Vec<&str> = g["processes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(procs, vec!["chrome", "msedge", "Arc"]);
    }

    #[test]
    fn validation_rejects_broken_results() {
        assert!(validate_settings("interval_sec = \"ten\"").is_err());
        assert!(validate_settings("[doctor]\nkeep = 1").is_err());
        assert!(validate_settings("[doctor]\nkeep = [\"a\"]\n").is_ok());
        assert!(validate_settings("nonsense_key = 1\n").is_err());
    }

    // ---- guard

    #[test]
    fn guard_proposes_once_while_unreal_runs() {
        let mut g = Guard::new(vec!["Spotify".into(), "steam*".into()]);
        let procs = vec![proc(10, 1, "Spotify", 100), proc(11, 10, "Spotify", 50), proc(20, 1, "steamwebhelper", 10), proc(30, 1, "chrome", 10)];
        assert!(g.on_tick(&procs, false).is_empty(), "nothing while Unreal isn't running");
        let ev = g.on_tick(&procs, true);
        assert_eq!(ev.len(), 2);
        assert!(ev.contains(&GuardEvent::Close { app: "Spotify".into(), pids: vec![10, 11] }));
        assert!(ev.contains(&GuardEvent::Close { app: "steamwebhelper".into(), pids: vec![20] }));
        assert!(g.on_tick(&procs, true).is_empty(), "proposed once");
        // It reappears (new PID): proposed again.
        let again = vec![proc(12, 1, "Spotify", 100), proc(20, 1, "steamwebhelper", 10)];
        assert_eq!(g.on_tick(&again, true), vec![GuardEvent::Close { app: "Spotify".into(), pids: vec![12] }]);
        // Allowed this time: left alone until Unreal stops.
        g.allow("spotify");
        assert!(g.on_tick(&[proc(13, 1, "Spotify", 1)], true).is_empty());
        assert!(g.on_tick(&[proc(13, 1, "Spotify", 1)], false).is_empty());
        assert_eq!(g.on_tick(&[proc(13, 1, "Spotify", 1)], true).len(), 1, "allowance ends when Unreal stops");
    }

    #[test]
    fn guard_ignores_protected_and_empty_lists() {
        let mut g = Guard::new(vec!["*".into()]);
        let ev = g.on_tick(&[proc(10, 1, "explorer", 1), proc(11, 1, "AtherMemGuard", 1), proc(12, 1, "MsMpEng", 1)], true);
        assert!(ev.is_empty(), "{:?}", ev);
        let mut g = Guard::new(vec![]);
        assert!(g.on_tick(&[proc(10, 1, "Spotify", 1)], true).is_empty());
    }

    fn ram_report(verdict: crate::ramcheck::Verdict, problems: &[(&str, &str, &str)]) -> crate::ramcheck::RamReport {
        use crate::ramcheck::{RamFinding, RamReport, Severity};
        let mut findings: Vec<RamFinding> = problems
            .iter()
            .map(|(id, title, detail)| RamFinding { id: id.to_string(), severity: Severity::Problem, title: title.to_string(), detail: detail.to_string() })
            .collect();
        findings.push(RamFinding { id: "ram:bios".into(), severity: Severity::Caution, title: "BIOS may be out of date".into(), detail: String::new() });
        RamReport {
            verdict,
            headline: "Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz.".into(),
            summary: "The kit's fast profile (EXPO) looks switched off in the BIOS. Turning it on takes about five minutes. The steps below say how.".into(),
            sticks: Vec::new(),
            slots_total: 4,
            rated: None,
            current_mhz: Some(4800),
            current_voltage: None,
            board: Default::default(),
            cpu: Default::default(),
            findings,
            steps: Vec::new(),
            fallback_mhz: None,
            fingerprint: "f1".into(),
        }
    }

    #[test]
    fn ram_check_rows_in_this_pc() {
        use crate::ramcheck::Verdict as V;
        // Below rated: one row, the headline over the summary, information only.
        let below = ram_report(V::BelowRated, &[("ram:speed", "Your RAM is running below its rated speed", "4800 MHz now.")]);
        let rows = ram_findings(&below);
        assert_eq!(rows.len(), 1, "ram:speed is the sys:ram row itself, cautions don't show");
        let f = &rows[0];
        assert_eq!((f.id.as_str(), f.kind, f.verdict), ("sys:ram", FindingKind::System, Verdict::Review));
        assert_eq!(f.title, below.headline);
        assert_eq!(f.detail, "The kit's fast profile (EXPO) looks switched off in the BIOS. Turning it on takes about five minutes.");
        assert!(f.actions.is_empty() && !f.recommended && f.ram_mb == 0);
        assert!(is_ram_finding(&f.id));

        // Speed fine, but old Intel microcode: its own row, with its own words.
        let micro = ram_report(V::AtRated, &[("ram:microcode", "Update the BIOS before changing memory settings", "This CPU runs microcode 0x129.")]);
        let rows = ram_findings(&micro);
        assert_eq!(rows.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["sys:ram:microcode"]);
        assert_eq!(rows[0].title, "Update the BIOS before changing memory settings");
        assert_eq!(rows[0].detail, "This CPU runs microcode 0x129.");
        assert!(is_ram_finding("sys:ram:microcode") && !is_ram_finding("sys:rampage") && !is_ram_finding("sys:pagefile"));

        // Both: the speed row first, then the problem.
        let both = ram_report(V::BelowRated, &[("ram:speed", "x", "y"), ("ram:slots", "Both sticks are on the same memory channel", "Move them.")]);
        assert_eq!(ram_findings(&both).iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["sys:ram", "sys:ram:slots"]);

        // Nothing to act on: no rows (cautions and other verdicts stay in `memguard ram`).
        for v in [V::AtRated, V::NoProfile, V::MixedKit, V::Unknown, V::NotApplicable] {
            assert!(ram_findings(&ram_report(v, &[])).is_empty(), "{:?}", v);
        }

        // Added to the report's THIS PC rows only when the setting is on, and the check isn't
        // even run when it is off.
        let inp = input(vec![proc(10, 1, "steam", 900)]);
        let mut r = report(&inp, &DoctorSettings::default());
        assert!(r.findings.iter().all(|f| !is_ram_finding(&f.id)), "build_report doesn't run the check");
        add_ram_findings(&mut r, false, || -> &crate::ramcheck::RamReport { panic!("the RAM check runs only when it is on") });
        assert!(r.finding("sys:ram").is_none());
        let reclaim = (r.reclaimable_ram_gb, r.reclaimable_commit_gb);
        add_ram_findings(&mut r, true, || &below);
        assert_eq!(r.findings.last().unwrap().id, "sys:ram", "system rows come last");
        assert_eq!((r.reclaimable_ram_gb, r.reclaimable_commit_gb), reclaim, "frees nothing");
    }
}

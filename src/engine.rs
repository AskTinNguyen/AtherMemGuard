//! snapshot -> analyze (pure) -> apply.
//!
//! `analyze` turns a process snapshot into a `Status`: what Unreal is doing, how much
//! pressure memory is under, and which policy each group should get. `Engine::apply`
//! then enforces that policy. The CLI uses `analyze` alone, so `memguard status` works
//! with or without the background app running.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::config::{any_matches, limit_mb, scale_soft_cap, Config, Cpu, Group, MemPriority, Patterns, Policy, Preset};
use crate::history::History;
use crate::win::{self, Proc, SysInfo};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: u64 = 1024 * 1024;

fn gb(bytes: u64) -> f64 {
    (bytes as f64 / GB * 10.0).round() / 10.0
}

// ---------------------------------------------------------------- status model

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Normal,
    Elevated,
    Critical,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Normal => "normal",
            Level::Elevated => "elevated",
            Level::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Activities {
    pub editor: bool,
    pub build: bool,
    pub cook: bool,
}

impl Activities {
    pub fn any(&self) -> bool {
        self.editor || self.build || self.cook
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.editor {
            parts.push("editor");
        }
        if self.build {
            parts.push("build");
        }
        if self.cook {
            parts.push("cook");
        }
        if parts.is_empty() {
            "idle".to_string()
        } else {
            parts.join("+")
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemStatus {
    pub ram_total_gb: f64,
    pub ram_free_gb: f64,
    pub ram_free_pct: f64,
    pub commit_limit_gb: f64,
    pub commit_used_gb: f64,
    pub commit_free_gb: f64,
    pub commit_free_pct: f64,
    pub file_cache_gb: f64,
    pub nonpaged_pool_gb: f64,
}

impl SystemStatus {
    pub fn from(s: &SysInfo) -> SystemStatus {
        let pct = |part: u64, whole: u64| if whole == 0 { 0.0 } else { (part as f64 / whole as f64 * 1000.0).round() / 10.0 };
        SystemStatus {
            ram_total_gb: gb(s.ram_total),
            ram_free_gb: gb(s.ram_avail),
            ram_free_pct: pct(s.ram_avail, s.ram_total),
            commit_limit_gb: gb(s.commit_limit),
            commit_used_gb: gb(s.commit_used),
            commit_free_gb: gb(s.commit_free()),
            commit_free_pct: pct(s.commit_free(), s.commit_limit),
            file_cache_gb: gb(s.file_cache),
            nonpaged_pool_gb: gb(s.nonpaged_pool),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UeProcess {
    pub pid: u32,
    pub name: String,
    /// editor | cook | commandlet
    pub kind: &'static str,
    pub committed_gb: f64,
    pub ram_gb: f64,
    pub uptime_min: u64,
    /// Creation time as FILETIME, 0 if unknown. Tells a recycled pid apart.
    #[serde(skip)]
    pub created: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UeStatus {
    pub processes: Vec<UeProcess>,
    pub workers: usize,
    pub workers_committed_gb: f64,
    pub workers_paused: usize,
    /// Everything on the protect list, added up.
    pub total_committed_gb: f64,
    pub total_ram_gb: f64,
}

/// The policy a group gets right now, after mode and pressure overrides.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Effective {
    pub priority: MemPriority,
    pub trim_above_mb: u64,
    pub cpu: Cpu,
    pub suspend: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupStatus {
    pub name: String,
    pub processes: usize,
    pub ram_mb: u64,
    pub committed_mb: u64,
    pub policy: Effective,
    pub hard_cap_mb: u64,
    pub hard_cap_per_process_mb: u64,
    pub suspended: bool,
    /// Members as (pid, creation time).
    #[serde(skip)]
    pub pids: Vec<(u32, u64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub time: String,
    /// True when this status was written by the background app.
    pub daemon: bool,
    pub paused: bool,
    pub mode: String,
    pub activities: Activities,
    pub pressure: Level,
    pub system: SystemStatus,
    pub ue: UeStatus,
    pub groups: Vec<GroupStatus>,
    pub advice: Vec<String>,
    /// Free space on the watched drives (set by the CLI, the MCP server and the tray app).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<crate::disk::DiskBrief>,
    #[serde(skip)]
    pub worker_pids: Vec<(u32, u64)>,
}

pub struct Snapshot {
    pub sys: SysInfo,
    pub procs: Vec<Proc>,
    pub now: u64,
}

pub fn snapshot() -> Snapshot {
    Snapshot { sys: win::sys_info(), procs: win::list_processes(), now: win::now_filetime() }
}

// ---------------------------------------------------------------- analysis (no side effects)

fn overlay(e: &mut Effective, p: &Option<Policy>, ram_total_mb: u64) {
    if let Some(p) = p {
        if let Some(v) = p.priority {
            e.priority = v;
        }
        if let Some(v) = limit_mb(p.trim_above_mb, p.trim_above_pct, ram_total_mb) {
            e.trim_above_mb = v;
        }
        if let Some(v) = p.cpu {
            e.cpu = v;
        }
        if let Some(v) = p.suspend {
            e.suspend = v;
        }
    }
}

/// The policy a group gets now. Soft caps resolve from MB or percent of `ram_total_mb`
/// (MB wins within one table), then the preset scales them and the 128 MB floor applies.
/// The aggressive preset also lowers CPU priority (`below`) while Unreal is running, unless
/// the group already has a stronger CPU setting.
pub fn effective(g: &Group, ue_active: bool, level: Level, preset: Preset, ram_total_mb: u64) -> Effective {
    let mut e = Effective {
        priority: g.priority.unwrap_or(MemPriority::Normal),
        trim_above_mb: limit_mb(g.trim_above_mb, g.trim_above_pct, ram_total_mb).unwrap_or(0),
        cpu: g.cpu.unwrap_or(Cpu::Normal),
        suspend: g.suspend.unwrap_or(false),
    };
    if ue_active {
        overlay(&mut e, &g.ue, ram_total_mb);
    }
    if level >= Level::Elevated {
        overlay(&mut e, &g.elevated, ram_total_mb);
    }
    if level >= Level::Critical {
        overlay(&mut e, &g.critical, ram_total_mb);
    }
    e.trim_above_mb = scale_soft_cap(e.trim_above_mb, preset);
    if preset == Preset::Aggressive && ue_active && e.cpu == Cpu::Normal {
        e.cpu = Cpu::Below;
    }
    e
}

pub fn pressure_level(cfg: &Config, sys: &SysInfo) -> Level {
    if sys.ram_total == 0 || sys.commit_limit == 0 {
        return Level::Normal;
    }
    let ram = sys.ram_avail as f64 / sys.ram_total as f64 * 100.0;
    let commit = sys.commit_free() as f64 / sys.commit_limit as f64 * 100.0;
    let p = &cfg.pressure;
    if ram < p.critical_ram_free_pct || commit < p.critical_commit_free_pct {
        Level::Critical
    } else if ram < p.elevated_ram_free_pct || commit < p.elevated_commit_free_pct {
        Level::Elevated
    } else {
        Level::Normal
    }
}

/// Is this process one AtherMemGuard must never touch?
pub fn is_protected(cfg: &Config, p: &Proc) -> bool {
    p.pid == win::current_pid() || p.name.to_ascii_lowercase().contains("memguard") || any_matches(&cfg.ue.protect, &p.name)
}

/// Lowercased command lines by (pid, creation time). A process's command line doesn't
/// change, so the engine reads each one once instead of every tick.
#[derive(Default)]
pub struct CmdLines(HashMap<ProcKey, String>);

impl CmdLines {
    /// The lowercased command line of `p`, from the cache or from `read`. Empty if it
    /// can't be read; failures and empty lines are not cached (a process that just started
    /// may not have one yet), nor are processes without a creation time.
    fn get_with(&mut self, p: &Proc, read: impl FnOnce(u32) -> Option<String>) -> String {
        let key = (p.pid, p.created);
        if let Some(c) = self.0.get(&key) {
            return c.clone();
        }
        let cmd = read(p.pid).unwrap_or_default().to_ascii_lowercase();
        if p.created != 0 && !cmd.is_empty() {
            self.0.insert(key, cmd.clone());
        }
        cmd
    }

    /// Drop the processes that are gone.
    fn prune(&mut self, procs: &[Proc]) {
        if self.0.is_empty() {
            return;
        }
        let alive: HashSet<ProcKey> = procs.iter().map(|p| (p.pid, p.created)).collect();
        self.0.retain(|k, _| alive.contains(k));
    }
}

pub fn analyze(cfg: &Config, snap: &Snapshot) -> Status {
    analyze_with(cfg, snap, &mut CmdLines::default())
}

/// `analyze` with a command-line cache that lives across calls (the engine's).
pub fn analyze_with(cfg: &Config, snap: &Snapshot, cmds: &mut CmdLines) -> Status {
    cmds.prune(&snap.procs);
    // Each pattern list and each process name is lowercased once per call, not once per
    // comparison.
    let protect = Patterns::new(&cfg.ue.protect);
    let workers = Patterns::new(&cfg.ue.workers);
    let me = win::current_pid();
    let lowers: Vec<String> = snap.procs.iter().map(|p| p.name.to_ascii_lowercase()).collect();
    let protected: Vec<bool> = snap
        .procs
        .iter()
        .zip(&lowers)
        .map(|(p, lower)| p.pid == me || lower.contains("memguard") || protect.matches_lower(lower))
        .collect();

    let mut act = Activities::default();
    let mut ue = UeStatus::default();
    let mut worker_pids = Vec::new();
    let (mut ue_committed, mut ue_ram, mut workers_committed) = (0u64, 0u64, 0u64);
    let mut git_count = 0usize;

    for (p, lower) in snap.procs.iter().zip(&lowers) {
        let lower = lower.as_str();
        if lower == "git" {
            git_count += 1;
        }
        if protect.matches_lower(lower) {
            ue_committed += p.committed;
            ue_ram += p.ram;
        }
        if workers.matches_lower(lower) {
            ue.workers += 1;
            workers_committed += p.committed;
            worker_pids.push((p.pid, p.created));
        }
        if lower.starts_with("unrealeditor") {
            let cmd = cmds.get_with(p, win::command_line);
            let kind = if cmd.contains("-run=cook") {
                act.cook = true;
                "cook"
            } else if lower.contains("-cmd") || cmd.contains("-run=") {
                "commandlet"
            } else {
                act.editor = true;
                "editor"
            };
            ue.processes.push(UeProcess {
                pid: p.pid,
                name: p.name.clone(),
                kind,
                committed_gb: gb(p.committed),
                ram_gb: gb(p.ram),
                uptime_min: if p.created > 0 && snap.now > p.created { (snap.now - p.created) / 600_000_000 } else { 0 },
                created: p.created,
            });
        } else if lower == "unrealbuildtool" {
            act.build = true;
        } else if lower == "unrealpak" {
            act.cook = true;
        } else if lower == "dotnet" || lower == "automationtool" {
            let cmd = cmds.get_with(p, win::command_line);
            if cmd.contains("unrealbuildtool") {
                act.build = true;
            }
            if cmd.contains("automationtool") && (cmd.contains("buildcookrun") || cmd.contains("-cook")) {
                act.cook = true;
            }
        }
    }
    ue.workers_committed_gb = gb(workers_committed);
    ue.total_committed_gb = gb(ue_committed);
    ue.total_ram_gb = gb(ue_ram);

    let level = pressure_level(cfg, &snap.sys);
    let ue_active = act.any();

    let ram_total_mb = snap.sys.ram_total / MB;
    let groups: Vec<GroupStatus> = cfg
        .active_groups()
        .map(|g| {
            let pats = Patterns::new(&g.processes);
            // Processes we can't open can't be trimmed or re-prioritized: they don't count.
            let members: Vec<&Proc> = snap
                .procs
                .iter()
                .enumerate()
                .filter(|&(i, p)| p.queryable && !protected[i] && pats.matches_lower(&lowers[i]))
                .map(|(_, p)| p)
                .collect();
            GroupStatus {
                name: g.name.clone(),
                processes: members.len(),
                ram_mb: members.iter().map(|p| p.ram).sum::<u64>() / MB,
                committed_mb: members.iter().map(|p| p.committed).sum::<u64>() / MB,
                policy: effective(g, ue_active, level, cfg.general.preset, ram_total_mb),
                hard_cap_mb: g.hard_cap_mb,
                hard_cap_per_process_mb: g.hard_cap_per_process_mb,
                suspended: false,
                pids: members.iter().map(|p| (p.pid, p.created)).collect(),
            }
        })
        .collect();

    let system = SystemStatus::from(&snap.sys);
    let mut advice = Vec::new();
    for p in &ue.processes {
        if p.kind == "editor" && p.committed_gb >= cfg.ue.editor_restart_hint_gb {
            advice.push(format!(
                "{} (PID {}) has {:.0} GB committed after {} h {} min. Restart it to get that memory back.",
                p.name,
                p.pid,
                p.committed_gb,
                p.uptime_min / 60,
                p.uptime_min % 60
            ));
        }
    }
    if level == Level::Critical {
        advice.push(format!(
            "Memory is critical: {:.1} GB RAM free, {:.1} GB commit free. Don't start a build or cook until this clears.",
            system.ram_free_gb, system.commit_free_gb
        ));
    }
    if snap.sys.commit_limit > 0 && (snap.sys.commit_limit as f64) < snap.sys.ram_total as f64 * 1.4 {
        advice.push(format!(
            "The commit limit is {:.0} GB on {:.0} GB of RAM, so the page file is small. A larger fixed page file gives Unreal room to allocate.",
            system.commit_limit_gb, system.ram_total_gb
        ));
    }
    if git_count >= 20 {
        advice.push(format!("{} git processes are running. That usually means a stuck poller is piling them up.", git_count));
    }

    Status {
        time: win::local_time_string(),
        daemon: false,
        paused: false,
        mode: act.label(),
        activities: act,
        pressure: level,
        system,
        ue,
        groups,
        advice,
        worker_pids,
        disk: None,
    }
}

// ---------------------------------------------------------------- events

/// Things the UI may want to tell the user about. Collected by `Engine::tick`, handed out by
/// `Engine::take_events`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PressureChanged { from: Level, to: Level },
    AutoPrimed { reason: &'static str /* "build" | "cook" */, trimmed: usize, groups_ram_before_gb: f64 },
    EditorRestartHint { pid: u32, committed_gb: f64 },
    EditorGrowing { pid: u32, gb_per_hour: f64, committed_gb: f64 },
    SettingsReloaded,
    SettingsError(String),
}

/// Events kept when nobody takes them; the oldest are dropped first.
const MAX_EVENTS: usize = 256;

/// Which workload just started: `activities.build` or `activities.cook` going from false to
/// true. The first observation (`prev` = None) is a baseline, not a start.
pub fn auto_prime_reason(prev: Option<Activities>, now: Activities) -> Option<&'static str> {
    let prev = prev?;
    if now.build && !prev.build {
        Some("build")
    } else if now.cook && !prev.cook {
        Some("cook")
    } else {
        None
    }
}

/// Once-per-editor-process hints. A hint fires when its condition starts and can fire again
/// for the same process only after it has cleared (the growth hint clears below half the
/// alert rate, so a slope hovering at the threshold doesn't repeat it).
#[derive(Default)]
struct Hints {
    restart: HashSet<u32>,
    growing: HashSet<u32>,
}

impl Hints {
    fn update(&mut self, cfg: &Config, st: &Status, growth: impl Fn(&UeProcess) -> Option<f64>) -> Vec<Event> {
        let editors: Vec<&UeProcess> = st.ue.processes.iter().filter(|p| p.kind == "editor").collect();
        let alive: HashSet<u32> = editors.iter().map(|p| p.pid).collect();
        self.restart.retain(|pid| alive.contains(pid));
        self.growing.retain(|pid| alive.contains(pid));
        let mut out = Vec::new();
        let hint_gb = cfg.ue.editor_restart_hint_gb;
        let alert = cfg.ue.editor_growth_alert_gb_per_hour;
        for p in editors {
            if hint_gb > 0.0 && p.committed_gb >= hint_gb {
                if self.restart.insert(p.pid) {
                    out.push(Event::EditorRestartHint { pid: p.pid, committed_gb: p.committed_gb });
                }
            } else {
                self.restart.remove(&p.pid);
            }
            match growth(p).filter(|_| alert > 0.0) {
                Some(g) if g >= alert => {
                    if self.growing.insert(p.pid) {
                        out.push(Event::EditorGrowing {
                            pid: p.pid,
                            gb_per_hour: (g * 10.0).round() / 10.0,
                            committed_gb: p.committed_gb,
                        });
                    }
                }
                Some(g) if g >= alert / 2.0 => {}
                _ => {
                    self.growing.remove(&p.pid);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------- enforcement

/// A process as (pid, creation time): a recycled pid is a different key.
type ProcKey = (u32, u64);

/// How long a memory priority and CPU setting the engine made is trusted before it is set
/// again. Browsers reset their own priorities, and prime (here, in the tray's worker or in
/// another process) changes them behind the engine's back.
const REAPPLY_AFTER: Duration = Duration::from_secs(60);

/// What the engine last set on a process.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Applied {
    priority: u32,
    cpu: Cpu,
    at: Instant,
}

/// Set memory priority and CPU policy on a process now? Yes the first time, when the
/// policy changed, and again once `REAPPLY_AFTER` has passed.
fn should_apply(last: Option<&Applied>, priority: u32, cpu: Cpu, now: Instant) -> bool {
    match last {
        None => true,
        Some(a) => a.priority != priority || a.cpu != cpu || now.saturating_duration_since(a.at) >= REAPPLY_AFTER,
    }
}

/// The CPU calls that bring a process to `want`, given whether we changed its priority
/// class (`class_touched`) and turned on efficiency mode (`eco_on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CpuPlan {
    /// Read and remember the class before changing it.
    remember_class: bool,
    set_class: Option<win::CpuClass>,
    /// Put back the class we remembered.
    restore_class: bool,
    eco: Option<bool>,
}

fn cpu_plan(want: Cpu, class_touched: bool, eco_on: bool) -> CpuPlan {
    match want {
        Cpu::Normal => CpuPlan { restore_class: class_touched, eco: eco_on.then_some(false), ..Default::default() },
        Cpu::Below | Cpu::Idle => CpuPlan {
            remember_class: !class_touched,
            set_class: Some(if want == Cpu::Idle { win::CpuClass::Idle } else { win::CpuClass::BelowNormal }),
            eco: eco_on.then_some(false),
            ..Default::default()
        },
        // Efficiency mode replaces a lowered class: put the class back first.
        Cpu::Eco => CpuPlan { restore_class: class_touched, eco: Some(true), ..Default::default() },
    }
}

/// The members of one group the engine acts on: a process in two groups gets the policy of
/// the first, instead of flip-flopping between both every tick.
fn claim(claimed: &mut HashSet<u32>, members: &[ProcKey]) -> Vec<ProcKey> {
    members.iter().copied().filter(|k| claimed.insert(k.0)).collect()
}

/// Modification times of settings.toml and team.toml, to notice edits.
type Stamp = (Option<SystemTime>, Option<SystemTime>);

fn settings_stamp() -> Stamp {
    let m = |p: std::path::PathBuf| fs::metadata(p).and_then(|m| m.modified()).ok();
    (m(crate::paths::config_file()), m(crate::config::team_file()))
}

pub struct Engine {
    pub cfg: Config,
    cfg_stamp: Option<Stamp>,
    jobs: HashMap<String, win::Job>,
    last_trim: HashMap<String, Instant>,
    /// Cooldown multiplier per group, doubled each time a trim doesn't stick.
    trim_backoff: HashMap<String, u64>,
    /// Process -> CPU priority class it had before we changed it.
    cpu_touched: HashMap<ProcKey, u32>,
    eco_touched: HashSet<ProcKey>,
    /// Memory priority and CPU policy last set per process, so unchanged ones aren't set
    /// again every tick.
    applied: HashMap<ProcKey, Applied>,
    cmdlines: CmdLines,
    suspended: HashSet<u32>,
    workers_paused: HashSet<u32>,
    cap_misses: HashMap<String, usize>,
    pub paused: bool,
    /// When a timed pause ends; `None` while running or paused until resumed.
    paused_until: Option<Instant>,
    prev_activities: Option<Activities>,
    prev_level: Option<Level>,
    hints: Hints,
    events: Vec<Event>,
    history: History,
    /// The process list of the last tick, for watchers in the tray (the doctor's guard).
    last_procs: Vec<Proc>,
    /// The latest disk poll (the tray polls every 60 s; the engine never reads disks).
    disk: Option<crate::disk::DiskBrief>,
}

impl Engine {
    /// The engine for the background app: layered settings from the data folder and the
    /// memory history in `history\`.
    pub fn new() -> Engine {
        let mut e = Engine::with_config(Config::builtin());
        e.reload(true);
        e.history = History::open_default(e.cfg.history.keep_days);
        e
    }

    /// An engine with these settings and an in-memory history. Reads no files and touches
    /// no processes until `tick`, `reload` or `restore_all` is called.
    pub fn with_config(cfg: Config) -> Engine {
        Engine {
            cfg,
            cfg_stamp: None,
            jobs: HashMap::new(),
            last_trim: HashMap::new(),
            trim_backoff: HashMap::new(),
            cpu_touched: HashMap::new(),
            eco_touched: HashSet::new(),
            applied: HashMap::new(),
            cmdlines: CmdLines::default(),
            suspended: HashSet::new(),
            workers_paused: HashSet::new(),
            cap_misses: HashMap::new(),
            paused: false,
            paused_until: None,
            prev_activities: None,
            prev_level: None,
            hints: Hints::default(),
            events: Vec::new(),
            history: History::in_memory(),
            last_procs: Vec::new(),
            disk: None,
        }
    }

    fn push_event(&mut self, e: Event) {
        if self.events.len() >= MAX_EVENTS {
            self.events.remove(0);
        }
        self.events.push(e);
    }

    /// Events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// The memory history this engine records.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The disk block for status.json, from the tray's 60-second disk poll.
    pub fn set_disk(&mut self, disk: Option<crate::disk::DiskBrief>) {
        self.disk = disk;
    }

    /// The processes seen by the last `tick`.
    pub fn last_procs(&self) -> &[Proc] {
        &self.last_procs
    }

    /// Re-read the settings if settings.toml or team.toml changed on disk.
    pub fn reload(&mut self, force: bool) {
        let stamp = settings_stamp();
        if !force && self.cfg_stamp == Some(stamp) {
            return;
        }
        let first = self.cfg_stamp.is_none();
        match crate::config::load_layered() {
            Ok((cfg, warnings)) => {
                for w in &warnings {
                    crate::log::write(&format!("settings: {}", w));
                }
                if !first {
                    self.restore_all(); // undo the old settings before applying new ones
                }
                self.jobs.clear();
                for g in cfg.active_groups() {
                    if g.hard_cap_mb > 0 || g.hard_cap_per_process_mb > 0 {
                        let name = format!("Local\\AtherMemGuard-{}", g.name);
                        match win::Job::create(Some(&name), g.hard_cap_per_process_mb * MB, g.hard_cap_mb * MB, false) {
                            Ok(job) => {
                                self.jobs.insert(g.name.clone(), job);
                            }
                            Err(e) => crate::log::write(&format!("[{}] hard cap not applied: {}", g.name, e)),
                        }
                    }
                }
                crate::log::write(&format!(
                    "settings loaded: {} groups, preset {}, every {} s",
                    cfg.active_groups().count(),
                    cfg.general.preset.label(),
                    cfg.interval_sec
                ));
                self.history.set_keep_days(cfg.history.keep_days);
                self.cfg = cfg;
                if !first {
                    self.push_event(Event::SettingsReloaded);
                }
            }
            Err(e) => {
                crate::log::write(&format!("settings error, keeping previous settings: {}", e));
                self.push_event(Event::SettingsError(e));
            }
        }
        // Loading may have written the template or migrated the file: stamp after it.
        self.cfg_stamp = Some(settings_stamp());
    }

    /// One pass: measure, decide, enforce, record history, publish status.json.
    pub fn tick(&mut self) -> Status {
        self.reload(false);
        if self.pause_expired(Instant::now()) {
            self.set_paused(false);
        }
        let snap = snapshot();
        let mut st = analyze_with(&self.cfg, &snap, &mut self.cmdlines);
        st.daemon = true;
        st.paused = self.paused;
        if self.cfg.history.enabled {
            self.history.record(&st);
        } else {
            self.history.record_memory(&st);
        }
        let prime_reason = self.observe(&st);
        if !self.paused {
            self.apply(&snap, &mut st, Instant::now());
            if let Some(reason) = prime_reason {
                self.auto_prime(reason, &snap, &st);
            }
        }
        st.disk = self.disk.clone();
        write_status(&st);
        self.last_procs = snap.procs;
        st
    }

    /// Compare this status with the previous one: queue pressure and editor events, and say
    /// whether a build or cook just started and auto-prime should run.
    fn observe(&mut self, st: &Status) -> Option<&'static str> {
        if let Some(prev) = self.prev_level {
            if prev != st.pressure {
                self.push_event(Event::PressureChanged { from: prev, to: st.pressure });
            }
        }
        self.prev_level = Some(st.pressure);
        let reason = auto_prime_reason(self.prev_activities, st.activities);
        self.prev_activities = Some(st.activities);
        let history = &self.history;
        let hints = self.hints.update(&self.cfg, st, |p| history.editor_growth_gb_per_hour(p.pid, p.created));
        for e in hints {
            self.push_event(e);
        }
        reason.filter(|_| self.cfg.general.auto_prime && !self.paused)
    }

    /// Trim every group once because a build or cook just started.
    fn auto_prime(&mut self, reason: &'static str, snap: &Snapshot, st: &Status) {
        let groups_ram_before_gb = (st.groups.iter().map(|g| g.ram_mb).sum::<u64>() as f64 / 102.4).round() / 10.0;
        let trimmed = crate::ops::trim_groups(&self.cfg, &snap.procs);
        self.forget_applied(); // trim_groups lowered memory priorities
        crate::log::write(&format!(
            "auto-prime: {} started, trimmed {} processes (app groups held {:.1} GB)",
            reason, trimmed, groups_ram_before_gb
        ));
        self.push_event(Event::AutoPrimed { reason, trimmed, groups_ram_before_gb });
    }

    /// Forget which priorities the engine set, so the next tick sets them all again. Call it
    /// after something else changed them (a prime).
    pub fn forget_applied(&mut self) {
        self.applied.clear();
    }

    fn apply(&mut self, snap: &Snapshot, st: &mut Status, now: Instant) {
        let alive: HashSet<ProcKey> = snap.procs.iter().map(|p| (p.pid, p.created)).collect();
        let alive_pids: HashSet<u32> = alive.iter().map(|k| k.0).collect();
        self.cpu_touched.retain(|k, _| alive.contains(k));
        self.eco_touched.retain(|k| alive.contains(k));
        self.applied.retain(|k, _| alive.contains(k));
        self.suspended.retain(|pid| alive_pids.contains(pid));
        self.workers_paused.retain(|pid| alive_pids.contains(pid));

        let critical = st.pressure == Level::Critical;
        let cooldown = if critical { 15 } else { self.cfg.trim_cooldown_sec };
        let mut claimed = HashSet::new();

        for gs in st.groups.iter_mut() {
            let eff = gs.policy;
            let mine = claim(&mut claimed, &gs.pids);
            let mut uncapped = 0usize;
            for &key in &mine {
                let priority = eff.priority.value();
                if should_apply(self.applied.get(&key), priority, eff.cpu, now) {
                    self.set_policy(key, priority, eff.cpu);
                    self.applied.insert(key, Applied { priority, cpu: eff.cpu, at: now });
                }

                if let Some(job) = self.jobs.get(&gs.name) {
                    if !job.assign(key.0) {
                        uncapped += 1;
                    }
                }
            }
            // Sandboxed helpers refuse to join a job. Say so once, when the count changes.
            if self.jobs.contains_key(&gs.name) && self.cap_misses.insert(gs.name.clone(), uncapped) != Some(uncapped) {
                crate::log::write(&format!(
                    "[{}] hard cap covers {} of {} processes{}",
                    gs.name,
                    mine.len() - uncapped,
                    mine.len(),
                    if uncapped > 0 { " (the rest refused to join the job; sandboxed helpers do that)" } else { "" }
                ));
            }

            if eff.trim_above_mb > 0 && gs.ram_mb > eff.trim_above_mb {
                // Back off when a group climbs straight back over its limit: that memory is in
                // active use, and trimming it again and again only causes page faults.
                let backoff = if critical { 1 } else { *self.trim_backoff.get(&gs.name).unwrap_or(&1) };
                let since = self.last_trim.get(&gs.name).map(|t| t.elapsed().as_secs());
                let due = since.map_or(true, |s| s >= cooldown * backoff);
                if due {
                    let trimmed = count_ok(gs.pids.iter().map(|k| k.0), win::trim);
                    let next = match since {
                        Some(s) if !critical && s < cooldown * backoff * 3 => (backoff * 2).min(16),
                        _ => 1,
                    };
                    self.trim_backoff.insert(gs.name.clone(), next);
                    self.last_trim.insert(gs.name.clone(), Instant::now());
                    crate::log::write(&format!(
                        "[{}] RAM {} MB > {} MB ({}, {}): trimmed {} of {} processes",
                        gs.name,
                        gs.ram_mb,
                        eff.trim_above_mb,
                        st.mode,
                        st.pressure.label(),
                        trimmed,
                        gs.pids.len()
                    ));
                }
            }

            if eff.suspend {
                for &(pid, _) in &mine {
                    if !self.suspended.contains(&pid) && win::suspend(pid) {
                        self.suspended.insert(pid);
                    }
                }
                gs.suspended = true;
            } else {
                for &(pid, _) in &mine {
                    if self.suspended.remove(&pid) {
                        win::resume(pid);
                    }
                }
            }
        }

        self.throttle_workers(st, critical);
        st.ue.workers_paused = self.workers_paused.len();
    }

    /// Opt-in: at critical pressure pause the newest compile/shader workers, keeping at
    /// least `min_workers` running, and resume them all when pressure drops.
    fn throttle_workers(&mut self, st: &Status, critical: bool) {
        if !self.cfg.ue.worker_throttle || !critical {
            if !self.workers_paused.is_empty() {
                crate::log::write(&format!("resuming {} paused workers", self.workers_paused.len()));
                for pid in self.workers_paused.drain() {
                    win::resume(pid);
                }
            }
            return;
        }
        let mut workers = st.worker_pids.clone();
        workers.sort_by_key(|&(_, created)| created); // oldest first: closest to finishing
        let min = self.cfg.ue.min_workers.max(1);
        let mut running = 0usize;
        let mut newly = 0usize;
        for (pid, _) in workers {
            if running < min {
                if self.workers_paused.remove(&pid) {
                    win::resume(pid);
                }
                running += 1;
            } else if !self.workers_paused.contains(&pid) && win::suspend(pid) {
                self.workers_paused.insert(pid);
                newly += 1;
            }
        }
        if newly > 0 {
            crate::log::write(&format!("critical pressure: paused {} workers, {} still running", newly, running));
        }
    }

    /// Set memory priority and CPU policy on one process.
    fn set_policy(&mut self, key: ProcKey, priority: u32, cpu: Cpu) {
        let pid = key.0;
        win::set_memory_priority(pid, priority);
        let plan = cpu_plan(cpu, self.cpu_touched.contains_key(&key), self.eco_touched.contains(&key));
        if plan.restore_class {
            self.restore_class(key);
        }
        if plan.remember_class {
            self.cpu_touched.insert(key, win::cpu_class_raw(pid));
        }
        if let Some(class) = plan.set_class {
            win::set_cpu_class(pid, class);
        }
        match plan.eco {
            Some(true) => {
                self.eco_touched.insert(key);
                win::set_eco_mode(pid, true);
            }
            Some(false) => {
                self.eco_touched.remove(&key);
                win::set_eco_mode(pid, false);
            }
            None => {}
        }
    }

    /// Put back the CPU priority class a process had before we lowered it.
    fn restore_class(&mut self, key: ProcKey) {
        if let Some(orig) = self.cpu_touched.remove(&key) {
            if orig != 0 {
                win::set_cpu_class_raw(key.0, orig);
            } else {
                win::set_cpu_class(key.0, win::CpuClass::Normal);
            }
        }
    }

    /// Undo our CPU changes to one process and forget what we set on it.
    fn restore_cpu(&mut self, key: ProcKey) {
        self.restore_class(key);
        if self.eco_touched.remove(&key) {
            win::set_eco_mode(key.0, false);
        }
        self.applied.remove(&key);
    }

    /// Put every app back the way we found it: normal memory priority, original CPU
    /// class, nothing suspended. Called on pause, exit and settings reload.
    pub fn restore_all(&mut self) {
        for pid in self.suspended.drain().chain(self.workers_paused.drain()) {
            win::resume(pid);
        }
        let keys: Vec<ProcKey> = self.cpu_touched.keys().copied().chain(self.eco_touched.iter().copied()).collect();
        for key in keys {
            self.restore_cpu(key);
        }
        self.applied.clear();
        let procs = win::list_processes();
        for g in self.cfg.active_groups() {
            for p in procs.iter().filter(|p| p.queryable && any_matches(&g.processes, &p.name) && !is_protected(&self.cfg, p)) {
                win::set_memory_priority(p.pid, MemPriority::Normal.value());
            }
        }
    }

    /// Pause until resumed (`true`) or resume now (`false`). Pausing puts every app back to
    /// normal.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.paused_until = None;
        if paused {
            self.restore_all();
        }
        crate::log::write(if paused { "paused, apps restored to normal" } else { "resumed" });
    }

    /// Pause for a while; the engine resumes by itself on the first tick after `d`.
    pub fn pause_for(&mut self, d: Duration) {
        self.set_paused(true);
        self.paused_until = Some(Instant::now() + d);
        crate::log::write(&format!("paused for {} min", d.as_secs().div_ceil(60)));
    }

    /// When a timed pause ends. `None` when running or paused until resumed.
    pub fn pause_until(&self) -> Option<Instant> {
        if self.paused {
            self.paused_until
        } else {
            None
        }
    }

    fn pause_expired(&self, now: Instant) -> bool {
        self.paused && self.paused_until.is_some_and(|t| now >= t)
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// Run `f` on every pid; how many succeeded.
fn count_ok(pids: impl IntoIterator<Item = u32>, mut f: impl FnMut(u32) -> bool) -> usize {
    pids.into_iter().filter(|&pid| f(pid)).count()
}

/// Publish the latest status for agents and `memguard status`. Written atomically.
pub fn write_status(st: &Status) {
    let path = crate::paths::status_file();
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(st) {
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(total_gb: u64, avail_gb: u64, limit_gb: u64, used_gb: u64) -> SysInfo {
        let g = 1024 * 1024 * 1024;
        SysInfo { ram_total: total_gb * g, ram_avail: avail_gb * g, commit_limit: limit_gb * g, commit_used: used_gb * g, ..Default::default() }
    }

    #[test]
    fn pressure_thresholds() {
        let cfg = Config::builtin();
        assert_eq!(pressure_level(&cfg, &sys(128, 40, 144, 80)), Level::Normal);
        assert_eq!(pressure_level(&cfg, &sys(128, 10, 144, 80)), Level::Elevated); // 7.8% RAM free
        assert_eq!(pressure_level(&cfg, &sys(128, 40, 144, 130)), Level::Elevated); // 9.7% commit free
        assert_eq!(pressure_level(&cfg, &sys(128, 4, 144, 80)), Level::Critical); // 3.1% RAM free
        assert_eq!(pressure_level(&cfg, &sys(128, 40, 144, 140)), Level::Critical); // 2.8% commit free
    }

    #[test]
    fn policy_overrides_stack() {
        let cfg = Config::builtin();
        let browsers = &cfg.groups[0];
        let ram = 128 * 1024;
        let b = Preset::Balanced;
        let idle = effective(browsers, false, Level::Normal, b, ram);
        assert_eq!((idle.priority, idle.trim_above_mb, idle.cpu), (MemPriority::Low, 6291, Cpu::Normal));
        let ue = effective(browsers, true, Level::Normal, b, ram);
        assert_eq!(ue.trim_above_mb, 3146);
        let elevated = effective(browsers, true, Level::Elevated, b, ram);
        assert_eq!((elevated.trim_above_mb, elevated.cpu), (1049, Cpu::Below));
        let critical = effective(browsers, true, Level::Critical, b, ram);
        assert_eq!((critical.trim_above_mb, critical.cpu, critical.suspend), (262, Cpu::Below, false));
    }

    #[test]
    fn soft_caps_scale_with_ram_and_preset() {
        let cfg = Config::builtin();
        let browsers = &cfg.groups[0];
        let chat = &cfg.groups[1];
        // Browsers idle soft cap (4.8%) and Chat critical (0.05%, under the floor).
        let cases = [(16u64, 786u64), (32, 1573), (64, 3146), (128, 6291)];
        for (gb, mb) in cases {
            let ram = gb * 1024;
            let at = |p: Preset| effective(browsers, false, Level::Normal, p, ram).trim_above_mb;
            assert_eq!(at(Preset::Balanced), mb, "{} GB", gb);
            assert_eq!(at(Preset::Gentle), mb * 2, "{} GB", gb);
            assert_eq!(at(Preset::Aggressive), ((mb as f64 * 0.5).round() as u64).max(128), "{} GB", gb);
            assert_eq!(effective(chat, true, Level::Critical, Preset::Balanced, ram).trim_above_mb, 128);
        }
        // Agents have no base soft cap: 0 stays off under every preset.
        let agents = &cfg.groups[2];
        assert_eq!(effective(agents, true, Level::Normal, Preset::Aggressive, 16 * 1024).trim_above_mb, 0);
        // MB wins over percent within one table.
        let mut g = browsers.clone();
        g.trim_above_mb = Some(2000);
        assert_eq!(effective(&g, false, Level::Normal, Preset::Balanced, 64 * 1024).trim_above_mb, 2000);
        assert_eq!(effective(&g, false, Level::Normal, Preset::Aggressive, 64 * 1024).trim_above_mb, 1000);
    }

    #[test]
    fn aggressive_lowers_cpu_while_unreal_runs() {
        let cfg = Config::builtin();
        let ram = 32 * 1024;
        let browsers = &cfg.groups[0];
        let background = &cfg.groups[3]; // cpu = "eco", stronger than below: kept
        assert_eq!(effective(browsers, false, Level::Normal, Preset::Aggressive, ram).cpu, Cpu::Normal);
        assert_eq!(effective(browsers, true, Level::Normal, Preset::Aggressive, ram).cpu, Cpu::Below);
        assert_eq!(effective(browsers, true, Level::Normal, Preset::Balanced, ram).cpu, Cpu::Normal);
        assert_eq!(effective(background, true, Level::Normal, Preset::Aggressive, ram).cpu, Cpu::Eco);
    }

    #[test]
    fn auto_prime_fires_on_rising_edge_only() {
        let a = |editor, build, cook| Activities { editor, build, cook };
        assert_eq!(auto_prime_reason(None, a(false, true, false)), None); // first tick is a baseline
        assert_eq!(auto_prime_reason(Some(a(false, false, false)), a(false, true, false)), Some("build"));
        assert_eq!(auto_prime_reason(Some(a(true, false, false)), a(true, false, true)), Some("cook"));
        assert_eq!(auto_prime_reason(Some(a(false, true, false)), a(false, true, false)), None);
        assert_eq!(auto_prime_reason(Some(a(false, true, false)), a(false, true, true)), Some("cook"));
        assert_eq!(auto_prime_reason(Some(a(false, true, true)), a(false, false, false)), None);
        assert_eq!(auto_prime_reason(Some(a(false, false, false)), a(true, false, false)), None);
        assert_eq!(auto_prime_reason(Some(a(false, false, false)), a(false, true, true)), Some("build"));
    }

    /// Creation time of the test editors: an hour before the history samples start.
    const TEST_CREATED: u64 = (1_794_000_000 - 3600 + 11_644_473_600) * 10_000_000;

    fn status(level: Level, act: Activities, editors: &[(u32, f64)]) -> Status {
        Status {
            time: "2026-11-14 12:00:00".into(),
            daemon: true,
            paused: false,
            mode: act.label(),
            activities: act,
            pressure: level,
            system: SystemStatus::from(&sys(32, 10, 48, 20)),
            ue: UeStatus {
                processes: editors
                    .iter()
                    .map(|&(pid, gb)| UeProcess {
                        pid,
                        name: "UnrealEditor".into(),
                        kind: "editor",
                        committed_gb: gb,
                        ram_gb: gb,
                        uptime_min: 60,
                        created: TEST_CREATED,
                    })
                    .collect(),
                ..Default::default()
            },
            groups: Vec::new(),
            advice: Vec::new(),
            worker_pids: Vec::new(),
            disk: None,
        }
    }

    fn test_engine() -> Engine {
        let mut cfg = Config::builtin();
        cfg.groups.clear(); // nothing to touch even if restore_all runs
        Engine::with_config(cfg)
    }

    #[test]
    fn observe_queues_pressure_and_auto_prime() {
        let mut e = test_engine();
        let idle = Activities::default();
        let build = Activities { build: true, ..Default::default() };
        assert_eq!(e.observe(&status(Level::Normal, idle, &[])), None);
        assert!(e.take_events().is_empty());
        assert_eq!(e.observe(&status(Level::Elevated, build, &[])), Some("build"));
        assert_eq!(e.take_events(), vec![Event::PressureChanged { from: Level::Normal, to: Level::Elevated }]);
        assert_eq!(e.observe(&status(Level::Elevated, build, &[])), None);
        assert!(e.take_events().is_empty());
        // Turned off in settings: no auto-prime.
        e.cfg.general.auto_prime = false;
        e.observe(&status(Level::Elevated, idle, &[]));
        assert_eq!(e.observe(&status(Level::Elevated, build, &[])), None);
        // Paused: no auto-prime either.
        e.cfg.general.auto_prime = true;
        e.observe(&status(Level::Elevated, idle, &[]));
        e.paused = true;
        assert_eq!(e.observe(&status(Level::Elevated, build, &[])), None);
    }

    #[test]
    fn restart_hint_fires_once_per_process_until_it_clears() {
        let mut e = test_engine();
        e.cfg.ue.editor_restart_hint_gb = 48.0;
        e.cfg.ue.editor_growth_alert_gb_per_hour = 0.0;
        let act = Activities { editor: true, ..Default::default() };
        let hints = |e: &mut Engine, editors: &[(u32, f64)]| {
            e.observe(&status(Level::Normal, act, editors));
            e.take_events()
        };
        assert!(hints(&mut e, &[(10, 40.0)]).is_empty());
        assert_eq!(hints(&mut e, &[(10, 50.0)]), vec![Event::EditorRestartHint { pid: 10, committed_gb: 50.0 }]);
        assert!(hints(&mut e, &[(10, 55.0)]).is_empty());
        // A second editor gets its own hint.
        assert_eq!(hints(&mut e, &[(10, 55.0), (11, 60.0)]), vec![Event::EditorRestartHint { pid: 11, committed_gb: 60.0 }]);
        // Cleared (memory dropped), then over again: fires again.
        assert!(hints(&mut e, &[(10, 30.0), (11, 60.0)]).is_empty());
        assert_eq!(hints(&mut e, &[(10, 49.0), (11, 60.0)]), vec![Event::EditorRestartHint { pid: 10, committed_gb: 49.0 }]);
        // The process exits; a new one with a recycled PID can be hinted again.
        assert!(hints(&mut e, &[]).is_empty());
        assert_eq!(hints(&mut e, &[(11, 60.0)]).len(), 1);
    }

    #[test]
    fn growth_hint_fires_once_with_hysteresis() {
        let mut hints = Hints::default();
        let mut cfg = Config::builtin();
        cfg.ue.editor_growth_alert_gb_per_hour = 6.0;
        cfg.ue.editor_restart_hint_gb = 0.0;
        let st = status(Level::Normal, Activities { editor: true, ..Default::default() }, &[(10, 30.0)]);
        let mut run = |g: Option<f64>| hints.update(&cfg, &st, |_| g);
        assert!(run(None).is_empty());
        assert!(run(Some(5.0)).is_empty());
        assert_eq!(run(Some(7.04)), vec![Event::EditorGrowing { pid: 10, gb_per_hour: 7.0, committed_gb: 30.0 }]);
        assert!(run(Some(8.0)).is_empty());
        assert!(run(Some(4.0)).is_empty()); // below the alert but above half: not cleared
        assert!(run(Some(6.5)).is_empty());
        assert!(run(Some(2.0)).is_empty()); // cleared
        assert_eq!(run(Some(6.0)).len(), 1);
        // 0 = off.
        cfg.ue.editor_growth_alert_gb_per_hour = 0.0;
        let mut hints = Hints::default();
        assert!(hints.update(&cfg, &st, |_| Some(100.0)).is_empty());
    }

    #[test]
    fn growth_hint_uses_history() {
        let mut e = test_engine();
        e.cfg.ue.editor_restart_hint_gb = 0.0;
        e.cfg.ue.editor_growth_alert_gb_per_hour = 6.0;
        let act = Activities { editor: true, ..Default::default() };
        let mut fired = Vec::new();
        for i in 0..=180 {
            let gb = 20.0 + 12.0 * (i * 10) as f64 / 3600.0; // 12 GB/h for 30 minutes
            let mut s = crate::history::Sample::from_status(&status(Level::Normal, act, &[(10, gb)]), 1_794_000_000 + i * 10);
            s.time = "2026-11-14 12:00:00".into();
            e.history.push(s);
            e.observe(&status(Level::Normal, act, &[(10, gb)]));
            fired.extend(e.take_events());
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert!(matches!(fired[0], Event::EditorGrowing { pid: 10, gb_per_hour, .. } if (gb_per_hour - 12.0).abs() < 0.2));
    }

    #[test]
    fn timed_pause_expires() {
        let mut e = test_engine();
        assert_eq!(e.pause_until(), None);
        e.pause_for(Duration::from_secs(1800));
        assert!(e.paused);
        let until = e.pause_until().unwrap();
        assert!(!e.pause_expired(until - Duration::from_secs(1)));
        assert!(e.pause_expired(until));
        assert!(e.pause_expired(until + Duration::from_secs(60)));
        // Resuming by hand clears the timer; pausing indefinitely has none.
        e.set_paused(false);
        assert_eq!(e.pause_until(), None);
        e.set_paused(true);
        assert_eq!(e.pause_until(), None);
        assert!(!e.pause_expired(Instant::now() + Duration::from_secs(86_400)));
    }

    #[test]
    fn events_are_capped() {
        let mut e = test_engine();
        for _ in 0..(MAX_EVENTS + 10) {
            e.push_event(Event::SettingsReloaded);
        }
        assert_eq!(e.take_events().len(), MAX_EVENTS);
        assert!(e.take_events().is_empty());
    }

    #[test]
    fn apply_skips_unchanged_and_reapplies_after_a_minute() {
        let t0 = Instant::now();
        let low = MemPriority::Low.value();
        let a = Applied { priority: low, cpu: Cpu::Below, at: t0 };
        assert!(should_apply(None, low, Cpu::Below, t0));
        assert!(!should_apply(Some(&a), low, Cpu::Below, t0 + Duration::from_secs(1)));
        assert!(!should_apply(Some(&a), low, Cpu::Below, t0 + Duration::from_secs(59)));
        assert!(should_apply(Some(&a), low, Cpu::Below, t0 + REAPPLY_AFTER));
        assert!(should_apply(Some(&a), MemPriority::Normal.value(), Cpu::Below, t0 + Duration::from_secs(1)));
        assert!(should_apply(Some(&a), low, Cpu::Eco, t0 + Duration::from_secs(1)));
    }

    #[test]
    fn cpu_plan_restores_the_class_for_eco() {
        // Below/Idle -> Eco used to leave the lowered class in place.
        let p = cpu_plan(Cpu::Eco, true, false);
        assert_eq!(p, CpuPlan { restore_class: true, eco: Some(true), ..Default::default() });
        // Eco -> Below turns efficiency mode off and remembers the class once.
        let p = cpu_plan(Cpu::Below, false, true);
        assert_eq!(p, CpuPlan { remember_class: true, set_class: Some(win::CpuClass::BelowNormal), eco: Some(false), ..Default::default() });
        assert!(!cpu_plan(Cpu::Idle, true, false).remember_class);
        assert_eq!(cpu_plan(Cpu::Idle, true, false).set_class, Some(win::CpuClass::Idle));
        // Normal undoes whatever we did, and nothing if we did nothing.
        assert_eq!(cpu_plan(Cpu::Normal, true, true), CpuPlan { restore_class: true, eco: Some(false), ..Default::default() });
        assert_eq!(cpu_plan(Cpu::Normal, false, false), CpuPlan::default());
    }

    #[test]
    fn first_group_wins() {
        let mut claimed = HashSet::new();
        assert_eq!(claim(&mut claimed, &[(4, 1), (8, 1)]), [(4, 1), (8, 1)]);
        assert_eq!(claim(&mut claimed, &[(8, 1), (12, 1)]), [(12, 1)]);
    }

    #[test]
    fn applied_cache_is_cleared_by_restore_all_and_auto_prime() {
        let mut e = test_engine();
        let mark = |e: &mut Engine| {
            e.applied.insert((4, 1), Applied { priority: 1, cpu: Cpu::Below, at: Instant::now() });
        };
        mark(&mut e);
        e.restore_all();
        assert!(e.applied.is_empty());
        mark(&mut e);
        let snap = Snapshot { sys: sys(32, 20, 48, 10), procs: Vec::new(), now: 0 };
        let st = analyze(&e.cfg, &snap);
        e.auto_prime("build", &snap, &st);
        assert!(e.applied.is_empty());
        mark(&mut e);
        e.forget_applied();
        assert!(e.applied.is_empty());
    }

    /// The whole loop on a process this test starts: set once, skipped while unchanged,
    /// set again after the TTL, Below -> Eco puts the class back, a recycled pid starts
    /// over, restore_all undoes it.
    #[test]
    fn apply_on_our_own_ping() {
        const NORMAL: u32 = 0x20;
        const BELOW: u32 = 0x4000;
        let mut child = std::process::Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start ping");
        let pid = child.id();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut cfg = Config::builtin();
            let mut g = cfg.groups[0].clone();
            g.name = "PingTest".into();
            g.processes = vec!["PING".into()];
            g.priority = Some(MemPriority::Low);
            g.cpu = Some(Cpu::Below);
            g.trim_above_mb = None;
            g.trim_above_pct = None;
            g.suspend = None;
            g.hard_cap_mb = 0;
            g.hard_cap_per_process_mb = 0;
            g.ue = None;
            g.elevated = None;
            g.critical = None;
            cfg.groups = vec![g.clone(), g.clone()]; // the same process in two groups
            cfg.groups[1].name = "PingTest2".into();
            cfg.groups[1].cpu = Some(Cpu::Idle);
            let procs: Vec<Proc> = win::list_processes().into_iter().filter(|p| p.pid == pid).collect();
            assert_eq!(procs.len(), 1);
            let key = (pid, procs[0].created);
            let snap = Snapshot { sys: sys(32, 20, 48, 10), procs, now: 0 };
            let mut e = Engine::with_config(cfg);
            let t0 = Instant::now();
            let mut st = analyze(&e.cfg, &snap);
            e.apply(&snap, &mut st, t0);
            assert_eq!(win::cpu_class_raw(pid), BELOW, "first group wins over Idle");
            assert_eq!(e.applied[&key].at, t0);
            // Something resets the class behind our back: not noticed within the TTL...
            win::set_cpu_class_raw(pid, NORMAL);
            e.apply(&snap, &mut st, t0 + Duration::from_secs(10));
            assert_eq!(win::cpu_class_raw(pid), NORMAL);
            // ...set again after it.
            e.apply(&snap, &mut st, t0 + REAPPLY_AFTER);
            assert_eq!(win::cpu_class_raw(pid), BELOW);
            // Below -> Eco: the class goes back to normal.
            e.cfg.groups[0].cpu = Some(Cpu::Eco);
            let mut st = analyze(&e.cfg, &snap);
            e.apply(&snap, &mut st, t0 + REAPPLY_AFTER + Duration::from_secs(1));
            assert_eq!(win::cpu_class_raw(pid), NORMAL);
            assert!(e.eco_touched.contains(&key) && !e.cpu_touched.contains_key(&key));
            // A recycled pid is another process: nothing carries over.
            let recycled = Snapshot { sys: snap.sys, procs: vec![Proc { created: key.1 + 1, ..snap.procs[0].clone() }], now: 0 };
            e.cfg.groups[0].cpu = Some(Cpu::Below);
            let mut st = analyze(&e.cfg, &recycled);
            e.apply(&recycled, &mut st, t0 + REAPPLY_AFTER + Duration::from_secs(2));
            assert!(!e.applied.contains_key(&key) && !e.eco_touched.contains(&key));
            assert!(e.applied.contains_key(&(pid, key.1 + 1)));
            e.restore_all();
            assert!(e.applied.is_empty() && e.cpu_touched.is_empty() && e.eco_touched.is_empty());
            assert_eq!(win::cpu_class_raw(pid), NORMAL);
        }));
        let _ = child.kill();
        let _ = child.wait();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn command_lines_are_cached_per_process() {
        let p = |pid: u32, created: u64| Proc { pid, ppid: 1, name: "dotnet".into(), ram: 0, committed: 0, created, queryable: true };
        let mut c = CmdLines::default();
        let reads = std::cell::Cell::new(0);
        let read = |s: &'static str| {
            let reads = &reads;
            move |_pid: u32| {
                reads.set(reads.get() + 1);
                Some(s.to_string())
            }
        };
        assert_eq!(c.get_with(&p(8, 1), read("dotnet UnrealBuildTool.dll")), "dotnet unrealbuildtool.dll");
        assert_eq!(c.get_with(&p(8, 1), read("something else")), "dotnet unrealbuildtool.dll");
        assert_eq!(reads.get(), 1);
        // A recycled pid is read again.
        assert_eq!(c.get_with(&p(8, 2), read("dotnet other.dll")), "dotnet other.dll");
        assert_eq!(reads.get(), 2);
        // Failures, empty lines and unknown creation times are not kept.
        assert_eq!(c.get_with(&p(12, 1), |_| None), "");
        assert_eq!(c.get_with(&p(12, 1), read("")), "");
        assert_eq!(c.get_with(&p(16, 0), read("x")), "x");
        assert_eq!(c.0.len(), 2);
        // Pruned to the processes still running.
        c.prune(&[p(8, 2)]);
        assert_eq!(c.0.keys().copied().collect::<Vec<_>>(), [(8, 2)]);
    }

    #[test]
    fn trim_log_counts_successes() {
        assert_eq!(count_ok([4, 8, 12, 16], |pid| pid != 8), 3);
        assert_eq!(count_ok([], |_| true), 0);
    }

    #[test]
    fn unopenable_processes_stay_out_of_groups() {
        let cfg = Config::builtin();
        let pattern = cfg.groups[0].processes[0].trim_end_matches('*').to_string();
        let gb = 1024 * 1024 * 1024;
        let p = |pid: u32, queryable: bool| Proc { pid, ppid: 1, name: pattern.clone(), ram: gb, committed: gb, created: 1, queryable };
        let snap = Snapshot { sys: sys(32, 20, 48, 10), procs: vec![p(100, true), p(104, false)], now: 2 };
        let st = analyze(&cfg, &snap);
        let g = st.groups.iter().find(|g| g.name == cfg.groups[0].name).unwrap();
        assert_eq!((g.processes, g.ram_mb, g.committed_mb), (1, 1024, 1024));
    }

    #[test]
    fn ue_processes_are_protected_even_if_listed() {
        let mut cfg = Config::builtin();
        cfg.groups[0].processes.push("UnrealEditor".into());
        let p = |name: &str| Proc { pid: 4242, ppid: 1, name: name.into(), ram: 0, committed: 0, created: 0, queryable: true };
        assert!(is_protected(&cfg, &p("UnrealEditor")));
        assert!(is_protected(&cfg, &p("UnrealEditor-Cmd")));
        assert!(is_protected(&cfg, &p("ShaderCompileWorker")));
        assert!(!is_protected(&cfg, &p("chrome")));
    }
}

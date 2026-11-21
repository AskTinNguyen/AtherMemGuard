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
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS};

use crate::breakdown::{breakdown_with_lowers, Owner, RamBreakdown};
use crate::config::{
    any_matches, limit_mb, scale_soft_cap, CompiledPatterns, Config, Cpu, Group, IoPriority, MemPriority, Policy, Preset,
};
use crate::history::History;
use crate::win::{self, Proc, SysInfo};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: u64 = 1024 * 1024;
const SURGICAL_MIN_PRIVATE_BYTES: u64 = 32 * MB;
const OS_RELIEF_COOLDOWN: Duration = Duration::from_secs(60);
const STATUS_MAX_STALE: Duration = Duration::from_secs(60);

fn gb(bytes: u64) -> f64 {
    (bytes as f64 / GB * 10.0).round() / 10.0
}

fn bytes_to_mb_round(bytes: u64) -> u64 {
    bytes.saturating_add(MB / 2) / MB
}

fn cpu_class_to_raw(class: win::CpuClass) -> u32 {
    match class {
        win::CpuClass::Normal => NORMAL_PRIORITY_CLASS,
        win::CpuClass::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        win::CpuClass::Idle => IDLE_PRIORITY_CLASS,
    }
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
    /// Where the RAM in use is.
    pub ram: RamBreakdown,
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

pub fn try_snapshot() -> Result<Snapshot, String> {
    let procs = win::try_list_processes()?;
    Ok(Snapshot { sys: win::sys_info(), procs, now: win::now_filetime() })
}

pub fn snapshot() -> Snapshot {
    Snapshot { sys: win::sys_info(), procs: win::list_processes(), now: win::now_filetime() }
}

// ---------------------------------------------------------------- analysis (no side effects)

#[derive(Debug, Clone, Copy, PartialEq)]
struct ResolvedPolicy {
    eff: Effective,
    io_priority: Option<IoPriority>,
    ws_cap_mb: u64,
    ws_cap_per_process_mb: u64,
    cpu_rate_pct: Option<u32>,
}

fn overlay_ram(res: &mut ResolvedPolicy, p: &Option<Policy>, ram_total_mb: u64) {
    if let Some(p) = p {
        if let Some(v) = p.priority {
            res.eff.priority = v;
        }
        if let Some(v) = limit_mb(p.trim_above_mb, p.trim_above_pct, ram_total_mb) {
            res.eff.trim_above_mb = v;
        }
        if let Some(v) = p.ws_cap_mb {
            res.ws_cap_mb = v;
        }
        if let Some(v) = p.ws_cap_per_process_mb {
            res.ws_cap_per_process_mb = v;
        }
    }
}

fn overlay_overall(res: &mut ResolvedPolicy, p: &Option<Policy>) {
    if let Some(p) = p {
        if let Some(v) = p.cpu {
            res.eff.cpu = v;
        }
        if let Some(v) = p.io_priority {
            res.io_priority = Some(v);
        }
        if let Some(v) = p.suspend {
            res.eff.suspend = v;
        }
        if let Some(v) = p.cpu_rate_pct {
            res.cpu_rate_pct = Some(v);
        }
    }
}

fn effective_split(
    g: &Group,
    ue_active: bool,
    ram_level: Level,
    commit_level: Level,
    preset: Preset,
    ram_total_mb: u64,
) -> ResolvedPolicy {
    let mut res = ResolvedPolicy {
        eff: Effective {
            priority: g.priority.unwrap_or(MemPriority::Normal),
            trim_above_mb: limit_mb(g.trim_above_mb, g.trim_above_pct, ram_total_mb).unwrap_or(0),
            cpu: g.cpu.unwrap_or(Cpu::Normal),
            suspend: g.suspend.unwrap_or(false),
        },
        io_priority: g.io_priority,
        ws_cap_mb: g.ws_cap_mb,
        ws_cap_per_process_mb: g.ws_cap_per_process_mb,
        cpu_rate_pct: g.cpu_rate_pct,
    };
    if ue_active {
        overlay_ram(&mut res, &g.ue, ram_total_mb);
        overlay_overall(&mut res, &g.ue);
    }
    if ram_level >= Level::Elevated {
        overlay_ram(&mut res, &g.elevated, ram_total_mb);
    }
    if ram_level >= Level::Critical {
        overlay_ram(&mut res, &g.critical, ram_total_mb);
    }
    let overall = ram_level.max(commit_level);
    if overall >= Level::Elevated {
        overlay_overall(&mut res, &g.elevated);
    }
    if overall >= Level::Critical {
        overlay_overall(&mut res, &g.critical);
    }
    res.eff.trim_above_mb = scale_soft_cap(res.eff.trim_above_mb, preset);
    if preset == Preset::Aggressive && ue_active && res.eff.cpu == Cpu::Normal {
        res.eff.cpu = Cpu::Below;
    }
    res
}

/// The policy a group gets now. Soft caps resolve from MB or percent of `ram_total_mb`
/// (MB wins within one table), then the preset scales them and the 128 MB floor applies.
/// The aggressive preset also lowers CPU priority (`below`) while Unreal is running, unless
/// the group already has a stronger CPU setting.
pub fn effective(g: &Group, ue_active: bool, level: Level, preset: Preset, ram_total_mb: u64) -> Effective {
    effective_split(g, ue_active, level, level, preset, ram_total_mb).eff
}

fn level_with_hysteresis(
    total: u64,
    avail: u64,
    elev_pct: f64,
    crit_pct: f64,
    hyst_pct: f64,
    hyst_gb: f64,
    prev: Option<Level>,
) -> Level {
    if total == 0 {
        return Level::Normal;
    }
    let total_f = total as f64;
    let avail_f = avail as f64;
    let crit_thresh = total_f * crit_pct / 100.0;
    let elev_thresh = total_f * elev_pct / 100.0;
    let slack = (total_f * hyst_pct / 100.0).min(hyst_gb * GB).max(0.0);

    if avail_f < crit_thresh {
        Level::Critical
    } else if prev == Some(Level::Critical) && avail_f < crit_thresh + slack {
        Level::Critical
    } else if avail_f < elev_thresh {
        Level::Elevated
    } else if prev.is_some_and(|l| l >= Level::Elevated) && avail_f < elev_thresh + slack {
        Level::Elevated
    } else {
        Level::Normal
    }
}

fn ram_pressure_with_hysteresis(cfg: &Config, sys: &SysInfo, prev: Option<Level>) -> Level {
    let p = &cfg.pressure;
    level_with_hysteresis(
        sys.ram_total,
        sys.ram_avail,
        p.elevated_ram_free_pct,
        p.critical_ram_free_pct,
        p.hysteresis_pct,
        p.hysteresis_gb,
        prev,
    )
}

fn commit_pressure_with_hysteresis(cfg: &Config, sys: &SysInfo, prev: Option<Level>) -> Level {
    let p = &cfg.pressure;
    level_with_hysteresis(
        sys.commit_limit,
        sys.commit_free(),
        p.elevated_commit_free_pct,
        p.critical_commit_free_pct,
        p.hysteresis_pct,
        p.hysteresis_gb,
        prev,
    )
}

pub fn pressure_level(cfg: &Config, sys: &SysInfo) -> Level {
    if sys.ram_total == 0 || sys.commit_limit == 0 {
        return Level::Normal;
    }
    ram_pressure_with_hysteresis(cfg, sys, None).max(commit_pressure_with_hysteresis(cfg, sys, None))
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
    /// The lowercased command line of `p`, from the cache or from `read`. Caches empty/unreadable
    /// results too when `p.created != 0` so uninspectable processes aren't re-queried every tick.
    /// Never caches when `p.created == 0` due to PID reuse ambiguity.
    fn get_with(&mut self, p: &Proc, read: impl FnOnce(u32) -> Option<String>) -> String {
        let key = (p.pid, p.created);
        if let Some(c) = self.0.get(&key) {
            return c.clone();
        }
        let cmd = read(p.pid).unwrap_or_default().to_ascii_lowercase();
        if p.created != 0 {
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
    let patterns = cfg.compile_patterns();
    let ram_level = ram_pressure_with_hysteresis(cfg, &snap.sys, None);
    let commit_level = commit_pressure_with_hysteresis(cfg, &snap.sys, None);
    analyze_with_compiled(cfg, &patterns, snap, cmds, ram_level, commit_level)
}

fn is_ue_build_tool(lower: &str) -> bool {
    matches!(
        lower,
        "ubahost"
            | "ubaagent"
            | "ubacli"
            | "livecodingconsole"
            | "unrealheadertool"
            | "unreallightmass"
            | "unrealbuildtool"
            | "shadercompileworker"
            | "dxc"
            | "fxc"
            | "ispc"
            | "cl"
            | "clang-cl"
            | "link"
            | "lld-link"
            | "rc"
            | "lib"
            | "llvm-lib"
            | "msbuild"
    )
}

fn classify_ue_editor(lower: &str, cmd: &str) -> (&'static str, bool, bool) {
    if cmd.contains("-run=cook") {
        ("cook", false, true)
    } else if lower.contains("-cmd") || cmd.contains("-run=") {
        let cook = cmd.contains("-run=deriveddatacache") || cmd.contains("-run=resavepackages");
        let build = cmd.contains("-run=buildlighting");
        ("commandlet", build, cook)
    } else {
        ("editor", false, false)
    }
}

fn classify_dotnet_or_uat(lower: &str, cmd: &str) -> (bool, bool) {
    let build = cmd.contains("unrealbuildtool")
        || cmd.contains("ubahost")
        || cmd.contains("unrealheadertool")
        || cmd.split_whitespace().any(|t| t == "build" || t == "-build");
    let cook = (lower == "automationtool" || cmd.contains("automationtool"))
        && (cmd.contains("buildcookrun") || cmd.contains("-cook"));
    (build, cook)
}

fn analyze_with_compiled(
    cfg: &Config,
    patterns: &CompiledPatterns,
    snap: &Snapshot,
    cmds: &mut CmdLines,
    ram_level: Level,
    commit_level: Level,
) -> Status {
    cmds.prune(&snap.procs);
    let me = win::current_pid();
    let lowers: Vec<String> = snap.procs.iter().map(|p| p.name.to_ascii_lowercase()).collect();

    let mut act = Activities::default();
    let mut ue = UeStatus::default();
    let mut worker_pids = Vec::new();
    let (mut ue_committed, mut ue_ram, mut workers_committed) = (0u64, 0u64, 0u64);
    let mut git_count = 0usize;
    // Who each process's RAM counts for: Unreal here, the groups below, the rest by name.
    let mut owners = vec![Owner::Nobody; snap.procs.len()];
    let mut group_members: Vec<Vec<&Proc>> = vec![Vec::new(); patterns.groups.len()];

    for (i, (p, lower)) in snap.procs.iter().zip(&lowers).enumerate() {
        let lower = lower.as_str();
        let is_protect_pat = patterns.protect.matches_lower(lower);
        let is_protected = p.pid == me || lower.contains("memguard") || is_protect_pat;

        if lower == "git" {
            git_count += 1;
        }
        if is_protect_pat {
            ue_committed += p.committed;
            ue_ram += p.ram;
            owners[i] = Owner::Unreal;
        }
        if patterns.workers.matches_lower(lower) {
            owners[i] = Owner::UnrealWorker;
            ue.workers += 1;
            workers_committed += p.committed;
            worker_pids.push((p.pid, p.created));
        }
        if lower.starts_with("unrealeditor") {
            let cmd = if p.queryable { cmds.get_with(p, win::command_line) } else { String::new() };
            let (kind, build, cook) = classify_ue_editor(lower, &cmd);
            act.build |= build;
            act.cook |= cook;
            if kind == "editor" {
                act.editor = true;
                owners[i] = Owner::UnrealEditor;
            }
            ue.processes.push(UeProcess {
                pid: p.pid,
                name: p.name.clone(),
                kind,
                committed_gb: gb(p.committed),
                ram_gb: gb(p.ram),
                uptime_min: if p.created > 0 && snap.now > p.created { (snap.now - p.created) / 600_000_000 } else { 0 },
                created: p.created,
            });
        } else if is_ue_build_tool(lower) {
            act.build = true;
        } else if lower == "unrealpak" {
            act.cook = true;
        } else if lower == "dotnet" || lower == "automationtool" {
            let cmd = if p.queryable { cmds.get_with(p, win::command_line) } else { String::new() };
            let (build, cook) = classify_dotnet_or_uat(lower, &cmd);
            act.build |= build;
            act.cook |= cook;
        }

        if p.queryable && !is_protected {
            for (gi, pats) in patterns.groups.iter().enumerate() {
                if pats.matches_lower(lower) {
                    group_members[gi].push(p);
                    if owners[i] == Owner::Nobody {
                        owners[i] = Owner::Group(gi);
                    }
                }
            }
        }
    }
    ue.workers_committed_gb = gb(workers_committed);
    ue.total_committed_gb = gb(ue_committed);
    ue.total_ram_gb = gb(ue_ram);

    let level = ram_level.max(commit_level);
    let ue_active = act.any();

    let ram_total_mb = snap.sys.ram_total / MB;
    let groups: Vec<GroupStatus> = cfg
        .active_groups()
        .zip(group_members)
        .map(|(g, members)| GroupStatus {
            name: g.name.clone(),
            processes: members.len(),
            ram_mb: bytes_to_mb_round(members.iter().map(|p| p.private).sum::<u64>()),
            committed_mb: members.iter().map(|p| p.committed).sum::<u64>() / MB,
            policy: effective_split(g, ue_active, ram_level, commit_level, cfg.general.preset, ram_total_mb).eff,
            hard_cap_mb: g.hard_cap_mb,
            hard_cap_per_process_mb: g.hard_cap_per_process_mb,
            suspended: false,
            pids: members.iter().map(|p| (p.pid, p.created)).collect(),
        })
        .collect();

    let group_names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
    let ram = breakdown_with_lowers(&snap.sys, &snap.procs, &lowers, &owners, &group_names);

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
        ram,
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
    restart: HashSet<ProcKey>,
    growing: HashSet<ProcKey>,
}

impl Hints {
    fn update(&mut self, cfg: &Config, st: &Status, growth: impl Fn(&UeProcess) -> Option<f64>) -> Vec<Event> {
        let editors: Vec<&UeProcess> = st.ue.processes.iter().filter(|p| p.kind == "editor").collect();
        let alive: HashSet<ProcKey> = editors.iter().map(|p| (p.pid, p.created)).collect();
        self.restart.retain(|key| alive.contains(key));
        self.growing.retain(|key| alive.contains(key));
        let mut out = Vec::new();
        let hint_gb = cfg.ue.editor_restart_hint_gb;
        let alert = cfg.ue.editor_growth_alert_gb_per_hour;
        for p in editors {
            let key = (p.pid, p.created);
            if hint_gb > 0.0 && p.committed_gb >= hint_gb {
                if self.restart.insert(key) {
                    out.push(Event::EditorRestartHint { pid: p.pid, committed_gb: p.committed_gb });
                }
            } else {
                self.restart.remove(&key);
            }
            match growth(p).filter(|_| alert > 0.0) {
                Some(g) if g >= alert => {
                    if self.growing.insert(key) {
                        out.push(Event::EditorGrowing {
                            pid: p.pid,
                            gb_per_hour: (g * 10.0).round() / 10.0,
                            committed_gb: p.committed_gb,
                        });
                    }
                }
                Some(g) if g >= alert / 2.0 => {}
                _ => {
                    self.growing.remove(&key);
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
    io: Option<u32>,
    at: Instant,
}

/// Set memory priority, CPU, and I/O policy on a process now? Yes the first time, when the
/// policy changed, and again once `REAPPLY_AFTER` has passed.
fn should_apply(last: Option<&Applied>, priority: u32, cpu: Cpu, io: Option<u32>, now: Instant) -> bool {
    match last {
        None => true,
        Some(a) => a.priority != priority || a.cpu != cpu || a.io != io || now.saturating_duration_since(a.at) >= REAPPLY_AFTER,
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
        // Win11 EcoQoS pairs IDLE_PRIORITY_CLASS + ExecutionSpeed.
        Cpu::Eco => CpuPlan {
            remember_class: !class_touched,
            set_class: Some(win::CpuClass::Idle),
            restore_class: false,
            eco: Some(true),
        },
    }
}

/// The members of one group the engine acts on: a process in two groups gets the policy of
/// the first, instead of flip-flopping between both every tick.
fn claim(claimed: &mut HashSet<u32>, members: &[ProcKey]) -> Vec<ProcKey> {
    members.iter().copied().filter(|k| claimed.insert(k.0)).collect()
}

fn is_shell_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "explorer" | "svchost" | "services" | "wininit" | "winlogon" | "csrss" | "smss" | "lsass"
    )
}

/// Build the set of `ProcKey`s belonging to the foreground window's application subtree:
/// walks up parents ONLY while parent has the same executable name (never ascending into
/// `explorer.exe`/`cmd.exe`/shell), then walks down to collect the app subtree.
fn foreground_tree(procs: &[Proc], fg_pid: Option<u32>) -> HashSet<ProcKey> {
    let mut out = HashSet::new();
    let Some(fg_pid) = fg_pid else {
        return out;
    };
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let Some(fg_proc) = by_pid.get(&fg_pid).copied() else {
        return out;
    };

    let mut root = fg_proc;
    let mut seen_up = HashSet::new();
    seen_up.insert((root.pid, root.created));
    while root.ppid != 0 && root.ppid != root.pid {
        let Some(&parent) = by_pid.get(&root.ppid) else {
            break;
        };
        if !seen_up.insert((parent.pid, parent.created)) {
            break;
        }
        if !parent.name.eq_ignore_ascii_case(&root.name) {
            break;
        }
        if parent.created != 0 && root.created != 0 && parent.created > root.created {
            break;
        }
        root = parent;
    }

    if is_shell_name(&root.name) {
        out.insert((fg_proc.pid, fg_proc.created));
        return out;
    }

    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        if p.ppid != 0 && p.ppid != p.pid {
            children.entry(p.ppid).or_default().push(p);
        }
    }
    let mut stack = vec![root];
    while let Some(cur) = stack.pop() {
        let key = (cur.pid, cur.created);
        if !out.insert(key) {
            continue;
        }
        if let Some(kids) = children.get(&cur.pid) {
            for &kid in kids {
                if cur.created != 0 && kid.created != 0 && cur.created > kid.created {
                    continue;
                }
                stack.push(kid);
            }
        }
    }
    out
}

/// Select which processes to surgically trim when `mine_private_bytes > trim_above_mb * MB`:
/// skips foreground-protected processes and processes under 32 MB private WS, sorts descending
/// by `p.private`, and stops once `excess` bytes are relieved.
fn select_surgical_trims<'a>(members: &[&'a Proc], trim_above_mb: u64, is_fg: impl Fn(ProcKey) -> bool) -> Vec<&'a Proc> {
    if trim_above_mb == 0 {
        return Vec::new();
    }
    let total_private: u64 = members.iter().map(|p| p.private).sum();
    let target = trim_above_mb.saturating_mul(MB);
    if total_private <= target {
        return Vec::new();
    }
    let mut excess = total_private - target;
    let mut candidates: Vec<&Proc> = members
        .iter()
        .copied()
        .filter(|p| p.private >= SURGICAL_MIN_PRIVATE_BYTES && !is_fg((p.pid, p.created)))
        .collect();
    candidates.sort_by(|a, b| b.private.cmp(&a.private).then_with(|| a.pid.cmp(&b.pid)));
    let mut out = Vec::new();
    for p in candidates {
        if excess == 0 {
            break;
        }
        out.push(p);
        excess = excess.saturating_sub(p.private);
    }
    out
}

fn group_has_cpu_rate(g: &Group) -> bool {
    g.cpu_rate_pct.is_some_and(|r| r > 0)
        || g.ue.as_ref().and_then(|p| p.cpu_rate_pct).is_some_and(|r| r > 0)
        || g.elevated.as_ref().and_then(|p| p.cpu_rate_pct).is_some_and(|r| r > 0)
        || g.critical.as_ref().and_then(|p| p.cpu_rate_pct).is_some_and(|r| r > 0)
}

fn group_has_ws_cap(g: &Group) -> bool {
    g.ws_cap_mb > 0
        || g.ue.as_ref().and_then(|p| p.ws_cap_mb).is_some_and(|w| w > 0)
        || g.elevated.as_ref().and_then(|p| p.ws_cap_mb).is_some_and(|w| w > 0)
        || g.critical.as_ref().and_then(|p| p.ws_cap_mb).is_some_and(|w| w > 0)
}

fn status_changed(prev: &Status, cur: &Status) -> bool {
    prev.paused != cur.paused
        || prev.ue.workers_paused != cur.ue.workers_paused
        || prev.pressure != cur.pressure
        || prev.mode != cur.mode
        || prev.activities.editor != cur.activities.editor
        || prev.activities.build != cur.activities.build
        || prev.activities.cook != cur.activities.cook
        || prev.groups.len() != cur.groups.len()
        || prev.groups.iter().zip(&cur.groups).any(|(a, b)| {
            a.name != b.name
                || a.processes != b.processes
                || a.suspended != b.suspended
                || a.hard_cap_mb != b.hard_cap_mb
                || a.hard_cap_per_process_mb != b.hard_cap_per_process_mb
        })
        || prev.disk.as_ref().map(|d| (d.level, d.volumes.len())) != cur.disk.as_ref().map(|d| (d.level, d.volumes.len()))
        || (prev.system.ram_free_gb - cur.system.ram_free_gb).abs() >= 0.5
        || (prev.system.commit_free_gb - cur.system.commit_free_gb).abs() >= 0.5
}

/// Modification times and byte lengths of settings.toml and team.toml, to notice edits.
type FileStamp = (Option<SystemTime>, u64);
type Stamp = (Option<FileStamp>, Option<FileStamp>);

fn settings_stamp() -> Stamp {
    let m = |p: std::path::PathBuf| fs::metadata(p).ok().map(|meta| (meta.modified().ok(), meta.len()));
    (m(crate::paths::config_file()), m(crate::config::team_file()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFile {
    Ready,
    Absent,
    TransientBusy,
    Empty,
}

fn is_transient_io_err(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock)
        || matches!(e.raw_os_error(), Some(32 | 33 | 5))
}

fn is_transient_err_msg(msg: &str) -> bool {
    msg.contains("os error 32")
        || msg.contains("os error 33")
        || msg.contains("being used by another process")
        || msg.contains("sharing violation")
}

fn probe_config_file(path: &std::path::Path) -> ProbeFile {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ProbeFile::Absent,
        Err(e) if is_transient_io_err(&e) => return ProbeFile::TransientBusy,
        Err(_) => return ProbeFile::Ready,
    };
    if meta.len() == 0 {
        return ProbeFile::Empty;
    }
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => ProbeFile::Empty,
        Ok(_) => ProbeFile::Ready,
        Err(e) if is_transient_io_err(&e) => ProbeFile::TransientBusy,
        Err(_) => ProbeFile::Ready,
    }
}

fn settle_config_files(settings_path: &std::path::Path, team_path: &std::path::Path) -> bool {
    const RETRIES: usize = 5;
    const DELAY: Duration = Duration::from_millis(40);
    for attempt in 0..=RETRIES {
        let s = probe_config_file(settings_path);
        let t = probe_config_file(team_path);
        let busy = s == ProbeFile::TransientBusy || t == ProbeFile::TransientBusy;
        let empty = s == ProbeFile::Empty || t == ProbeFile::Empty;
        if !busy && !empty {
            return true;
        }
        if attempt < RETRIES {
            std::thread::sleep(DELAY);
        } else {
            return !busy;
        }
    }
    true
}

pub struct Engine {
    pub cfg: Config,
    patterns: CompiledPatterns,
    cfg_stamp: Option<Stamp>,
    dir_watch: Option<win::DirWatch>,
    config_dirty: bool,
    jobs: HashMap<String, win::Job>,
    job_cache: HashMap<(ProcKey, String), bool>,
    job_cpu_rates: HashMap<String, Option<u32>>,
    job_ws_caps: HashMap<String, Option<u64>>,
    job_ws_caps_unsupported: HashSet<String>,
    #[cfg(test)]
    job_ws_cap_attempts: usize,
    last_trim: HashMap<String, Instant>,
    /// Cooldown multiplier per group, doubled each time a trim doesn't stick.
    trim_backoff: HashMap<String, u64>,
    /// Process -> CPU priority class it had before we changed it.
    cpu_touched: HashMap<ProcKey, u32>,
    eco_touched: HashSet<ProcKey>,
    io_touched: HashSet<ProcKey>,
    ws_capped: HashMap<ProcKey, usize>,
    /// Memory priority, CPU, and I/O policy last set per process, so unchanged ones aren't set
    /// again every tick.
    applied: HashMap<ProcKey, Applied>,
    cmdlines: CmdLines,
    suspended: HashSet<ProcKey>,
    workers_paused: HashSet<ProcKey>,
    fg_last_seen: HashMap<ProcKey, Instant>,
    cap_misses: HashMap<String, usize>,
    pub paused: bool,
    /// When a timed pause ends; `None` while running or paused until resumed.
    paused_until: Option<Instant>,
    prev_activities: Option<Activities>,
    prev_level: Option<Level>,
    prev_ram_level: Option<Level>,
    prev_commit_level: Option<Level>,
    hints: Hints,
    events: Vec<Event>,
    history: History,
    /// The process list of the last tick, for watchers in the tray (the doctor's guard).
    last_procs: Vec<Proc>,
    /// The latest disk poll (the tray polls every 60 s; the engine never reads disks).
    disk: Option<crate::disk::DiskBrief>,
    last_status: Option<Status>,
    last_status_at: Option<Instant>,
    last_os_relief: Option<Instant>,
    helper_task_missing: bool,
    last_snap_err: Option<Instant>,
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
        let patterns = cfg.compile_patterns();
        Engine {
            cfg,
            patterns,
            cfg_stamp: None,
            dir_watch: None,
            config_dirty: false,
            jobs: HashMap::new(),
            job_cache: HashMap::new(),
            job_cpu_rates: HashMap::new(),
            job_ws_caps: HashMap::new(),
            job_ws_caps_unsupported: HashSet::new(),
            #[cfg(test)]
            job_ws_cap_attempts: 0,
            last_trim: HashMap::new(),
            trim_backoff: HashMap::new(),
            cpu_touched: HashMap::new(),
            eco_touched: HashSet::new(),
            io_touched: HashSet::new(),
            ws_capped: HashMap::new(),
            applied: HashMap::new(),
            cmdlines: CmdLines::default(),
            suspended: HashSet::new(),
            workers_paused: HashSet::new(),
            fg_last_seen: HashMap::new(),
            cap_misses: HashMap::new(),
            paused: false,
            paused_until: None,
            prev_activities: None,
            prev_level: None,
            prev_ram_level: None,
            prev_commit_level: None,
            hints: Hints::default(),
            events: Vec::new(),
            history: History::in_memory(),
            last_procs: Vec::new(),
            disk: None,
            last_status: None,
            last_status_at: None,
            last_os_relief: None,
            helper_task_missing: false,
            last_snap_err: None,
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

    /// Release the retained `last_procs` vector once watchers (like doctor's guard) have read it.
    pub fn clear_last_procs(&mut self) {
        self.last_procs = Vec::new();
    }

    /// Clear `last_procs` and shrink oversized Win32 process-enumeration buffers when idle.
    pub fn shrink_idle(&mut self) {
        self.clear_last_procs();
        win::shrink_buffers();
    }

    /// Flush buffered history CSV rows to disk.
    pub fn flush_history(&mut self) {
        self.history.flush();
    }

    /// Win32 directory-watch handle for `data_dir()`, usable with `MsgWaitForMultipleObjects`.
    pub fn config_watch_handle(&self) -> Option<HANDLE> {
        self.dir_watch.as_ref().map(|w| w.raw_handle())
    }

    /// Safe wait handle for `data_dir()`, usable with `win::wait_any` / `win::msg_wait_any`.
    pub fn config_watch_wait(&self) -> Option<win::WaitHandle> {
        self.dir_watch.as_ref().map(|w| w.wait_handle())
    }

    /// Mark settings dirty so the next `reload(false)` re-checks file timestamps even if `DirWatch`
    /// has not fired or was already drained externally.
    pub fn mark_config_dirty(&mut self) {
        self.config_dirty = true;
    }

    fn ensure_jobs(&mut self) {
        for g in self.cfg.active_groups() {
            if self.jobs.contains_key(&g.name) {
                continue;
            }
            let needs_job = g.hard_cap_mb > 0
                || g.hard_cap_per_process_mb > 0
                || group_has_cpu_rate(g)
                || (win::is_elevated() && group_has_ws_cap(g));
            if needs_job {
                let name = crate::paths::scoped_kernel_name(&format!("Local\\AtherMemGuard-{}", g.name));
                match win::Job::create(Some(&name), g.hard_cap_per_process_mb * MB, g.hard_cap_mb * MB, false) {
                    Ok(job) => {
                        self.jobs.insert(g.name.clone(), job);
                    }
                    Err(e) => crate::log::write(&format!("[{}] hard cap not applied: {}", g.name, e)),
                }
            }
        }
    }

    fn clear_jobs(&mut self) {
        for job in self.jobs.values() {
            let _ = job.set_limits(0, 0, false);
            let _ = job.set_working_set_limits(0, 0);
            let _ = job.set_cpu_rate(None);
        }
        self.jobs.clear();
        self.job_cache.clear();
        self.job_cpu_rates.clear();
        self.job_ws_caps.clear();
        self.job_ws_caps_unsupported.clear();
        self.cap_misses.clear();
    }

    /// Re-read the settings if settings.toml or team.toml changed on disk.
    pub fn reload(&mut self, force: bool) {
        if !force && !self.config_dirty {
            if let Some(w) = &self.dir_watch {
                if !w.is_signaled() {
                    return;
                }
                for _ in 0..8 {
                    if !w.is_signaled() || !w.next() {
                        break;
                    }
                }
            }
        } else if let Some(w) = &self.dir_watch {
            for _ in 0..8 {
                if !w.is_signaled() || !w.next() {
                    break;
                }
            }
        }
        self.config_dirty = false;

        let stamp = settings_stamp();
        if !force && self.cfg_stamp == Some(stamp) {
            return;
        }
        let settings_path = crate::paths::config_file();
        let team_path = crate::config::team_file();
        if !settle_config_files(&settings_path, &team_path) {
            crate::log::write("settings file is busy, keeping previous settings until unlocked");
            if self.dir_watch.is_none() {
                self.dir_watch = win::watch_dir_handle(&crate::paths::data_dir());
            }
            return;
        }
        if let Some(w) = &self.dir_watch {
            for _ in 0..8 {
                if !w.is_signaled() || !w.next() {
                    break;
                }
            }
        }

        let first = self.cfg_stamp.is_none();
        let mut loaded = crate::config::load_layered();
        for _ in 0..3 {
            let Err(ref e) = loaded else { break };
            if !is_transient_err_msg(e) {
                break;
            }
            std::thread::sleep(Duration::from_millis(35));
            loaded = crate::config::load_layered();
        }
        match loaded {
            Ok((cfg, warnings)) => {
                for w in &warnings {
                    crate::log::write(&format!("settings: {}", w));
                }
                if !first {
                    self.restore_all(); // undo the old settings before applying new ones
                }
                self.clear_jobs();
                self.helper_task_missing = false;
                crate::log::write(&format!(
                    "settings loaded: {} groups, preset {}, every {} s",
                    cfg.active_groups().count(),
                    cfg.general.preset.label(),
                    cfg.interval_sec
                ));
                self.history.set_keep_days(cfg.history.keep_days);
                self.patterns = cfg.compile_patterns();
                self.cfg = cfg;
                if !self.paused {
                    self.ensure_jobs();
                }
                if !first {
                    self.push_event(Event::SettingsReloaded);
                }
                // Loading may have written the template or migrated the file: stamp after it.
                self.cfg_stamp = Some(settings_stamp());
            }
            Err(e) if is_transient_err_msg(&e) => {
                crate::log::write(&format!("settings file is busy, keeping previous settings until unlocked: {}", e));
                // Do NOT advance self.cfg_stamp on transient sharing/I/O errors so the next tick/wakeup retries.
            }
            Err(e) => {
                crate::log::write(&format!("settings error, keeping previous settings: {}", e));
                self.push_event(Event::SettingsError(e));
                // Stamp invalid TOML/schema errors so they are not re-emitted every tick until edited again.
                self.cfg_stamp = Some(settings_stamp());
            }
        }
        if self.dir_watch.is_none() {
            self.dir_watch = win::watch_dir_handle(&crate::paths::data_dir());
        }
    }

    fn maybe_write_status(&mut self, st: &Status, now: Instant) {
        let changed = self.last_status.as_ref().is_none_or(|prev| status_changed(prev, st));
        let stale = self.last_status_at.is_none_or(|at| now.saturating_duration_since(at) >= STATUS_MAX_STALE);
        if changed || stale {
            write_status(st);
            self.last_status = Some(st.clone());
            self.last_status_at = Some(now);
        }
    }

    fn fallback_status_on_snapshot_err(&self) -> Status {
        if let Some(prev) = &self.last_status {
            let mut st = prev.clone();
            st.time = win::local_time_string();
            st.daemon = true;
            st.paused = self.paused;
            st.disk = self.disk.clone();
            return st;
        }
        let sys = win::sys_info();
        Status {
            time: win::local_time_string(),
            daemon: true,
            paused: self.paused,
            mode: "idle".to_string(),
            activities: Activities::default(),
            pressure: Level::Normal,
            system: SystemStatus::from(&sys),
            ue: UeStatus::default(),
            groups: Vec::new(),
            ram: RamBreakdown::default(),
            advice: Vec::new(),
            worker_pids: Vec::new(),
            disk: self.disk.clone(),
        }
    }

    /// One pass: measure, decide, enforce, record history, publish status.json.
    pub fn tick(&mut self) -> Status {
        self.reload(false);
        let now = Instant::now();
        if self.pause_expired(now) {
            self.set_paused(false);
        }
        let snap = match try_snapshot() {
            Ok(s) => s,
            Err(e) => {
                if self.last_snap_err.is_none_or(|t| now.saturating_duration_since(t) >= STATUS_MAX_STALE) {
                    crate::log::write(&format!("snapshot failed (preserving engine state): {}", e));
                    self.last_snap_err = Some(now);
                }
                return self.fallback_status_on_snapshot_err();
            }
        };
        let ram_level = ram_pressure_with_hysteresis(&self.cfg, &snap.sys, self.prev_ram_level);
        let commit_level = commit_pressure_with_hysteresis(&self.cfg, &snap.sys, self.prev_commit_level);
        self.prev_ram_level = Some(ram_level);
        self.prev_commit_level = Some(commit_level);

        let mut st = analyze_with_compiled(&self.cfg, &self.patterns, &snap, &mut self.cmdlines, ram_level, commit_level);
        st.daemon = true;
        st.paused = self.paused;
        if self.cfg.history.enabled {
            self.history.record(&st);
        } else {
            self.history.record_memory(&st);
        }
        let prime_reason = self.observe(&st);
        if !self.paused {
            self.apply_with_levels(&snap, &mut st, ram_level, commit_level, now);
            if let Some(reason) = prime_reason {
                self.auto_prime(reason, &snap, &st);
            }
        }
        st.disk = self.disk.clone();
        self.maybe_write_status(&st, now);
        if !st.activities.any() {
            win::shrink_buffers();
        }
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
        if win::is_elevated() || (self.cfg.general.elevated_helper && !self.helper_task_missing) {
            let rep = crate::ops::relieve_os_memory(&self.cfg, true, true);
            if rep.status == crate::ops::OsReliefStatus::HelperNotInstalled && !self.helper_task_missing {
                self.helper_task_missing = true;
                crate::log::write("elevated helper task is not installed; skipping automatic OS relief until reload");
            }
        }
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

    fn is_foreground_protected(&self, key: ProcKey, fg_keys: &HashSet<ProcKey>, now: Instant) -> bool {
        if fg_keys.contains(&key) {
            return true;
        }
        if self.cfg.foreground_grace_sec > 0 {
            if let Some(&seen) = self.fg_last_seen.get(&key) {
                return now.saturating_duration_since(seen) < Duration::from_secs(self.cfg.foreground_grace_sec);
            }
        }
        false
    }

    #[cfg(test)]
    fn apply(&mut self, snap: &Snapshot, st: &mut Status, now: Instant) {
        let ram_level = ram_pressure_with_hysteresis(&self.cfg, &snap.sys, None);
        let commit_level = commit_pressure_with_hysteresis(&self.cfg, &snap.sys, None);
        self.apply_with_levels(snap, st, ram_level, commit_level, now);
    }

    fn apply_with_levels(&mut self, snap: &Snapshot, st: &mut Status, ram_level: Level, commit_level: Level, now: Instant) {
        if snap.procs.is_empty() {
            return;
        }
        let alive: HashSet<ProcKey> = snap.procs.iter().map(|p| (p.pid, p.created)).collect();
        self.cpu_touched.retain(|k, _| alive.contains(k));
        self.eco_touched.retain(|k| alive.contains(k));
        self.io_touched.retain(|k| alive.contains(k));
        self.ws_capped.retain(|k, _| alive.contains(k));
        self.applied.retain(|k, _| alive.contains(k));
        self.suspended.retain(|k| alive.contains(k));
        self.workers_paused.retain(|k| alive.contains(k));
        self.fg_last_seen.retain(|k, _| alive.contains(k));
        self.job_cache.retain(|(k, _), _| alive.contains(k));

        let fg_keys = foreground_tree(&snap.procs, win::foreground_pid());
        for &key in &fg_keys {
            self.fg_last_seen.insert(key, now);
        }
        let proc_by_key: HashMap<ProcKey, &Proc> = snap.procs.iter().map(|p| ((p.pid, p.created), p)).collect();

        let ram_critical = ram_level == Level::Critical;
        let overall_critical = ram_level.max(commit_level) == Level::Critical;
        let cooldown = if ram_critical { 15 } else { self.cfg.trim_cooldown_sec };
        let mut claimed = HashSet::new();
        let ue_active = st.activities.any();
        let ram_total_mb = snap.sys.ram_total / MB;

        let active_groups: Vec<Group> = self.cfg.active_groups().cloned().collect();
        for (g, gs) in active_groups.iter().zip(st.groups.iter_mut()) {
            let resolved = effective_split(g, ue_active, ram_level, commit_level, self.cfg.general.preset, ram_total_mb);
            let eff = resolved.eff;
            let mine = claim(&mut claimed, &gs.pids);
            let mut uncapped = 0usize;
            let mut eligible_keys: Vec<ProcKey> = Vec::with_capacity(mine.len());

            for &key in &mine {
                if self.is_foreground_protected(key, &fg_keys, now) {
                    if self.applied.contains_key(&key)
                        || self.cpu_touched.contains_key(&key)
                        || self.eco_touched.contains(&key)
                        || self.io_touched.contains(&key)
                        || self.ws_capped.contains_key(&key)
                        || self.suspended.contains(&key)
                    {
                        self.restore_process(key, true);
                    }
                } else {
                    eligible_keys.push(key);
                    let priority = eff.priority.value();
                    let want_io = resolved.io_priority.map(IoPriority::value);
                    if should_apply(self.applied.get(&key), priority, eff.cpu, want_io, now) {
                        self.set_policy(key, priority, eff.cpu, resolved.io_priority);
                        self.applied.insert(key, Applied { priority, cpu: eff.cpu, io: want_io, at: now });
                    }
                }

                if let Some(job) = self.jobs.get(&gs.name) {
                    let cache_key = (key, gs.name.clone());
                    let assigned = if let Some(&cached) = self.job_cache.get(&cache_key) {
                        cached
                    } else {
                        let ok = job.assign(key.0);
                        self.job_cache.insert(cache_key, ok);
                        ok
                    };
                    if !assigned {
                        uncapped += 1;
                    }
                }
            }

            if let Some(job) = self.jobs.get(&gs.name) {
                if gs.hard_cap_mb > 0 || gs.hard_cap_per_process_mb > 0 {
                    if self.cap_misses.insert(gs.name.clone(), uncapped) != Some(uncapped) {
                        crate::log::write(&format!(
                            "[{}] hard cap covers {} of {} processes{}",
                            gs.name,
                            mine.len() - uncapped,
                            mine.len(),
                            if uncapped > 0 { " (the rest refused to join the job; sandboxed helpers do that)" } else { "" }
                        ));
                    }
                }
                let want_cpu_rate = resolved.cpu_rate_pct.filter(|&r| r > 0);
                if self.job_cpu_rates.get(&gs.name).copied() != Some(want_cpu_rate) {
                    if let Err(e) = job.set_cpu_rate(want_cpu_rate) {
                        crate::log::write(&format!("[{}] CPU rate control failed: {}", gs.name, e));
                    }
                    self.job_cpu_rates.insert(gs.name.clone(), want_cpu_rate);
                }
            }

            // Optional physical working-set caps (driven by ram_level only).
            let mut per_proc_ws_cap: Option<usize> = if resolved.ws_cap_per_process_mb > 0 {
                Some((resolved.ws_cap_per_process_mb * MB) as usize)
            } else {
                None
            };
            let mut job_ws_handled = false;
            if resolved.ws_cap_mb > 0 && resolved.ws_cap_per_process_mb == 0 {
                if let Some(job) = self.jobs.get(&gs.name) {
                    let want = Some(resolved.ws_cap_mb * MB);
                    if !self.job_ws_caps_unsupported.contains(&gs.name)
                        && self.job_ws_caps.get(&gs.name).copied() != Some(want)
                    {
                        #[cfg(test)]
                        {
                            self.job_ws_cap_attempts += 1;
                        }
                        if job.set_working_set_limits(0, resolved.ws_cap_mb * MB).is_ok() {
                            self.job_ws_caps.insert(gs.name.clone(), want);
                            job_ws_handled = true;
                        } else {
                            self.job_ws_caps.insert(gs.name.clone(), None);
                            self.job_ws_caps_unsupported.insert(gs.name.clone());
                        }
                    } else if self.job_ws_caps.get(&gs.name).copied().flatten() == want {
                        job_ws_handled = true;
                    }
                }
                if !job_ws_handled && !eligible_keys.is_empty() {
                    let per = ((resolved.ws_cap_mb * MB) / eligible_keys.len() as u64).max(64 * MB);
                    per_proc_ws_cap = Some(per as usize);
                }
            } else if let Some(job) = self.jobs.get(&gs.name) {
                if self.job_ws_caps.get(&gs.name).copied().flatten().is_some() {
                    let _ = job.set_working_set_limits(0, 0);
                    self.job_ws_caps.insert(gs.name.clone(), None);
                }
            }

            if let Some(cap_bytes) = per_proc_ws_cap {
                for &key in &eligible_keys {
                    if self.ws_capped.get(&key) != Some(&cap_bytes) {
                        if win::set_working_set_cap(key.0, 0, cap_bytes, true) {
                            self.ws_capped.insert(key, cap_bytes);
                        }
                    }
                }
            } else {
                for &key in &eligible_keys {
                    if self.ws_capped.remove(&key).is_some() {
                        win::set_working_set_cap(key.0, 0, 0, true);
                    }
                }
            }

            // Surgical trimming on `mine` (Bug #4), driven by `ram_level` / `p.private` only.
            let mine_procs: Vec<&Proc> = mine.iter().filter_map(|k| proc_by_key.get(k).copied()).collect();
            let mine_private_bytes: u64 = mine_procs.iter().map(|p| p.private).sum();
            let mine_ram_mb = bytes_to_mb_round(mine_private_bytes);
            if eff.trim_above_mb > 0 && mine_ram_mb > eff.trim_above_mb {
                // Back off when a group climbs straight back over its limit: that memory is in
                // active use, and trimming it again and again only causes page faults.
                let backoff = if ram_critical { 1 } else { *self.trim_backoff.get(&gs.name).unwrap_or(&1) };
                let since = self.last_trim.get(&gs.name).map(|t| t.elapsed().as_secs());
                let due = since.map_or(true, |s| s >= cooldown * backoff);
                if due {
                    let targets = select_surgical_trims(&mine_procs, eff.trim_above_mb, |k| self.is_foreground_protected(k, &fg_keys, now));
                    let trimmed = count_ok(targets.iter().map(|p| p.pid), win::trim);
                    let next = match since {
                        Some(s) if !ram_critical && s < cooldown * backoff * 3 => (backoff * 2).min(16),
                        _ => 1,
                    };
                    self.trim_backoff.insert(gs.name.clone(), next);
                    self.last_trim.insert(gs.name.clone(), now);
                    crate::log::write(&format!(
                        "[{}] RAM {} MB > {} MB ({}, {}): trimmed {} of {} processes",
                        gs.name,
                        mine_ram_mb,
                        eff.trim_above_mb,
                        st.mode,
                        st.pressure.label(),
                        trimmed,
                        mine_procs.len()
                    ));
                }
            }

            if eff.suspend {
                let mut any_suspended = false;
                for &key in &eligible_keys {
                    if self.suspended.contains(&key) {
                        any_suspended = true;
                    } else if win::suspend(key.0) {
                        self.suspended.insert(key);
                        any_suspended = true;
                    }
                }
                gs.suspended = any_suspended;
            } else {
                for &key in &mine {
                    if self.suspended.remove(&key) {
                        win::resume(key.0);
                    }
                }
            }
        }

        // OS memory relief: driven strictly by physical RAM pressure (never commit pressure),
        // with 60s cooldown and strictly zero-admin/no-UAC when unconfigured.
        if ram_level >= Level::Elevated
            && (win::is_elevated() || (self.cfg.general.elevated_helper && !self.helper_task_missing))
            && self.last_os_relief.is_none_or(|t| now.saturating_duration_since(t) >= OS_RELIEF_COOLDOWN)
        {
            self.last_os_relief = Some(now);
            let rep = crate::ops::relieve_os_memory(&self.cfg, true, ram_level == Level::Critical);
            if rep.status == crate::ops::OsReliefStatus::HelperNotInstalled && !self.helper_task_missing {
                self.helper_task_missing = true;
                crate::log::write("elevated helper task is not installed; skipping automatic OS relief until reload");
            }
        }

        self.throttle_workers(st, overall_critical);
        st.ue.workers_paused = self.workers_paused.len();
    }

    /// Opt-in: at critical pressure pause the newest compile/shader workers, keeping at
    /// least `min_workers` running, and resume them all when pressure drops.
    fn throttle_workers(&mut self, st: &Status, critical: bool) {
        if !self.cfg.ue.worker_throttle || !critical {
            if !self.workers_paused.is_empty() {
                crate::log::write(&format!("resuming {} paused workers", self.workers_paused.len()));
                for key in self.workers_paused.drain() {
                    win::resume(key.0);
                    win::apply_process_policy(
                        key.0,
                        false,
                        win::ProcessPolicy {
                            memory_priority: Some(MemPriority::Normal.value()),
                            io_priority: Some(IoPriority::Normal.value()),
                            ..Default::default()
                        },
                    );
                }
            }
            return;
        }
        let mut workers = st.worker_pids.clone();
        workers.sort_by_key(|&(_, created)| created); // oldest first: closest to finishing
        let min = self.cfg.ue.min_workers.max(1);
        let mut running = 0usize;
        let mut newly = 0usize;
        for key in workers {
            if running < min {
                if self.workers_paused.remove(&key) {
                    win::resume(key.0);
                    win::apply_process_policy(
                        key.0,
                        false,
                        win::ProcessPolicy {
                            memory_priority: Some(MemPriority::Normal.value()),
                            io_priority: Some(IoPriority::Normal.value()),
                            ..Default::default()
                        },
                    );
                }
                running += 1;
            } else if !self.workers_paused.contains(&key) {
                win::apply_process_policy(
                    key.0,
                    false,
                    win::ProcessPolicy {
                        memory_priority: Some(MemPriority::VeryLow.value()),
                        io_priority: Some(IoPriority::VeryLow.value()),
                        ..Default::default()
                    },
                );
                win::trim(key.0);
                if win::suspend(key.0) {
                    self.workers_paused.insert(key);
                    newly += 1;
                }
            }
        }
        if newly > 0 {
            crate::log::write(&format!("critical pressure: paused {} workers, {} still running", newly, running));
        }
    }

    /// Set memory priority, CPU policy, and I/O priority on one process in a single OpenProcess.
    fn set_policy(&mut self, key: ProcKey, priority: u32, cpu: Cpu, io_priority: Option<IoPriority>) {
        let pid = key.0;
        let plan = cpu_plan(cpu, self.cpu_touched.contains_key(&key), self.eco_touched.contains(&key));
        let cpu_class_raw = if plan.restore_class {
            self.cpu_touched.remove(&key).map(|orig| if orig != 0 { orig } else { NORMAL_PRIORITY_CLASS })
        } else {
            plan.set_class.map(cpu_class_to_raw)
        };
        let want_io = match io_priority {
            Some(io) if io != IoPriority::Normal => {
                self.io_touched.insert(key);
                Some(io.value())
            }
            Some(_) => {
                self.io_touched.remove(&key);
                Some(IoPriority::Normal.value())
            }
            None if self.io_touched.remove(&key) => Some(IoPriority::Normal.value()),
            None => None,
        };
        let res = win::apply_process_policy(
            pid,
            plan.remember_class,
            win::ProcessPolicy {
                memory_priority: Some(priority),
                cpu_class_raw,
                eco_mode: plan.eco,
                io_priority: want_io,
            },
        );
        if plan.remember_class && res.opened {
            self.cpu_touched.insert(key, res.prev_cpu_class);
        }
        match plan.eco {
            Some(true) => {
                self.eco_touched.insert(key);
            }
            Some(false) => {
                self.eco_touched.remove(&key);
            }
            None => {}
        }
    }

    /// Undo our changes to one process (CPU, EcoQoS, I/O priority, WS cap, suspend, optionally memory priority).
    fn restore_process(&mut self, key: ProcKey, restore_mem: bool) {
        let orig_class = self
            .cpu_touched
            .remove(&key)
            .map(|orig| if orig != 0 { orig } else { NORMAL_PRIORITY_CLASS });
        let eco = self.eco_touched.remove(&key).then_some(false);
        let io = self.io_touched.remove(&key).then_some(IoPriority::Normal.value());
        let mem = restore_mem.then_some(MemPriority::Normal.value());
        if mem.is_some() || orig_class.is_some() || eco.is_some() || io.is_some() {
            win::apply_process_policy(
                key.0,
                false,
                win::ProcessPolicy {
                    memory_priority: mem,
                    cpu_class_raw: orig_class,
                    eco_mode: eco,
                    io_priority: io,
                },
            );
        }
        if self.ws_capped.remove(&key).is_some() {
            win::set_working_set_cap(key.0, 0, 0, true);
        }
        if self.suspended.remove(&key) {
            win::resume(key.0);
        }
        self.applied.remove(&key);
    }

    /// Put every app back the way we found it: normal memory priority, original CPU
    /// class, normal I/O priority, no WS/Job caps, nothing suspended. Called on pause, exit and settings reload.
    pub fn restore_all(&mut self) {
        for key in self.suspended.drain() {
            win::resume(key.0);
        }
        for key in self.workers_paused.drain() {
            win::resume(key.0);
            win::apply_process_policy(
                key.0,
                false,
                win::ProcessPolicy {
                    memory_priority: Some(MemPriority::Normal.value()),
                    io_priority: Some(IoPriority::Normal.value()),
                    ..Default::default()
                },
            );
        }
        let mut keys: Vec<ProcKey> = self
            .cpu_touched
            .keys()
            .copied()
            .chain(self.eco_touched.iter().copied())
            .chain(self.io_touched.iter().copied())
            .chain(self.ws_capped.keys().copied())
            .chain(self.applied.keys().copied())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            self.restore_process(key, true);
        }
        self.applied.clear();
        self.clear_jobs();
        if let Ok(procs) = win::try_list_processes() {
            let me = win::current_pid();
            for p in &procs {
                if !p.queryable || p.pid == me {
                    continue;
                }
                let lower = p.name.to_ascii_lowercase();
                if lower.contains("memguard") || self.patterns.protect.matches_lower(&lower) {
                    continue;
                }
                if self.patterns.groups.iter().any(|pats| pats.matches_lower(&lower)) {
                    win::set_memory_priority(p.pid, MemPriority::Normal.value());
                }
            }
        }
    }

    /// Pause until resumed (`true`) or resume now (`false`). Pausing puts every app back to
    /// normal. Idempotent when already in the requested state.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            if paused {
                self.paused_until = None;
            }
            return;
        }
        self.paused = paused;
        self.paused_until = None;
        if paused {
            self.restore_all();
        } else {
            self.ensure_jobs();
        }
        if let Some(st) = self.last_status.as_mut() {
            st.paused = self.paused;
            st.time = win::local_time_string();
            write_status(st);
            self.last_status_at = Some(Instant::now());
        }
        crate::log::write(if paused { "paused, apps restored to normal" } else { "resumed" });
    }

    /// Pause for a while; the engine resumes by itself on the first tick after `d`.
    pub fn pause_for(&mut self, d: Duration) {
        if !self.paused {
            self.set_paused(true);
        }
        self.paused_until = Instant::now().checked_add(d);
        if self.paused_until.is_some() {
            crate::log::write(&format!("paused for {} min", d.as_secs().div_ceil(60)));
        } else {
            crate::log::write("paused until resumed (duration exceeded timer limit)");
        }
    }

    /// When a timed pause ends. `None` when running or paused until resumed.
    pub fn pause_until(&self) -> Option<Instant> {
        if self.paused {
            self.paused_until
        } else {
            None
        }
    }

    /// Remaining duration for a timed pause, or `None` when running or paused indefinitely.
    pub fn pause_remaining(&self, now: Instant) -> Option<Duration> {
        self.pause_until().map(|until| until.saturating_duration_since(now))
    }

    fn pause_expired(&self, now: Instant) -> bool {
        self.paused && self.paused_until.is_some_and(|t| now >= t)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if !self.jobs.is_empty() {
            self.clear_jobs();
        }
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
    if let Ok(text) = serde_json::to_string(st) {
        if let Err(e) = crate::config::write_atomic(&path, &text) {
            crate::log::write(&format!("status write error: {}", e));
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
            ram: RamBreakdown::default(),
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
        let a = Applied { priority: low, cpu: Cpu::Below, io: None, at: t0 };
        assert!(should_apply(None, low, Cpu::Below, None, t0));
        assert!(!should_apply(Some(&a), low, Cpu::Below, None, t0 + Duration::from_secs(1)));
        assert!(!should_apply(Some(&a), low, Cpu::Below, None, t0 + Duration::from_secs(59)));
        assert!(should_apply(Some(&a), low, Cpu::Below, None, t0 + REAPPLY_AFTER));
        assert!(should_apply(Some(&a), MemPriority::Normal.value(), Cpu::Below, None, t0 + Duration::from_secs(1)));
        assert!(should_apply(Some(&a), low, Cpu::Eco, None, t0 + Duration::from_secs(1)));
        assert!(should_apply(Some(&a), low, Cpu::Below, Some(0), t0 + Duration::from_secs(1)));
    }

    #[test]
    fn cpu_plan_restores_the_class_for_eco() {
        // Win11 EcoQoS pairs IDLE_PRIORITY_CLASS + ExecutionSpeed, remembering original class once.
        let p = cpu_plan(Cpu::Eco, false, false);
        assert_eq!(p, CpuPlan { remember_class: true, set_class: Some(win::CpuClass::Idle), restore_class: false, eco: Some(true) });
        let p = cpu_plan(Cpu::Eco, true, false);
        assert_eq!(p, CpuPlan { remember_class: false, set_class: Some(win::CpuClass::Idle), restore_class: false, eco: Some(true) });
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
            e.applied.insert((4, 1), Applied { priority: 1, cpu: Cpu::Below, io: None, at: Instant::now() });
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
    /// set again after the TTL, Below -> Eco sets IDLE + EcoQoS while keeping original class,
    /// a recycled pid starts over, restore_all undoes it.
    #[test]
    fn apply_on_our_own_ping() {
        const NORMAL: u32 = 0x20;
        const IDLE: u32 = 0x40;
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
            // Below -> Eco: Win11 EcoQoS sets IDLE_PRIORITY_CLASS + ExecutionSpeed, keeping NORMAL in cpu_touched.
            e.cfg.groups[0].cpu = Some(Cpu::Eco);
            let mut st = analyze(&e.cfg, &snap);
            e.apply(&snap, &mut st, t0 + REAPPLY_AFTER + Duration::from_secs(1));
            assert_eq!(win::cpu_class_raw(pid), IDLE);
            assert!(e.eco_touched.contains(&key) && e.cpu_touched.get(&key) == Some(&NORMAL));
            // Eco -> Normal restores the original NORMAL priority class and clears EcoQoS.
            e.cfg.groups[0].cpu = Some(Cpu::Normal);
            let mut st = analyze(&e.cfg, &snap);
            e.apply(&snap, &mut st, t0 + REAPPLY_AFTER + Duration::from_secs(2));
            assert_eq!(win::cpu_class_raw(pid), NORMAL);
            assert!(!e.eco_touched.contains(&key) && !e.cpu_touched.contains_key(&key));
            // Put it back in Eco so we verify a recycled PID prunes old tracking state.
            e.cfg.groups[0].cpu = Some(Cpu::Eco);
            let mut st = analyze(&e.cfg, &snap);
            e.apply(&snap, &mut st, t0 + REAPPLY_AFTER + Duration::from_secs(3));
            assert!(e.eco_touched.contains(&key));
            // A recycled pid is a new OS process (starts at NORMAL): nothing carries over.
            win::set_cpu_class_raw(pid, NORMAL);
            let recycled = Snapshot { sys: snap.sys, procs: vec![Proc { created: key.1 + 1, ..snap.procs[0].clone() }], now: 0 };
            e.cfg.groups[0].cpu = Some(Cpu::Below);
            let mut st = analyze(&e.cfg, &recycled);
            e.apply(&recycled, &mut st, t0 + REAPPLY_AFTER + Duration::from_secs(4));
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
        let p = |pid: u32, created: u64| Proc { pid, ppid: 1, name: "dotnet".into(), ram: 0, private: 0, committed: 0, created, queryable: true };
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
        // Negative (None) and empty lines ARE cached when created != 0; unknown created == 0 is NOT cached.
        assert_eq!(c.get_with(&p(12, 1), |_| None), "");
        assert_eq!(c.get_with(&p(12, 1), read("should not run")), "");
        assert_eq!(reads.get(), 2);
        assert_eq!(c.get_with(&p(13, 1), read("")), "");
        assert_eq!(c.get_with(&p(13, 1), read("also cached")), "");
        assert_eq!(reads.get(), 3);
        assert_eq!(c.get_with(&p(16, 0), read("x")), "x");
        assert_eq!(c.get_with(&p(16, 0), read("y")), "y");
        assert_eq!(reads.get(), 5);
        assert_eq!(c.0.len(), 4);
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
        let p = |pid: u32, queryable: bool| Proc { pid, ppid: 1, name: pattern.clone(), ram: gb, private: gb, committed: gb, created: 1, queryable };
        let snap = Snapshot { sys: sys(32, 20, 48, 10), procs: vec![p(100, true), p(104, false)], now: 2 };
        let st = analyze(&cfg, &snap);
        let g = st.groups.iter().find(|g| g.name == cfg.groups[0].name).unwrap();
        assert_eq!((g.processes, g.ram_mb, g.committed_mb), (1, 1024, 1024));
    }

    #[test]
    fn ram_breakdown_counts_each_process_once_and_adds_up() {
        let cfg = Config::builtin();
        let g = 1024 * 1024 * 1024u64;
        // Private RAM in tenths of a GB. Odd pids never exist on Windows, so reading the
        // editor's command line finds nothing and it counts as an editor.
        let p = |pid: u32, name: &str, tenths: u64| Proc {
            pid,
            ppid: 1,
            name: name.into(),
            ram: tenths * g / 10 * 2,
            private: tenths * g / 10,
            committed: tenths * g / 10,
            created: 0,
            queryable: true,
        };
        let procs = vec![
            p(0, "[System Process]", 0),
            p(4, "System", 2),
            p(999_999, "UnrealEditor", 300),
            p(1_000_001, "ShaderCompileWorker", 10),
            p(1_000_003, "ShaderCompileWorker", 10),
            p(1_000_005, "chrome", 15),
            p(1_000_007, "svchost", 5),
            p(1_000_009, "Memory Compression", 7),
            p(1_000_011, "rider64", 20),
            p(1_000_013, "Notion", 3),
            p(1_000_015, "notion", 2),
        ];
        let mut s = sys(64, 14, 96, 50);
        s.nonpaged_pool = g;
        s.resident_paged_pool = g / 2;
        s.resident_system_cache = 8 * g;
        let st = analyze(&cfg, &Snapshot { sys: s, procs, now: 0 });
        let r = &st.ram;
        assert_eq!(r.in_use_gb, 50.0);
        assert_eq!((r.unreal_gb, r.unreal_editor_gb, r.unreal_workers_gb), (32.0, 30.0, 2.0));
        assert_eq!(r.groups.iter().find(|x| x.name == "Browsers").map(|x| x.ram_gb), Some(1.5));
        assert_eq!((r.windows_gb, r.compressed_gb, r.file_cache_gb), (2.2, 0.7, 8.0));
        assert_eq!((r.other_apps_gb, r.other_apps_count), (2.5, 2));
        // Names that differ only in case are one app, named as first seen.
        assert_eq!(r.top_apps.iter().map(|a| (a.name.as_str(), a.processes, a.ram_gb)).collect::<Vec<_>>(), [("rider64", 1, 2.0), ("Notion", 2, 0.5)]);
        let parts = r.unreal_gb + r.groups.iter().map(|x| x.ram_gb).sum::<f64>() + r.other_apps_gb + r.windows_gb + r.compressed_gb + r.file_cache_gb;
        assert!((parts + r.unattributed_gb - r.in_use_gb).abs() < 0.05, "{} + {}", parts, r.unattributed_gb);
        // App names stay out of status.json.
        let json = serde_json::to_string(&st).unwrap();
        assert!(json.contains("\"file_cache_gb\":8.0") && !json.contains("rider64"));
    }

    #[test]
    fn ue_processes_are_protected_even_if_listed() {
        let mut cfg = Config::builtin();
        cfg.groups[0].processes.push("UnrealEditor".into());
        let p = |name: &str| Proc { pid: 4242, ppid: 1, name: name.into(), ram: 0, private: 0, committed: 0, created: 0, queryable: true };
        assert!(is_protected(&cfg, &p("UnrealEditor")));
        assert!(is_protected(&cfg, &p("UnrealEditor-Cmd")));
        assert!(is_protected(&cfg, &p("ShaderCompileWorker")));
        assert!(!is_protected(&cfg, &p("chrome")));
    }

    #[test]
    fn private_ws_rounding_handles_sub_mb_and_zero() {
        assert_eq!(bytes_to_mb_round(0), 0);
        assert_eq!(bytes_to_mb_round(400 * 1024), 0);
        assert_eq!(bytes_to_mb_round(729_088), 1); // ~712 KB ping.exe rounds to 1 MB
        assert_eq!(bytes_to_mb_round(1024 * MB), 1024);
    }

    #[test]
    fn ram_and_commit_pressure_split_with_hysteresis() {
        let mut cfg = Config::builtin();
        // 64 GB RAM, 100 GB commit.
        // elevated_ram = 10% (6.4 GB), critical_ram = 4% (2.56 GB)
        // hysteresis_pct = 2% (1.28 GB), hysteresis_gb = 2.0 GB -> slack = 1.28 GB
        cfg.pressure.hysteresis_pct = 2.0;
        cfg.pressure.hysteresis_gb = 2.0;

        // Escalate RAM to Critical (2.0 GB free < 2.56 GB).
        let r1 = ram_pressure_with_hysteresis(&cfg, &sys(64, 2, 100, 10), None);
        assert_eq!(r1, Level::Critical);
        // Recover to 3.0 GB: >= 2.56 GB but < 2.56 + 1.28 = 3.84 GB -> stays Critical!
        let r2 = ram_pressure_with_hysteresis(&cfg, &sys(64, 3, 100, 10), Some(r1));
        assert_eq!(r2, Level::Critical);
        // Recover to 4.0 GB: >= 3.84 GB, < 6.4 GB -> drops to Elevated.
        let r3 = ram_pressure_with_hysteresis(&cfg, &sys(64, 4, 100, 10), Some(r2));
        assert_eq!(r3, Level::Elevated);
        // Recover to 7.0 GB: >= 6.4 GB, < 6.4 + 1.28 = 7.68 GB -> stays Elevated!
        let r4 = ram_pressure_with_hysteresis(&cfg, &sys(64, 7, 100, 10), Some(r3));
        assert_eq!(r4, Level::Elevated);
        // Recover to 8.0 GB: >= 7.68 GB -> drops to Normal.
        let r5 = ram_pressure_with_hysteresis(&cfg, &sys(64, 8, 100, 10), Some(r4));
        assert_eq!(r5, Level::Normal);

        // Commit pressure Critical while RAM Normal: NEVER escalates trim_above_mb, priority, or ws_cap!
        let mut g = cfg.groups[0].clone();
        g.priority = Some(MemPriority::Normal);
        g.trim_above_mb = Some(4096);
        g.ws_cap_mb = 4096;
        g.cpu = Some(Cpu::Normal);
        g.io_priority = None;
        g.cpu_rate_pct = None;
        g.critical = Some(Policy {
            priority: Some(MemPriority::VeryLow),
            trim_above_mb: Some(256),
            ws_cap_mb: Some(512),
            cpu: Some(Cpu::Eco),
            io_priority: Some(IoPriority::VeryLow),
            cpu_rate_pct: Some(25),
            ..Default::default()
        });
        let res = effective_split(&g, false, Level::Normal, Level::Critical, Preset::Balanced, 64 * 1024);
        assert_eq!(res.eff.priority, MemPriority::Normal);
        assert_eq!(res.eff.trim_above_mb, 4096);
        assert_eq!(res.ws_cap_mb, 4096);
        assert_eq!(res.eff.cpu, Cpu::Eco);
        assert_eq!(res.io_priority, Some(IoPriority::VeryLow));
        assert_eq!(res.cpu_rate_pct, Some(25));
    }

    #[test]
    fn foreground_tree_and_grace_window() {
        let p = |pid: u32, ppid: u32, name: &str, created: u64| Proc {
            pid,
            ppid,
            name: name.into(),
            ram: 100 * MB,
            private: 100 * MB,
            committed: 100 * MB,
            created,
            queryable: true,
        };
        let procs = vec![
            p(100, 4, "explorer", 10),
            p(200, 100, "chrome", 20),
            p(201, 200, "chrome", 21),       // child renderer
            p(202, 201, "crashpad", 22),     // child helper of app
            p(300, 100, "discord", 25),      // unrelated app under explorer
            p(400, 300, "discord", 15),      // recycled PID: parent created after child
        ];
        // Focusing renderer (201) walks up only within same name ("chrome" -> 200), never explorer (100),
        // and walks down to collect 200, 201, 202, while excluding discord (300).
        let tree = foreground_tree(&procs, Some(201));
        assert!(tree.contains(&(200, 20)));
        assert!(tree.contains(&(201, 21)));
        assert!(tree.contains(&(202, 22)));
        assert!(!tree.contains(&(100, 10)));
        assert!(!tree.contains(&(300, 25)));

        // Focusing explorer itself must NOT protect unrelated apps spawned by explorer.
        let shell_tree = foreground_tree(&procs, Some(100));
        assert!(shell_tree.contains(&(100, 10)));
        assert!(!shell_tree.contains(&(200, 20)));
        assert!(!shell_tree.contains(&(300, 25)));

        // Recycled PID check on downward walk: parent discord(300, created 25) must not claim 400 (created 15).
        let disc_tree = foreground_tree(&procs, Some(300));
        assert!(disc_tree.contains(&(300, 25)));
        assert!(!disc_tree.contains(&(400, 15)));

        // Grace window protection.
        let mut cfg = Config::builtin();
        cfg.foreground_grace_sec = 45;
        let mut e = Engine::with_config(cfg);
        let t0 = Instant::now();
        e.fg_last_seen.insert((200, 20), t0);
        let empty = HashSet::new();
        assert!(e.is_foreground_protected((200, 20), &empty, t0 + Duration::from_secs(30)));
        assert!(!e.is_foreground_protected((200, 20), &empty, t0 + Duration::from_secs(45)));
    }

    #[test]
    fn surgical_trim_selection_order_and_threshold() {
        let p = |pid: u32, priv_mb: u64| Proc {
            pid,
            ppid: 1,
            name: "chrome".into(),
            ram: priv_mb * MB,
            private: priv_mb * MB,
            committed: priv_mb * MB,
            created: 1,
            queryable: true,
        };
        let p_fg = p(10, 800);
        let p_tiny = p(11, 20); // < 32 MB: skipped
        let p_med = p(12, 200);
        let p_big = p(13, 600);
        let p_mid = p(14, 400);
        let members = [&p_fg, &p_tiny, &p_med, &p_big, &p_mid];
        // Total private = 2020 MB, cap = 1200 MB -> excess = 820 MB.
        // Skips p_fg (foreground) and p_tiny (< 32 MB).
        // Picks largest first: p_big (600 MB -> remaining 220 MB), then p_mid (400 MB -> excess relieved!).
        // Stops before touching p_med (200 MB).
        let picked = select_surgical_trims(&members, 1200, |k| k.0 == 10);
        let pids: Vec<u32> = picked.iter().map(|x| x.pid).collect();
        assert_eq!(pids, vec![13, 14]);
    }

    #[test]
    fn bug2_job_caps_cleared_on_pause_and_rebuilt_on_resume() {
        let mut cfg = Config::builtin();
        let mut g = cfg.groups[0].clone();
        g.name = "Bug2Test".into();
        g.processes = vec!["__no_such_bug2_proc__".into()];
        g.hard_cap_mb = 512;
        g.cpu_rate_pct = Some(50);
        cfg.groups = vec![g];

        let mut e = Engine::with_config(cfg);
        e.ensure_jobs();
        assert!(e.jobs.contains_key("Bug2Test"));
        e.job_cpu_rates.insert("Bug2Test".into(), Some(50));
        e.job_cache.insert(((999_999, 1), "Bug2Test".into()), true);

        e.set_paused(true);
        assert!(e.jobs.is_empty(), "Bug #2: pause/restore_all must clear Job objects");
        assert!(e.job_cpu_rates.is_empty());
        assert!(e.job_cache.is_empty());

        e.set_paused(false);
        assert!(e.jobs.contains_key("Bug2Test"), "resuming must rebuild Job objects");
    }

    #[test]
    fn bug4_overlapping_groups_trim_only_mine() {
        let mut cfg = Config::builtin();
        let mut g1 = cfg.groups[0].clone();
        g1.name = "First".into();
        g1.processes = vec!["overlap_app".into()];
        g1.trim_above_mb = Some(10_000); // doesn't trim
        let mut g2 = g1.clone();
        g2.name = "Second".into();
        g2.trim_above_mb = Some(128); // below proc size, but g1 claimed it!
        cfg.groups = vec![g1, g2];

        let proc1 = Proc {
            pid: 777_777,
            ppid: 1,
            name: "overlap_app".into(),
            ram: 512 * MB,
            private: 512 * MB,
            committed: 512 * MB,
            created: 1,
            queryable: true,
        };
        let snap = Snapshot { sys: sys(64, 40, 80, 20), procs: vec![proc1], now: 100 };
        let mut e = Engine::with_config(cfg);
        let mut st = analyze(&e.cfg, &snap);
        e.apply(&snap, &mut st, Instant::now());
        // Second group's `mine` was empty, so it must NOT record a trim!
        assert!(!e.last_trim.contains_key("Second"), "Bug #4: Second group should not trim First's claimed process");
    }

    #[test]
    fn bug5_empty_snapshot_preserves_tracking_state() {
        let mut e = test_engine();
        e.cpu_touched.insert((100, 1), 0x20);
        e.eco_touched.insert((100, 1));
        e.io_touched.insert((100, 1));
        e.ws_capped.insert((100, 1), 64 * 1024 * 1024);
        e.suspended.insert((101, 2));
        e.workers_paused.insert((102, 3));
        e.applied.insert((100, 1), Applied { priority: 1, cpu: Cpu::Below, io: Some(0), at: Instant::now() });

        let empty_snap = Snapshot { sys: sys(64, 40, 80, 20), procs: Vec::new(), now: 0 };
        let mut st = analyze(&e.cfg, &empty_snap);
        e.apply(&empty_snap, &mut st, Instant::now());

        assert_eq!(e.cpu_touched.len(), 1);
        assert_eq!(e.eco_touched.len(), 1);
        assert_eq!(e.io_touched.len(), 1);
        assert_eq!(e.ws_capped.len(), 1);
        assert_eq!(e.suspended.len(), 1);
        assert_eq!(e.workers_paused.len(), 1);
        assert_eq!(e.applied.len(), 1);
    }

    #[test]
    fn bug11_pid_reuse_isolation_in_hints() {
        let mut hints = Hints::default();
        let mut cfg = Config::builtin();
        cfg.ue.editor_restart_hint_gb = 48.0;
        cfg.ue.editor_growth_alert_gb_per_hour = 0.0;

        let mut st1 = status(Level::Normal, Activities { editor: true, ..Default::default() }, &[(10, 50.0)]);
        st1.ue.processes[0].created = 1000;
        let ev1 = hints.update(&cfg, &st1, |_| None);
        assert_eq!(ev1, vec![Event::EditorRestartHint { pid: 10, committed_gb: 50.0 }]);

        // Same PID 10, different creation time (OS reused PID between ticks without an intermediate empty tick):
        // must be treated as a new process and fire again!
        let mut st2 = status(Level::Normal, Activities { editor: true, ..Default::default() }, &[(10, 50.0)]);
        st2.ue.processes[0].created = 2000;
        let ev2 = hints.update(&cfg, &st2, |_| None);
        assert_eq!(ev2, vec![Event::EditorRestartHint { pid: 10, committed_gb: 50.0 }]);
    }

    #[test]
    fn ue54_workload_and_commandlet_detection() {
        let cfg = Config::builtin();
        let p = |pid: u32, name: &str| Proc {
            pid,
            ppid: 1,
            name: name.into(),
            ram: 100 * MB,
            private: 100 * MB,
            committed: 100 * MB,
            created: 0,
            queryable: true,
        };
        for tool in [
            "UbaHost",
            "UbaAgent",
            "UbaCli",
            "LiveCodingConsole",
            "UnrealHeaderTool",
            "UnrealLightmass",
            "dxc",
            "fxc",
            "ispc",
            "rc",
            "lib",
            "llvm-lib",
        ] {
            let snap = Snapshot { sys: sys(64, 40, 80, 20), procs: vec![p(900_001, tool)], now: 0 };
            let st = analyze(&cfg, &snap);
            assert!(st.activities.build, "{} must activate build", tool);
        }

        let snap_pak = Snapshot { sys: sys(64, 40, 80, 20), procs: vec![p(900_002, "UnrealPak")], now: 0 };
        assert!(analyze(&cfg, &snap_pak).activities.cook);
    }

    #[test]
    fn status_delta_throttle_conditions() {
        let a = status(Level::Normal, Activities::default(), &[]);
        let mut b = a.clone();
        b.time = "2026-11-14 12:00:10".into();
        assert!(!status_changed(&a, &b));

        b.system.ram_free_gb += 0.2;
        assert!(!status_changed(&a, &b));

        b.system.ram_free_gb += 0.4; // total 0.6 >= 0.5
        assert!(status_changed(&a, &b));

        let mut c = a.clone();
        c.paused = true;
        assert!(status_changed(&a, &c));

        let mut d = a.clone();
        d.pressure = Level::Elevated;
        assert!(status_changed(&a, &d));

        let mut e = a.clone();
        e.ue.workers_paused = 2;
        assert!(status_changed(&a, &e));
    }

    #[test]
    fn ue54_commandlet_and_dotnet_classification() {
        assert_eq!(classify_ue_editor("unrealeditor", ""), ("editor", false, false));
        assert_eq!(classify_ue_editor("unrealeditor-cmd", "-run=cook"), ("cook", false, true));
        assert_eq!(classify_ue_editor("unrealeditor-cmd", "-run=deriveddatacache"), ("commandlet", false, true));
        assert_eq!(classify_ue_editor("unrealeditor-cmd", "-run=resavepackages"), ("commandlet", false, true));
        assert_eq!(classify_ue_editor("unrealeditor-cmd", "-run=buildlighting"), ("commandlet", true, false));
        assert_eq!(classify_ue_editor("unrealeditor", "-run=deriveddatacache"), ("commandlet", false, true));

        assert_eq!(classify_dotnet_or_uat("dotnet", "dotnet ubahost.dll"), (true, false));
        assert_eq!(classify_dotnet_or_uat("dotnet", "dotnet unrealheadertool.dll"), (true, false));
        assert_eq!(classify_dotnet_or_uat("dotnet", "dotnet build my.sln"), (true, false));
        assert_eq!(classify_dotnet_or_uat("automationtool", "automationtool.exe buildcookrun -cook"), (false, true));
    }

    #[test]
    fn io_priority_ws_cap_and_foreground_restore_on_ping() {
        let mut child = std::process::Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start ping");
        let pid = child.id();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut cfg = Config::builtin();
            let mut g = cfg.groups[0].clone();
            g.name = "IoWsTest".into();
            g.processes = vec!["PING".into()];
            g.priority = Some(MemPriority::Low);
            g.cpu = Some(Cpu::Eco);
            g.io_priority = Some(IoPriority::VeryLow);
            g.ws_cap_per_process_mb = 64;
            g.trim_above_mb = None;
            g.trim_above_pct = None;
            g.hard_cap_mb = 0;
            g.hard_cap_per_process_mb = 0;
            g.ue = None;
            g.elevated = None;
            g.critical = None;
            cfg.groups = vec![g];

            let procs: Vec<Proc> = win::list_processes().into_iter().filter(|p| p.pid == pid).collect();
            assert_eq!(procs.len(), 1);
            let key = (pid, procs[0].created);
            let snap = Snapshot { sys: sys(64, 40, 80, 20), procs, now: 0 };

            let mut e = Engine::with_config(cfg);
            let t0 = Instant::now();
            let mut st = analyze(&e.cfg, &snap);
            e.apply(&snap, &mut st, t0);
            assert!(e.io_touched.contains(&key));
            assert_eq!(e.ws_capped.get(&key), Some(&(64 * 1024 * 1024)));
            assert_eq!(win::cpu_class_raw(pid), IDLE_PRIORITY_CLASS);

            // Simulate process becoming recently foregrounded within grace window: must immediately restore!
            e.fg_last_seen.insert(key, t0 + Duration::from_secs(1));
            e.apply(&snap, &mut st, t0 + Duration::from_secs(2));
            assert!(!e.io_touched.contains(&key));
            assert!(!e.ws_capped.contains_key(&key));
            assert!(!e.eco_touched.contains(&key));
            assert!(!e.applied.contains_key(&key));
            assert_eq!(win::cpu_class_raw(pid), NORMAL_PRIORITY_CLASS);

            e.restore_all();
        }));
        let _ = child.kill();
        let _ = child.wait();
        if let Err(e) = res {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn pause_for_huge_duration_does_not_panic() {
        let mut e = Engine::with_config(Config::builtin());
        e.pause_for(Duration::from_secs(u64::MAX));
        assert!(e.paused);
        assert_eq!(e.pause_until(), None);
        assert_eq!(e.pause_remaining(Instant::now()), None);
    }

    #[test]
    fn settle_config_files_handles_zero_byte_truncate_and_sharing_lock() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = std::env::temp_dir().join(format!(
            "amg_engine_settle_{}_{}",
            win::current_pid(),
            win::now_filetime()
        ));
        fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.toml");
        let team = dir.join("team.toml");

        // Case 1: 0-byte truncate followed by delayed write completes cleanly.
        fs::write(&settings, b"").unwrap();
        let settings_bg = settings.clone();
        let writer1 = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            fs::write(&settings_bg, "interval_sec = 42\n").unwrap();
        });
        assert!(settle_config_files(&settings, &team));
        writer1.join().unwrap();
        assert_eq!(fs::read_to_string(&settings).unwrap(), "interval_sec = 42\n");

        // Case 2: exclusive share_mode(0) lock released after 60 ms resolves cleanly without error.
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(&settings)
            .unwrap();
        let writer2 = std::thread::spawn(move || {
            use std::io::Write;
            let mut f = lock;
            std::thread::sleep(Duration::from_millis(60));
            f.write_all(b"interval_sec = 77\n").unwrap();
        });
        assert!(settle_config_files(&settings, &team));
        writer2.join().unwrap();
        assert_eq!(fs::read_to_string(&settings).unwrap(), "interval_sec = 77\n");

        // Case 3: persistent exclusive lock beyond settle budget returns false (busy) so cfg_stamp is not poisoned.
        let held = fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&settings)
            .unwrap();
        assert!(!settle_config_files(&settings, &team));
        drop(held);
        assert!(settle_config_files(&settings, &team));

        let _ = fs::remove_dir_all(&dir);
    }

    struct KillChildGuard(std::process::Child);
    impl Drop for KillChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn t5_throttle_workers_oldest_first_resume_on_exit_and_restore_on_clear() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let spawn_ping = || {
            KillChildGuard(
                std::process::Command::new("ping")
                    .args(["-t", "127.0.0.1"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .expect("spawn ping worker"),
            )
        };
        let c0 = spawn_ping();
        let c1 = spawn_ping();
        let c2 = spawn_ping();
        let k0 = (c0.0.id(), 1000u64);
        let k1 = (c1.0.id(), 2000u64);
        let k2 = (c2.0.id(), 3000u64);

        let mut e = test_engine();
        e.cfg.ue.worker_throttle = true;
        e.cfg.ue.min_workers = 1;

        let mut st = status(Level::Critical, Activities { build: true, ..Default::default() }, &[]);
        st.worker_pids = vec![k2, k0, k1]; // unsorted input to verify oldest-first sorting
        e.throttle_workers(&st, true);
        // Oldest k0 stays active; newer k1 and k2 are suspended.
        assert!(!e.workers_paused.contains(&k0), "oldest worker k0 must stay running");
        assert!(e.workers_paused.contains(&k1), "k1 must be paused");
        assert!(e.workers_paused.contains(&k2), "k2 must be paused");

        // When oldest k0 exits, k1 (now oldest) must resume so min_workers=1 keeps running.
        st.worker_pids = vec![k2, k1];
        e.throttle_workers(&st, true);
        assert!(!e.workers_paused.contains(&k1), "k1 must resume when k0 exits");
        assert!(e.workers_paused.contains(&k2), "k2 stays paused");

        // When critical clears, all paused workers resume.
        e.throttle_workers(&st, false);
        assert!(e.workers_paused.is_empty(), "all paused workers must resume on clear");

        // Re-throttle and verify restore_all() clears/resumes cleanly too.
        st.worker_pids = vec![k0, k1, k2];
        e.throttle_workers(&st, true);
        assert_eq!(e.workers_paused.len(), 2);
        e.restore_all();
        assert!(e.workers_paused.is_empty());
    }

    #[test]
    fn t5_engine_commit_critical_ram_normal_skips_physical_trims_and_hysteresis() {
        let mut cfg = Config::builtin();
        let mut g = cfg.groups[0].clone();
        g.name = "SplitTest".into();
        g.processes = vec!["synthetic_split_app".into()];
        g.priority = Some(MemPriority::Normal);
        g.cpu = Some(Cpu::Normal);
        g.trim_above_mb = Some(500);
        g.trim_above_pct = None;
        g.hard_cap_mb = 0;
        g.hard_cap_per_process_mb = 0;
        g.ue = None;
        g.elevated = None;
        g.critical = Some(Policy {
            priority: Some(MemPriority::VeryLow),
            trim_above_mb: Some(128),
            trim_above_pct: None,
            cpu: Some(Cpu::Eco),
            io_priority: None,
            suspend: None,
            ws_cap_mb: None,
            ws_cap_per_process_mb: None,
            cpu_rate_pct: None,
        });
        cfg.groups = vec![g];

        // 50% RAM free (Normal) + 3% commit free (Critical).
        let sys_split = sys(100, 50, 100, 97);
        let ram_lvl = ram_pressure_with_hysteresis(&cfg, &sys_split, None);
        let commit_lvl = commit_pressure_with_hysteresis(&cfg, &sys_split, None);
        assert_eq!(ram_lvl, Level::Normal);
        assert_eq!(commit_lvl, Level::Critical);

        let proc_syn = Proc {
            pid: 888_881,
            ppid: 1,
            name: "synthetic_split_app".into(),
            ram: 256 * MB,
            private: 256 * MB, // > 128 MB critical cap, <= 500 MB base cap
            committed: 256 * MB,
            created: 42,
            queryable: true,
        };
        let snap = Snapshot { sys: sys_split, procs: vec![proc_syn], now: 1 };
        let mut e = Engine::with_config(cfg.clone());
        let patterns = e.cfg.compile_patterns();
        let mut st = analyze_with_compiled(&e.cfg, &patterns, &snap, &mut e.cmdlines, ram_lvl, commit_lvl);
        assert_eq!(st.pressure, Level::Critical);
        e.apply_with_levels(&snap, &mut st, ram_lvl, commit_lvl, Instant::now());
        // Physical trim must NOT trigger when ram_level is Normal, even though commit is Critical.
        assert!(!e.last_trim.contains_key("SplitTest"), "commit pressure must not trigger physical WS trim");
        assert!(e.last_os_relief.is_none(), "commit-only pressure must not trigger physical OS relief");

        // Stateful RAM hysteresis transition across 10% threshold with 2% / 2 GB slack:
        // 9.5% -> Elevated, 10.5% -> holds Elevated, 12.5% -> drops to Normal.
        let mut prev_ram = None;
        let l1 = ram_pressure_with_hysteresis(&cfg, &sys(100, 9500, 100, 50), prev_ram);
        let _ = l1; // Use MB-exact sys below since sys() takes GB!
        let g = 1024 * 1024 * 1024u64;
        let sys_pct = |avail_tenths_gb: u64| SysInfo {
            ram_total: 100 * g,
            ram_avail: avail_tenths_gb * g / 10,
            commit_limit: 100 * g,
            commit_used: 50 * g,
            ..Default::default()
        };
        prev_ram = Some(ram_pressure_with_hysteresis(&cfg, &sys_pct(95), prev_ram));
        assert_eq!(prev_ram, Some(Level::Elevated));
        prev_ram = Some(ram_pressure_with_hysteresis(&cfg, &sys_pct(105), prev_ram));
        assert_eq!(prev_ram, Some(Level::Elevated), "10.5% is inside 10%+2% hysteresis band");
        prev_ram = Some(ram_pressure_with_hysteresis(&cfg, &sys_pct(125), prev_ram));
        assert_eq!(prev_ram, Some(Level::Normal), "12.5% exits hysteresis band to Normal");
    }

    #[test]
    fn t5_non_elevated_job_ws_caps_does_not_retry_every_tick() {
        if win::is_elevated() {
            return;
        }
        let mut cfg = Config::builtin();
        let mut g = cfg.groups[0].clone();
        g.name = "WsCapNoRetry".into();
        g.processes = vec!["synthetic_wscap_proc".into()];
        g.hard_cap_mb = 256;
        g.ws_cap_mb = 128;
        g.ws_cap_per_process_mb = 0;
        g.trim_above_mb = None;
        g.trim_above_pct = None;
        g.ue = None;
        g.elevated = None;
        g.critical = None;
        cfg.groups = vec![g];

        let mut e = Engine::with_config(cfg);
        let job = win::Job::create(None, 0, 256 * MB, true).expect("create job");
        e.jobs.insert("WsCapNoRetry".into(), job);

        let proc_syn = Proc {
            pid: 888_882,
            ppid: 1,
            name: "synthetic_wscap_proc".into(),
            ram: 64 * MB,
            private: 64 * MB,
            committed: 64 * MB,
            created: 77,
            queryable: true,
        };
        let snap = Snapshot { sys: sys(64, 40, 80, 20), procs: vec![proc_syn], now: 1 };
        let mut st = analyze(&e.cfg, &snap);

        for _ in 0..5 {
            e.apply_with_levels(&snap, &mut st, Level::Normal, Level::Normal, Instant::now());
        }
        assert_eq!(e.job_ws_cap_attempts, 1, "B7: failing set_working_set_limits must not retry every tick");
        assert!(e.job_ws_caps_unsupported.contains("WsCapNoRetry"));

        e.clear_jobs();
        assert!(e.job_ws_caps_unsupported.is_empty());
    }
}

//! snapshot -> analyze (pure) -> apply.
//!
//! `analyze` turns a process snapshot into a `Status`: what Unreal is doing, how much
//! pressure memory is under, and which policy each group should get. `Engine::apply`
//! then enforces that policy. The CLI uses `analyze` alone, so `memcap status` works
//! with or without the background app running.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Instant, SystemTime};

use serde::Serialize;

use crate::config::{any_matches, Config, Cpu, Group, MemPriority, Policy};
use crate::win::{self, Proc, SysInfo};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: u64 = 1024 * 1024;

fn gb(bytes: u64) -> f64 {
    (bytes as f64 / GB * 10.0).round() / 10.0
}

// ---------------------------------------------------------------- status model

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
    #[serde(skip)]
    pub pids: Vec<u32>,
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

fn overlay(e: &mut Effective, p: &Option<Policy>) {
    if let Some(p) = p {
        if let Some(v) = p.priority {
            e.priority = v;
        }
        if let Some(v) = p.trim_above_mb {
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

pub fn effective(g: &Group, ue_active: bool, level: Level) -> Effective {
    let mut e = Effective {
        priority: g.priority.unwrap_or(MemPriority::Normal),
        trim_above_mb: g.trim_above_mb.unwrap_or(0),
        cpu: g.cpu.unwrap_or(Cpu::Normal),
        suspend: g.suspend.unwrap_or(false),
    };
    if ue_active {
        overlay(&mut e, &g.ue);
    }
    if level >= Level::Elevated {
        overlay(&mut e, &g.elevated);
    }
    if level >= Level::Critical {
        overlay(&mut e, &g.critical);
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

/// Is this process one MemCap must never touch?
pub fn is_protected(cfg: &Config, p: &Proc) -> bool {
    p.pid == win::current_pid() || p.name.to_ascii_lowercase().starts_with("memcap") || any_matches(&cfg.ue.protect, &p.name)
}

pub fn analyze(cfg: &Config, snap: &Snapshot) -> Status {
    let mut act = Activities::default();
    let mut ue = UeStatus::default();
    let mut worker_pids = Vec::new();
    let (mut ue_committed, mut ue_ram, mut workers_committed) = (0u64, 0u64, 0u64);
    let mut git_count = 0usize;

    for p in &snap.procs {
        let lower = p.name.to_ascii_lowercase();
        if lower == "git" {
            git_count += 1;
        }
        if any_matches(&cfg.ue.protect, &p.name) {
            ue_committed += p.committed;
            ue_ram += p.ram;
        }
        if any_matches(&cfg.ue.workers, &p.name) {
            ue.workers += 1;
            workers_committed += p.committed;
            worker_pids.push((p.pid, p.created));
        }
        if lower.starts_with("unrealeditor") {
            let cmd = win::command_line(p.pid).unwrap_or_default().to_ascii_lowercase();
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
            });
        } else if lower == "unrealbuildtool" {
            act.build = true;
        } else if lower == "unrealpak" {
            act.cook = true;
        } else if lower == "dotnet" || lower == "automationtool" {
            let cmd = win::command_line(p.pid).unwrap_or_default().to_ascii_lowercase();
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

    let groups: Vec<GroupStatus> = cfg
        .groups
        .iter()
        .map(|g| {
            let members: Vec<&Proc> =
                snap.procs.iter().filter(|p| any_matches(&g.processes, &p.name) && !is_protected(cfg, p)).collect();
            GroupStatus {
                name: g.name.clone(),
                processes: members.len(),
                ram_mb: members.iter().map(|p| p.ram).sum::<u64>() / MB,
                committed_mb: members.iter().map(|p| p.committed).sum::<u64>() / MB,
                policy: effective(g, ue_active, level),
                hard_cap_mb: g.hard_cap_mb,
                hard_cap_per_process_mb: g.hard_cap_per_process_mb,
                suspended: false,
                pids: members.iter().map(|p| p.pid).collect(),
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
    }
}

// ---------------------------------------------------------------- enforcement

pub struct Engine {
    pub cfg: Config,
    cfg_stamp: Option<SystemTime>,
    jobs: HashMap<String, win::Job>,
    last_trim: HashMap<String, Instant>,
    /// Cooldown multiplier per group, doubled each time a trim doesn't stick.
    trim_backoff: HashMap<String, u64>,
    /// pid -> CPU priority class it had before we changed it.
    cpu_touched: HashMap<u32, u32>,
    eco_touched: HashSet<u32>,
    suspended: HashSet<u32>,
    workers_paused: HashSet<u32>,
    cap_misses: HashMap<String, usize>,
    pub paused: bool,
}

impl Engine {
    pub fn new() -> Engine {
        let mut e = Engine {
            cfg: Config::builtin(),
            cfg_stamp: None,
            jobs: HashMap::new(),
            last_trim: HashMap::new(),
            trim_backoff: HashMap::new(),
            cpu_touched: HashMap::new(),
            eco_touched: HashSet::new(),
            suspended: HashSet::new(),
            workers_paused: HashSet::new(),
            cap_misses: HashMap::new(),
            paused: false,
        };
        e.reload(true);
        e
    }

    /// Re-read memcap.toml if it changed on disk.
    pub fn reload(&mut self, force: bool) {
        let path = crate::paths::config_file();
        let stamp = fs::metadata(&path).and_then(|m| m.modified()).ok();
        if !force && stamp.is_some() && stamp == self.cfg_stamp {
            return;
        }
        match Config::load() {
            Ok(cfg) => {
                if self.cfg_stamp.is_some() {
                    self.restore_all(); // undo the old settings before applying new ones
                }
                self.jobs.clear();
                for g in &cfg.groups {
                    if g.hard_cap_mb > 0 || g.hard_cap_per_process_mb > 0 {
                        let name = format!("Local\\MemCap-{}", g.name);
                        match win::Job::create(Some(&name), g.hard_cap_per_process_mb * MB, g.hard_cap_mb * MB, false) {
                            Ok(job) => {
                                self.jobs.insert(g.name.clone(), job);
                            }
                            Err(e) => crate::log::write(&format!("[{}] hard cap not applied: {}", g.name, e)),
                        }
                    }
                }
                crate::log::write(&format!("settings loaded: {} groups, every {} s", cfg.groups.len(), cfg.interval_sec));
                self.cfg = cfg;
            }
            Err(e) => crate::log::write(&format!("settings error, keeping previous settings: {}", e)),
        }
        self.cfg_stamp = fs::metadata(&path).and_then(|m| m.modified()).ok();
    }

    /// One pass: measure, decide, enforce, publish status.json.
    pub fn tick(&mut self) -> Status {
        self.reload(false);
        let snap = snapshot();
        let mut st = analyze(&self.cfg, &snap);
        st.daemon = true;
        st.paused = self.paused;
        if !self.paused {
            self.apply(&snap, &mut st);
        }
        write_status(&st);
        st
    }

    fn apply(&mut self, snap: &Snapshot, st: &mut Status) {
        let alive: HashSet<u32> = snap.procs.iter().map(|p| p.pid).collect();
        self.cpu_touched.retain(|pid, _| alive.contains(pid));
        self.eco_touched.retain(|pid| alive.contains(pid));
        self.suspended.retain(|pid| alive.contains(pid));
        self.workers_paused.retain(|pid| alive.contains(pid));

        let critical = st.pressure == Level::Critical;
        let cooldown = if critical { 15 } else { self.cfg.trim_cooldown_sec };

        for gs in st.groups.iter_mut() {
            let eff = gs.policy;
            let mut uncapped = 0usize;
            for &pid in &gs.pids {
                win::set_memory_priority(pid, eff.priority.value());

                match eff.cpu {
                    Cpu::Normal => self.restore_cpu(pid),
                    Cpu::Below | Cpu::Idle => {
                        self.cpu_touched.entry(pid).or_insert_with(|| win::cpu_class_raw(pid));
                        win::set_cpu_class(pid, if eff.cpu == Cpu::Idle { win::CpuClass::Idle } else { win::CpuClass::BelowNormal });
                        if self.eco_touched.remove(&pid) {
                            win::set_eco_mode(pid, false);
                        }
                    }
                    Cpu::Eco => {
                        if self.eco_touched.insert(pid) {
                            win::set_eco_mode(pid, true);
                        }
                    }
                }

                if let Some(job) = self.jobs.get(&gs.name) {
                    if !job.assign(pid) {
                        uncapped += 1;
                    }
                }
            }
            // Sandboxed helpers refuse to join a job. Say so once, when the count changes.
            if self.jobs.contains_key(&gs.name) && self.cap_misses.insert(gs.name.clone(), uncapped) != Some(uncapped) {
                crate::log::write(&format!(
                    "[{}] hard cap covers {} of {} processes{}",
                    gs.name,
                    gs.pids.len() - uncapped,
                    gs.pids.len(),
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
                    for &pid in &gs.pids {
                        win::trim(pid);
                    }
                    let next = match since {
                        Some(s) if !critical && s < cooldown * backoff * 3 => (backoff * 2).min(16),
                        _ => 1,
                    };
                    self.trim_backoff.insert(gs.name.clone(), next);
                    self.last_trim.insert(gs.name.clone(), Instant::now());
                    crate::log::write(&format!(
                        "[{}] RAM {} MB > {} MB ({}, {}): trimmed {} processes",
                        gs.name,
                        gs.ram_mb,
                        eff.trim_above_mb,
                        st.mode,
                        st.pressure.label(),
                        gs.pids.len()
                    ));
                }
            }

            if eff.suspend {
                for &pid in &gs.pids {
                    if !self.suspended.contains(&pid) && win::suspend(pid) {
                        self.suspended.insert(pid);
                    }
                }
                gs.suspended = true;
            } else {
                for &pid in &gs.pids {
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

    fn restore_cpu(&mut self, pid: u32) {
        if let Some(orig) = self.cpu_touched.remove(&pid) {
            if orig != 0 {
                win::set_cpu_class_raw(pid, orig);
            } else {
                win::set_cpu_class(pid, win::CpuClass::Normal);
            }
        }
        if self.eco_touched.remove(&pid) {
            win::set_eco_mode(pid, false);
        }
    }

    /// Put every app back the way we found it: normal memory priority, original CPU
    /// class, nothing suspended. Called on pause, exit and settings reload.
    pub fn restore_all(&mut self) {
        for pid in self.suspended.drain().chain(self.workers_paused.drain()) {
            win::resume(pid);
        }
        let pids: Vec<u32> = self.cpu_touched.keys().copied().chain(self.eco_touched.iter().copied()).collect();
        for pid in pids {
            self.restore_cpu(pid);
        }
        let procs = win::list_processes();
        for g in &self.cfg.groups {
            for p in procs.iter().filter(|p| any_matches(&g.processes, &p.name) && !is_protected(&self.cfg, p)) {
                win::set_memory_priority(p.pid, MemPriority::Normal.value());
            }
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.restore_all();
        }
        crate::log::write(if paused { "paused, apps restored to normal" } else { "resumed" });
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// Publish the latest status for agents and `memcap status`. Written atomically.
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
        let idle = effective(browsers, false, Level::Normal);
        assert_eq!((idle.priority, idle.trim_above_mb, idle.cpu), (MemPriority::Low, 6144, Cpu::Normal));
        let ue = effective(browsers, true, Level::Normal);
        assert_eq!(ue.trim_above_mb, 3072);
        let elevated = effective(browsers, true, Level::Elevated);
        assert_eq!((elevated.trim_above_mb, elevated.cpu), (1024, Cpu::Below));
        let critical = effective(browsers, true, Level::Critical);
        assert_eq!((critical.trim_above_mb, critical.cpu, critical.suspend), (256, Cpu::Below, false));
    }

    #[test]
    fn ue_processes_are_protected_even_if_listed() {
        let mut cfg = Config::builtin();
        cfg.groups[0].processes.push("UnrealEditor".into());
        let p = |name: &str| Proc { pid: 4242, ppid: 1, name: name.into(), ram: 0, committed: 0, created: 0 };
        assert!(is_protected(&cfg, &p("UnrealEditor")));
        assert!(is_protected(&cfg, &p("UnrealEditor-Cmd")));
        assert!(is_protected(&cfg, &p("ShaderCompileWorker")));
        assert!(!is_protected(&cfg, &p("chrome")));
    }
}

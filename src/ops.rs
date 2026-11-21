//! One-shot operations for agents and scripts: prime, wait, run, advise.

use std::collections::{HashMap, HashSet};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{Config, Cpu, MemPriority, Patterns, Target};
use crate::engine::{analyze, snapshot, Engine, Event, Level, Status, SystemStatus};
use crate::{ipc, paths, win};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const CREATE_SUSPENDED: u32 = 0x0000_0004;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Scheduled-task name used when the user opts into `general.elevated_helper`.
pub const ELEVATED_HELPER_TASK: &str = "AtherMemGuard-ElevatedHelper";

// ---------------------------------------------------------------- prime

#[derive(Debug, Serialize)]
pub struct Orphan {
    pub pid: u32,
    pub name: String,
    pub committed_mb: u64,
    pub killed: bool,
}

#[derive(Debug, Serialize)]
pub struct Readiness {
    pub target: String,
    pub need_ram_gb: f64,
    pub need_commit_gb: f64,
    pub ready: bool,
}

#[derive(Debug, Serialize)]
pub struct PrimeReport {
    pub before: SystemStatus,
    pub after: SystemStatus,
    pub trimmed_processes: usize,
    pub ram_freed_gb: f64,
    /// RAM held by the app groups before and after. Trimmed memory shows up as free
    /// RAM over the next seconds, as Windows writes it out.
    pub groups_ram_before_gb: f64,
    pub groups_ram_after_gb: f64,
    pub orphans: Vec<Orphan>,
    pub readiness: Option<Readiness>,
    pub advice: Vec<String>,
}

/// Unreal helper processes whose parent has exited: left over from a crashed or killed
/// editor, build or cook, and still holding memory.
fn find_orphans<'a>(cfg: &Config, procs: &'a [win::Proc]) -> Vec<&'a win::Proc> {
    let workers = Patterns::new(&cfg.ue.workers);
    let by_pid: HashMap<u32, &win::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    procs
        .iter()
        .filter(|p| {
            let lower = p.name.to_ascii_lowercase();
            let candidate = workers.matches_lower(&lower)
                || lower.starts_with("unrealeditor-cmd")
                || lower.starts_with("crashreportclient");
            // One we can't open we couldn't end either (and 0.2.0 never saw its parent link).
            if !candidate || !p.queryable || p.created == 0 {
                return false;
            }
            match by_pid.get(&p.ppid) {
                None => true,
                // A parent created after its "child" is a recycled PID, not the real parent.
                Some(parent) => parent.created > p.created,
            }
        })
        .collect()
}

/// Give every enabled group's processes the lowest memory priority the group ever gets and
/// trim them, whatever the current pressure. Returns how many processes were trimmed. Used
/// by `prime` and by the engine's auto-prime.
pub fn trim_groups(cfg: &Config, procs: &[win::Proc]) -> usize {
    let protect = Patterns::new(&cfg.ue.protect);
    let lowers: Vec<String> = procs.iter().map(|p| p.name.to_ascii_lowercase()).collect();
    let me = win::current_pid();
    let mut claimed: HashSet<u32> = HashSet::new();
    let mut trimmed = 0usize;
    for g in cfg.active_groups() {
        let pats = Patterns::new(&g.processes);
        let lowest = [&g.critical, &g.elevated, &g.ue]
            .iter()
            .filter_map(|p| p.as_ref().and_then(|p| p.priority))
            .chain(g.priority)
            .min_by_key(|p| p.value())
            .unwrap_or(MemPriority::Low);
        for (p, lower) in procs.iter().zip(&lowers) {
            if !p.queryable || p.pid == me || lower.contains("memguard") || protect.matches_lower(lower) {
                continue;
            }
            if pats.matches_lower(lower) && claimed.insert(p.pid) {
                win::set_memory_priority(p.pid, lowest.value());
                if win::trim(p.pid) {
                    trimmed += 1;
                }
            }
        }
    }
    trimmed
}

/// What freeing one group did.
#[derive(Debug, Clone, Serialize)]
pub struct GroupFreeReport {
    pub group: String,
    pub trimmed_processes: usize,
    pub ram_free_before_gb: f64,
    pub ram_free_after_gb: f64,
}

/// Free memory from one app group now: its processes get the group's lowest memory priority
/// and are trimmed (the flyout's "Free memory from {group} now"). Blocks a moment so the
/// trimmed memory shows up as free RAM.
pub fn free_group(cfg: &Config, group: &str) -> GroupFreeReport {
    let mut one = cfg.clone();
    one.groups.retain(|g| g.name == group);
    let gb = |b: u64| (b as f64 / GB * 10.0).round() / 10.0;
    let before = win::sys_info();
    let trimmed = match win::try_list_processes() {
        Ok(procs) => trim_groups(&one, &procs),
        Err(e) => {
            crate::log::write(&format!("free group [{}]: {}", group, e));
            0
        }
    };
    sleep(Duration::from_millis(800));
    let after = win::sys_info();
    GroupFreeReport { group: group.to_string(), trimmed_processes: trimmed, ram_free_before_gb: gb(before.ram_avail), ram_free_after_gb: gb(after.ram_avail) }
}

/// Outcome of an OS-level memory relief request (standby purge / page combining).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OsReliefStatus {
    Disabled,
    ElevatedDirect,
    HelperDispatched,
    HelperNotInstalled,
    NeedsAdmin,
}

#[derive(Debug, Clone, Serialize)]
pub struct OsReliefReport {
    pub status: OsReliefStatus,
    pub standby_purged: bool,
    pub pages_combined: u64,
}

/// Perform low-priority standby purge and/or physical page combining when elevated, or trigger
/// the opt-in scheduled-task helper when `general.elevated_helper = true`.
/// Strictly zero-admin and zero-UAC by default: never calls `win::run_elevated`.
pub fn relieve_os_memory(cfg: &Config, purge_standby: bool, combine_pages: bool) -> OsReliefReport {
    if win::is_elevated() {
        let mut standby_purged = false;
        let mut pages_combined = 0u64;
        if purge_standby {
            match win::purge_low_priority_standby() {
                Ok(()) => standby_purged = true,
                Err(e) => crate::log::write(&format!("os relief: standby purge: {}", e)),
            }
        }
        if combine_pages {
            match win::combine_physical_memory() {
                Ok(n) => pages_combined = n,
                Err(e) => crate::log::write(&format!("os relief: combine memory: {}", e)),
            }
        }
        return OsReliefReport { status: OsReliefStatus::ElevatedDirect, standby_purged, pages_combined };
    }
    if !cfg.general.elevated_helper {
        return OsReliefReport { status: OsReliefStatus::Disabled, standby_purged: false, pages_combined: 0 };
    }
    if cfg!(test) || crate::paths::is_test_home() {
        return OsReliefReport { status: OsReliefStatus::HelperNotInstalled, standby_purged: false, pages_combined: 0 };
    }
    let ok = Command::new("schtasks.exe")
        .args(["/Run", "/TN", ELEVATED_HELPER_TASK])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    OsReliefReport {
        status: if ok { OsReliefStatus::HelperDispatched } else { OsReliefStatus::HelperNotInstalled },
        standby_purged: false,
        pages_combined: 0,
    }
}

/// Free as much RAM as possible for Unreal right now: every group gets its lowest
/// memory priority and is trimmed, whatever the current pressure level.
pub fn prime(cfg: &Config, target: Option<&str>, kill_orphans: bool) -> PrimeReport {
    let before_snap = snapshot();
    let before = SystemStatus::from(&before_snap.sys);
    let groups_gb = |st: &crate::engine::Status| (st.groups.iter().map(|g| g.ram_mb).sum::<u64>() as f64 / 102.4).round() / 10.0;
    let groups_ram_before_gb = groups_gb(&analyze(cfg, &before_snap));

    let trimmed = trim_groups(cfg, &before_snap.procs);
    if win::is_elevated() || cfg.general.elevated_helper {
        let relief = relieve_os_memory(cfg, true, true);
        if relief.standby_purged || relief.pages_combined > 0 {
            crate::log::write(&format!(
                "prime: OS relief standby_purged={} pages_combined={}",
                relief.standby_purged, relief.pages_combined
            ));
        }
    }

    let mut orphans = Vec::new();
    for p in find_orphans(cfg, &before_snap.procs) {
        let killed = kill_orphans && win::terminate(p.pid);
        orphans.push(Orphan { pid: p.pid, name: p.name.clone(), committed_mb: p.committed / (1024 * 1024), killed });
    }

    // Trimmed pages take a moment to leave the working sets.
    sleep(Duration::from_millis(1500));
    let after_snap = snapshot();
    let after = SystemStatus::from(&after_snap.sys);
    let st = analyze(cfg, &after_snap);
    let groups_ram_after_gb = groups_gb(&st);

    let readiness = target.and_then(|name| {
        cfg.prime.target(name).map(|t| Readiness {
            target: name.to_string(),
            need_ram_gb: t.ram_gb,
            need_commit_gb: t.commit_gb,
            ready: after.ram_free_gb >= t.ram_gb && after.commit_free_gb >= t.commit_gb,
        })
    });

    let mut advice = st.advice;
    if !orphans.is_empty() && !kill_orphans {
        advice.push(format!(
            "{} leftover Unreal helper processes have no parent. Run `memguard prime --kill-orphans` to end them.",
            orphans.len()
        ));
    }
    crate::log::write(&format!(
        "prime: trimmed {} processes, RAM free {:.1} -> {:.1} GB, commit free {:.1} GB",
        trimmed, before.ram_free_gb, after.ram_free_gb, after.commit_free_gb
    ));

    PrimeReport {
        ram_freed_gb: ((after.ram_free_gb - before.ram_free_gb) * 10.0).round() / 10.0,
        before,
        after,
        trimmed_processes: trimmed,
        groups_ram_before_gb,
        groups_ram_after_gb,
        orphans,
        readiness,
        advice,
    }
}

// ---------------------------------------------------------------- Windows file cache

/// How emptying Windows' file cache went.
#[derive(Debug, Clone, Serialize)]
pub struct CacheFlushReport {
    pub ok: bool,
    /// The person said no to the Windows administrator prompt.
    pub declined: bool,
    /// What went wrong, in words.
    pub error: Option<String>,
    pub ram_free_before_gb: f64,
    pub ram_free_after_gb: f64,
    pub file_cache_before_gb: f64,
    pub file_cache_after_gb: f64,
}

impl CacheFlushReport {
    /// RAM that came back, never negative.
    pub fn freed_gb(&self) -> f64 {
        ((self.ram_free_after_gb - self.ram_free_before_gb) * 10.0).round().max(0.0) / 10.0
    }
}

/// Empty the file cache, then write out the modified list, in this process. Needs
/// administrator rights. The modified list is best effort: only the file cache failing is
/// an error.
pub fn flush_cache_here() -> Result<(), String> {
    win::flush_file_cache()?;
    if let Err(e) = win::flush_modified_list() {
        crate::log::write(&format!("flush cache: modified list: {}", e));
    }
    if let Err(e) = win::purge_low_priority_standby() {
        crate::log::write(&format!("flush cache: standby purge: {}", e));
    }
    match win::combine_physical_memory() {
        Ok(pages) if pages > 0 => crate::log::write(&format!("flush cache: combined {} pages", pages)),
        Ok(_) => {}
        Err(e) => crate::log::write(&format!("flush cache: combine memory: {}", e)),
    }
    Ok(())
}

/// Empty Windows' file cache and say what came back. When this process is elevated it does
/// it itself; otherwise it starts `exe args` with administrator rights (Windows asks first),
/// which must call `flush_cache_here` and exit 0 when that worked. Blocks until done.
pub fn flush_cache(exe: &std::path::Path, args: &str) -> CacheFlushReport {
    let gb = |b: u64| (b as f64 / GB * 10.0).round() / 10.0;
    let before = win::sys_info();
    use win::ElevateError;
    let result = if win::is_elevated() {
        flush_cache_here().map_err(ElevateError::Failed)
    } else {
        match win::run_elevated(exe, args, 120_000) {
            Ok(0) => Ok(()),
            Ok(code) => Err(ElevateError::Failed(format!("Windows didn't let it empty the cache (exit code {}; details in the log)", code))),
            Err(e) => Err(e),
        }
    };
    // The cache's pages move to the standby list at once; give the counters a moment.
    if result.is_ok() {
        sleep(Duration::from_millis(500));
    }
    let after = win::sys_info();
    CacheFlushReport {
        ok: result.is_ok(),
        declined: result == Err(ElevateError::Declined),
        error: match result {
            Ok(()) => None,
            Err(ElevateError::Declined) => Some("administrator permission was declined".into()),
            Err(ElevateError::Failed(e)) => Some(e),
        },
        ram_free_before_gb: gb(before.ram_avail),
        ram_free_after_gb: gb(after.ram_avail),
        file_cache_before_gb: gb(before.resident_system_cache),
        file_cache_after_gb: gb(after.resident_system_cache),
    }
}

// ---------------------------------------------------------------- wait

#[derive(Debug, Serialize)]
pub struct WaitResult {
    pub ready: bool,
    pub waited_sec: u64,
    pub ram_free_gb: f64,
    pub commit_free_gb: f64,
    pub need_ram_gb: f64,
    pub need_commit_gb: f64,
    /// What stopped it: "disk" (returned at once: disk doesn't free itself) or "memory"
    /// (the timeout passed). Empty when ready.
    pub blocked_by: Vec<String>,
    /// The project's volume, its free space (available to this user) and the need, when a
    /// disk need was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_volume: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub need_disk_gb: Option<f64>,
    /// Every volume checked, once each (the project's, and those of the DDC, Zen and TEMP).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disk_checks: Vec<crate::disk::DiskCheck>,
}

impl WaitResult {
    /// Not ready because of the disk: returned at once, before priming or waiting.
    pub fn blocked_by_disk(need: Target, checks: Vec<crate::disk::DiskCheck>) -> WaitResult {
        let sys = SystemStatus::from(&win::sys_info());
        let mut r = WaitResult {
            ready: false,
            waited_sec: 0,
            ram_free_gb: sys.ram_free_gb,
            commit_free_gb: sys.commit_free_gb,
            need_ram_gb: need.ram_gb,
            need_commit_gb: need.commit_gb,
            blocked_by: vec!["disk".into()],
            disk_volume: None,
            disk_free_gb: None,
            need_disk_gb: None,
            disk_checks: Vec::new(),
        };
        r.set_disk(checks);
        r
    }

    pub fn set_disk(&mut self, checks: Vec<crate::disk::DiskCheck>) {
        if let Some(p) = checks.iter().find(|c| c.roles.iter().any(|r| r == "project")).or_else(|| checks.iter().find(|c| !c.ok)) {
            self.disk_volume = Some(p.volume.clone());
            self.disk_free_gb = Some(p.free_gb);
            self.need_disk_gb = Some(p.need_gb);
        }
        self.disk_checks = checks;
    }
}

/// The disk part of a wait, each volume checked once. `None` when there is no disk need
/// (no `disk_gb` on the target or the command line, or `[disk] enabled = false`).
pub fn disk_gate(cfg: &Config, disk_path: Option<&str>, disk_gb: Option<f64>) -> Option<Vec<crate::disk::DiskCheck>> {
    let gb = disk_gb.filter(|g| *g > 0.0)?;
    if !cfg.disk().enabled {
        return None;
    }
    Some(crate::disk::gate(cfg, disk_path, gb))
}

/// Block until the PC has the requested headroom, or the timeout passes.
pub fn wait_for(need: Target, timeout: Duration, mut on_poll: impl FnMut(&SystemStatus)) -> WaitResult {
    let start = Instant::now();
    loop {
        let sys = SystemStatus::from(&win::sys_info());
        let ready = sys.ram_free_gb >= need.ram_gb && sys.commit_free_gb >= need.commit_gb;
        if ready || start.elapsed() >= timeout {
            return WaitResult {
                ready,
                waited_sec: start.elapsed().as_secs(),
                ram_free_gb: sys.ram_free_gb,
                commit_free_gb: sys.commit_free_gb,
                need_ram_gb: need.ram_gb,
                need_commit_gb: need.commit_gb,
                blocked_by: if ready { Vec::new() } else { vec!["memory".into()] },
                disk_volume: None,
                disk_free_gb: None,
                need_disk_gb: None,
                disk_checks: Vec::new(),
            };
        }
        on_poll(&sys);
        sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------- run

pub struct RunOptions {
    /// Hard cap on committed memory for the command and everything it starts. 0 = none.
    pub hard_cap_gb: f64,
    /// Hard cap per process. 0 = none.
    pub per_process_cap_gb: f64,
    pub priority: Option<MemPriority>,
    pub cpu: Option<Cpu>,
}

/// Run a command inside a Job Object, wait for it and return its exit code.
/// Children are in the job too, and all of them end if memguard itself is killed.
pub fn run(opts: &RunOptions, command: &[String]) -> Result<i32, String> {
    run_with_hooks(opts, command, |job, h| job.assign_handle(h), win::resume_handle)
}

fn run_with_hooks(
    opts: &RunOptions,
    command: &[String],
    assign: impl FnOnce(&win::Job, windows_sys::Win32::Foundation::HANDLE) -> bool,
    resume: impl FnOnce(windows_sys::Win32::Foundation::HANDLE) -> bool,
) -> Result<i32, String> {
    if command.is_empty() {
        return Err("no command specified".into());
    }
    let job = win::Job::create(
        None,
        (opts.per_process_cap_gb.max(0.0) * GB) as u64,
        (opts.hard_cap_gb.max(0.0) * GB) as u64,
        true,
    )?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .map_err(|e| format!("could not start {}: {}", command[0], e))?;
    let handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;

    // Assign before any priority change or resume so no instruction can execute outside the job.
    if !assign(&job, handle) {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not place the command in a job object, so the cap would not hold; it was not run".into());
    }
    if let Some(p) = opts.priority {
        win::set_memory_priority(child.id(), p.value());
    }
    match opts.cpu {
        Some(Cpu::Below) => drop(win::set_cpu_class(child.id(), win::CpuClass::BelowNormal)),
        Some(Cpu::Idle) => drop(win::set_cpu_class(child.id(), win::CpuClass::Idle)),
        Some(Cpu::Eco) => drop(win::set_eco_qos(child.id(), true)),
        _ => {}
    }
    if !resume(handle) {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not resume the command after placing it in a job object".into());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok(status.code().unwrap_or(1))
}

// ---------------------------------------------------------------- advise

#[derive(Debug, Serialize)]
pub struct Advice {
    pub mode: String,
    pub pressure: Level,
    pub logical_cores: usize,
    /// RAM Unreal could get right now: free RAM plus what the capped groups hold.
    pub ram_budget_gb: f64,
    pub commit_free_gb: f64,
    /// Estimate, assuming about 3 GB per compile action.
    pub suggested_max_parallel_actions: usize,
    pub notes: Vec<String>,
}

pub fn advise(cfg: &Config) -> Advice {
    let snap = snapshot();
    let st = analyze(cfg, &snap);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let reclaimable_gb: f64 = st.groups.iter().map(|g| g.ram_mb as f64 / 1024.0).sum();
    let ram_budget = st.system.ram_free_gb + reclaimable_gb;
    let budget = ram_budget.min(st.system.commit_free_gb);
    const GB_PER_ACTION: f64 = 3.0;
    let actions = ((budget / GB_PER_ACTION).floor() as usize).clamp(2, cores);

    let mut notes = st.advice.clone();
    if actions < cores {
        notes.push(format!(
            "A {}-wide compile could need about {:.0} GB at {:.0} GB per action, and {:.0} GB is available. Limit UnrealBuildTool with -MaxParallelActions={} or <MaxParallelActions>{}</MaxParallelActions> in BuildConfiguration.xml.",
            cores,
            cores as f64 * GB_PER_ACTION,
            GB_PER_ACTION,
            budget,
            actions,
            actions
        ));
    }
    if st.activities.editor && (st.activities.build || st.activities.cook) {
        notes.push("The editor is open during a build or cook. Closing it first gives the build its memory.".to_string());
    }
    Advice {
        mode: st.mode,
        pressure: st.pressure,
        logical_cores: cores,
        ram_budget_gb: (ram_budget * 10.0).round() / 10.0,
        commit_free_gb: st.system.commit_free_gb,
        suggested_max_parallel_actions: actions,
        notes,
    }
}

// ---------------------------------------------------------------- status formatting & headless daemon

pub fn status_text(st: &Status) -> String {
    let mut s = format!(
        "Mode: {}    Pressure: {}{}\n\nRAM: {:.1} GB free of {:.1} GB\nCommit: {:.1} GB free of {:.1} GB\n",
        st.mode,
        st.pressure.label(),
        if st.paused { "    (PAUSED)" } else { "" },
        st.system.ram_free_gb,
        st.system.ram_total_gb,
        st.system.commit_free_gb,
        st.system.commit_limit_gb
    );
    let r = &st.ram;
    if r.in_use_gb > 0.0 {
        let mut parts: Vec<(String, f64)> = r.parts().into_iter().map(|(p, gb)| (r.label(p), gb)).collect();
        parts.push(("Unreal".into(), r.unreal_gb));
        parts.sort_by(|a, b| b.1.total_cmp(&a.1));
        let list: Vec<String> = parts.iter().filter(|p| p.1 >= 0.05).map(|(n, gb)| format!("{} {:.1}", n, gb)).collect();
        s.push_str(&format!("RAM in use: {:.1} GB = {} (GB)\n", r.in_use_gb, list.join(", ")));
    }
    if !st.ue.processes.is_empty() || st.ue.workers > 0 {
        s.push_str("\nUnreal\n");
        for p in &st.ue.processes {
            s.push_str(&format!(
                "  {} ({}, PID {}): {:.1} GB committed, {:.1} GB RAM, up {} h {} min\n",
                p.name,
                p.kind,
                p.pid,
                p.committed_gb,
                p.ram_gb,
                p.uptime_min / 60,
                p.uptime_min % 60
            ));
        }
        if st.ue.workers > 0 {
            s.push_str(&format!(
                "  {} compile/shader workers, {:.1} GB committed{}\n",
                st.ue.workers,
                st.ue.workers_committed_gb,
                if st.ue.workers_paused > 0 { format!(", {} paused", st.ue.workers_paused) } else { String::new() }
            ));
        }
    }
    s.push_str("\nGroups\n");
    for g in &st.groups {
        s.push_str(&format!(
            "  {}: {} processes, {} MB RAM, {} MB committed{}\n",
            g.name,
            g.processes,
            g.ram_mb,
            g.committed_mb,
            if g.suspended { ", suspended" } else { "" }
        ));
    }
    if !st.advice.is_empty() {
        s.push_str("\nAdvice\n");
        for a in &st.advice {
            s.push_str(&format!("  - {}\n", a));
        }
    }
    s
}

fn headless_tick(engine: &mut Engine, status_pub: &mut ipc::StatusPublisher, print_line: bool) -> u64 {
    let st = engine.tick();
    let _ = engine.take_events();
    status_pub.publish(&st);
    engine.clear_last_procs();
    if print_line {
        println!(
            "{}  {}  pressure {}  RAM free {:.1} GB  commit free {:.1} GB",
            st.time,
            st.mode,
            st.pressure.label(),
            st.system.ram_free_gb,
            st.system.commit_free_gb
        );
    }
    engine.cfg.interval_sec.max(1)
}

/// Headless event-driven loop for `memguard daemon`: same engine, no tray icon, logs to the console.
pub fn run_headless() {
    let Some(_instance) = win::acquire_instance(paths::mutex(), 8000) else {
        eprintln!("Ather MemGuard is already running.");
        return;
    };
    let exit_event = win::create_event(paths::exit_event());
    let low_mem_event = win::low_memory_event();
    let control = ipc::ControlServer::start(None);
    let mut status_pub = ipc::StatusPublisher::new();
    let mut engine = Engine::new();
    crate::log::write(&format!("Ather MemGuard {} started (headless)", env!("CARGO_PKG_VERSION")));

    let interval = headless_tick(&mut engine, &mut status_pub, true);
    let mut next_tick_at = Instant::now() + Duration::from_secs(interval);
    let mut low_mem_suppress_until: Option<Instant> = None;

    #[derive(Clone, Copy)]
    enum WaitKind {
        Exit,
        Control,
        ConfigWatch,
        LowMem,
    }

    loop {
        if exit_event.as_ref().is_some_and(win::event_is_set) {
            engine.restore_all();
            engine.flush_history();
            crate::log::write("Ather MemGuard stopped, apps restored to normal");
            return;
        }

        if let Some(ctrl) = control.as_ref() {
            while let Some(req) = ctrl.try_recv() {
                match req.cmd {
                    ipc::ControlCmd::Pause { minutes } => {
                        if let Some(m) = minutes {
                            engine.pause_for(Duration::from_secs(m.saturating_mul(60)));
                        } else {
                            engine.set_paused(true);
                        }
                        headless_tick(&mut engine, &mut status_pub, false);
                        req.respond(Ok(()));
                    }
                    ipc::ControlCmd::Resume => {
                        engine.set_paused(false);
                        headless_tick(&mut engine, &mut status_pub, false);
                        req.respond(Ok(()));
                    }
                    ipc::ControlCmd::Reload => {
                        engine.mark_config_dirty();
                        engine.reload(true);
                        let _ = engine.take_events();
                        let sec = headless_tick(&mut engine, &mut status_pub, false);
                        next_tick_at = Instant::now() + Duration::from_secs(sec);
                        req.respond(Ok(()));
                    }
                }
            }
        }

        let now = Instant::now();
        if now >= next_tick_at {
            let sec = headless_tick(&mut engine, &mut status_pub, true);
            next_tick_at = Instant::now() + Duration::from_secs(sec);
            continue;
        }
        if engine.pause_until().is_some_and(|u| now >= u) {
            headless_tick(&mut engine, &mut status_pub, false);
            continue;
        }

        let mut handles: Vec<win::WaitHandle> = Vec::with_capacity(4);
        let mut kinds: Vec<WaitKind> = Vec::with_capacity(4);
        if let Some(h) = &exit_event {
            handles.push(h.wait_handle());
            kinds.push(WaitKind::Exit);
        }
        if let Some(c) = &control {
            handles.push(c.event_wait_handle());
            kinds.push(WaitKind::Control);
        }
        if let Some(w) = engine.config_watch_wait() {
            handles.push(w);
            kinds.push(WaitKind::ConfigWatch);
        }
        if let Some(h) = &low_mem_event {
            if low_mem_suppress_until.is_some_and(|t| now >= t) {
                if win::event_is_set(h) {
                    low_mem_suppress_until = Some(now + Duration::from_secs(engine.cfg.interval_sec.max(5)));
                } else {
                    low_mem_suppress_until = None;
                }
            }
            if low_mem_suppress_until.is_none() {
                handles.push(h.wait_handle());
                kinds.push(WaitKind::LowMem);
            }
        }

        let mut wake_at = next_tick_at;
        if let Some(rem) = engine.pause_remaining(now) {
            wake_at = wake_at.min(now + rem);
        }
        if let Some(t) = low_mem_suppress_until {
            wake_at = wake_at.min(t);
        }
        let timeout_ms = wake_at.saturating_duration_since(now).as_millis().min(u32::MAX as u128 - 1) as u32;

        match win::wait_any(&handles, timeout_ms) {
            win::WaitOutcome::Signaled(idx) => match kinds.get(idx).copied() {
                Some(WaitKind::Exit) => {
                    engine.restore_all();
                    engine.flush_history();
                    crate::log::write("Ather MemGuard stopped, apps restored to normal");
                    return;
                }
                Some(WaitKind::Control) => {}
                Some(WaitKind::ConfigWatch) => {
                    engine.mark_config_dirty();
                    engine.reload(false);
                    let events = engine.take_events();
                    if events.iter().any(|e| matches!(e, Event::SettingsReloaded)) {
                        let sec = headless_tick(&mut engine, &mut status_pub, false);
                        next_tick_at = Instant::now() + Duration::from_secs(sec);
                    }
                }
                Some(WaitKind::LowMem) => {
                    low_mem_suppress_until = Some(Instant::now() + Duration::from_secs(engine.cfg.interval_sec.max(5)));
                    headless_tick(&mut engine, &mut status_pub, false);
                }
                None => {}
            },
            win::WaitOutcome::Timeout | win::WaitOutcome::Message => {}
            win::WaitOutcome::Failed => sleep(Duration::from_millis(250)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn run_rejects_empty_command() {
        let opts = RunOptions { hard_cap_gb: 0.0, per_process_cap_gb: 0.0, priority: None, cpu: None };
        let err = run(&opts, &[]).unwrap_err();
        assert!(err.contains("no command"), "{}", err);
    }

    #[test]
    fn run_with_hooks_aborts_before_resume_when_assign_fails() {
        let dir = std::env::temp_dir().join(format!("amg_ops_bug10_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let marker = dir.join("ran.txt");
        let _ = std::fs::remove_file(&marker);

        let resume_called = AtomicBool::new(false);
        let opts = RunOptions { hard_cap_gb: 0.5, per_process_cap_gb: 0.25, priority: None, cpu: None };
        let cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            format!("echo RAN > \"{}\"", marker.display()),
        ];
        let res = run_with_hooks(
            &opts,
            &cmd,
            |_job, _h| false,
            |_h| {
                resume_called.store(true, Ordering::SeqCst);
                true
            },
        );
        assert!(res.is_err());
        assert!(!resume_called.load(Ordering::SeqCst), "Bug #10: resume must never be called when assign fails");
        assert!(!marker.exists(), "Bug #10: command must never execute when assign fails");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_with_hooks_aborts_when_resume_fails() {
        let opts = RunOptions { hard_cap_gb: 0.0, per_process_cap_gb: 0.0, priority: None, cpu: None };
        let cmd = vec!["cmd.exe".to_string(), "/c".to_string(), "exit".to_string(), "0".to_string()];
        let res = run_with_hooks(&opts, &cmd, |job, h| job.assign_handle(h), |_h| false);
        assert!(res.is_err());
    }

    #[test]
    fn run_with_hooks_succeeds_when_assign_and_resume_pass() {
        let opts = RunOptions { hard_cap_gb: 0.0, per_process_cap_gb: 0.0, priority: None, cpu: Some(Cpu::Eco) };
        let cmd = vec!["cmd.exe".to_string(), "/c".to_string(), "exit".to_string(), "0".to_string()];
        let res = run_with_hooks(&opts, &cmd, |job, h| job.assign_handle(h), win::resume_handle);
        assert_eq!(res, Ok(0));
    }

    #[test]
    fn relieve_os_memory_is_zero_admin_safe() {
        let mut cfg = Config::builtin();
        cfg.general.elevated_helper = false;
        let rep = relieve_os_memory(&cfg, true, true);
        if win::is_elevated() {
            assert_eq!(rep.status, OsReliefStatus::ElevatedDirect);
        } else {
            assert_eq!(rep.status, OsReliefStatus::Disabled);
            assert!(!rep.standby_purged);
            assert_eq!(rep.pages_combined, 0);
        }

        cfg.general.elevated_helper = true;
        let rep2 = relieve_os_memory(&cfg, true, true);
        if win::is_elevated() {
            assert_eq!(rep2.status, OsReliefStatus::ElevatedDirect);
        } else {
            assert_eq!(rep2.status, OsReliefStatus::HelperNotInstalled);
        }
    }

    #[test]
    fn trim_groups_deduplicates_overlapping_groups() {
        let mut child = Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn ping");
        let pid = child.id();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut cfg = Config::builtin();
            let mut g1 = cfg.groups[0].clone();
            g1.name = "Overlap1".into();
            g1.processes = vec!["ping".into()];
            let mut g2 = g1.clone();
            g2.name = "Overlap2".into();
            cfg.groups = vec![g1, g2];
            let procs: Vec<win::Proc> = win::list_processes().into_iter().filter(|p| p.pid == pid).collect();
            assert_eq!(procs.len(), 1);
            assert_eq!(trim_groups(&cfg, &procs), 1);
        }));
        let _ = child.kill();
        let _ = child.wait();
        if let Err(e) = res {
            std::panic::resume_unwind(e);
        }
    }
}

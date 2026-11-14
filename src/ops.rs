//! One-shot operations for agents and scripts: prime, wait, run, advise.

use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{any_matches, Config, Cpu, MemPriority, Target};
use crate::engine::{analyze, is_protected, snapshot, Level, SystemStatus};
use crate::win;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const CREATE_SUSPENDED: u32 = 0x0000_0004;

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
    let by_pid: HashMap<u32, &win::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
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
                // A parent created after its "child" is a recycled PID, not the real parent.
                Some(parent) => parent.created > p.created,
            }
        })
        .collect()
}

//// Give every enabled group's processes the lowest memory priority the group ever gets and
/// trim them, whatever the current pressure. Returns how many processes were trimmed. Used
/// by `prime` and by the engine's auto-prime.
pub fn trim_groups(cfg: &Config, procs: &[win::Proc]) -> usize {
    let mut trimmed = 0usize;
    for g in cfg.active_groups() {
        let lowest = [&g.critical, &g.elevated, &g.ue]
            .iter()
            .filter_map(|p| p.as_ref().and_then(|p| p.priority))
            .chain(g.priority)
            .min_by_key(|p| p.value())
            .unwrap_or(MemPriority::Low);
        for p in procs.iter().filter(|p| p.queryable && any_matches(&g.processes, &p.name) && !is_protected(cfg, p)) {
            win::set_memory_priority(p.pid, lowest.value());
            if win::trim(p.pid) {
                trimmed += 1;
            }
        }
    }
    trimmed
}

/// Free as much RAM as possible for Unreal right now: every group gets its lowest
/// memory priority and is trimmed, whatever the current pressure level.
pub fn prime(cfg: &Config, target: Option<&str>, kill_orphans: bool) -> PrimeReport {
    let before_snap = snapshot();
    let before = SystemStatus::from(&before_snap.sys);
    let groups_gb = |st: &crate::engine::Status| (st.groups.iter().map(|g| g.ram_mb).sum::<u64>() as f64 / 102.4).round() / 10.0;
    let groups_ram_before_gb = groups_gb(&analyze(cfg, &before_snap));

    let trimmed = trim_groups(cfg, &before_snap.procs);

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
    Ok(())
}

/// Empty Windows' file cache and say what came back. When this process is elevated it does
/// it itself; otherwise it starts `exe args` with administrator rights (Windows asks first),
/// which must call `flush_cache_here` and exit 0 when that worked. Blocks until done.
pub fn flush_cache(exe: &std::path::Path, args: &str) -> CacheFlushReport {
    let gb = |b: u64| (b as f64 / GB * 10.0).round() / 10.0;
    let before = win::sys_info();
    let result = if win::is_elevated() {
        flush_cache_here().map_err(|e| (false, e))
    } else {
        match win::run_elevated(exe, args, 120_000) {
            Ok(0) => Ok(()),
            Ok(code) => Err((false, format!("Windows didn't let it empty the cache (exit code {}; details in the log)", code))),
            Err(win::ElevateError::Declined) => Err((true, "administrator permission was declined".to_string())),
            Err(win::ElevateError::Failed(e)) => Err((false, e)),
        }
    };
    // The cache's pages move to the standby list at once; give the counters a moment.
    if result.is_ok() {
        sleep(Duration::from_millis(500));
    }
    let after = win::sys_info();
    let (declined, error) = match result {
        Ok(()) => (false, None),
        Err((d, e)) => (d, Some(e)),
    };
    CacheFlushReport {
        ok: error.is_none(),
        declined,
        error,
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
    let job = win::Job::create(None, (opts.per_process_cap_gb * GB) as u64, (opts.hard_cap_gb * GB) as u64, true)?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .map_err(|e| format!("could not start {}: {}", command[0], e))?;
    let handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;

    // Assign before the first instruction runs so no child can escape the job.
    let assigned = job.assign_handle(handle);
    if let Some(p) = opts.priority {
        win::set_memory_priority(child.id(), p.value());
    }
    match opts.cpu {
        Some(Cpu::Below) => drop(win::set_cpu_class(child.id(), win::CpuClass::BelowNormal)),
        Some(Cpu::Idle) => drop(win::set_cpu_class(child.id(), win::CpuClass::Idle)),
        Some(Cpu::Eco) => drop(win::set_eco_mode(child.id(), true)),
        _ => {}
    }
    win::resume_handle(handle);
    if !assigned {
        let _ = child.kill();
        return Err("could not place the command in a job object, so the cap would not hold; it was not run".into());
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

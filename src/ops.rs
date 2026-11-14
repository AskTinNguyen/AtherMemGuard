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
            if !candidate || p.created == 0 {
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

/// Free as much RAM as possible for Unreal right now: every group gets its lowest
/// memory priority and is trimmed, whatever the current pressure level.
pub fn prime(cfg: &Config, target: Option<&str>, kill_orphans: bool) -> PrimeReport {
    let before_snap = snapshot();
    let before = SystemStatus::from(&before_snap.sys);
    let groups_gb = |st: &crate::engine::Status| (st.groups.iter().map(|g| g.ram_mb).sum::<u64>() as f64 / 102.4).round() / 10.0;
    let groups_ram_before_gb = groups_gb(&analyze(cfg, &before_snap));

    let mut trimmed = 0usize;
    for g in &cfg.groups {
        let lowest = [&g.critical, &g.elevated, &g.ue]
            .iter()
            .filter_map(|p| p.as_ref().and_then(|p| p.priority))
            .chain(g.priority)
            .min_by_key(|p| p.value())
            .unwrap_or(MemPriority::Low);
        for p in before_snap.procs.iter().filter(|p| any_matches(&g.processes, &p.name) && !is_protected(cfg, p)) {
            win::set_memory_priority(p.pid, lowest.value());
            if win::trim(p.pid) {
                trimmed += 1;
            }
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
            "{} leftover Unreal helper processes have no parent. Run `memcap prime --kill-orphans` to end them.",
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

// ---------------------------------------------------------------- wait

#[derive(Debug, Serialize)]
pub struct WaitResult {
    pub ready: bool,
    pub waited_sec: u64,
    pub ram_free_gb: f64,
    pub commit_free_gb: f64,
    pub need_ram_gb: f64,
    pub need_commit_gb: f64,
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
/// Children are in the job too, and all of them end if memcap itself is killed.
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

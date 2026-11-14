//! Where the UI gets the engine's latest `Status` (the tray loop owns the engine).

use crate::config::{Cpu, MemPriority};
use crate::breakdown::{AppRam, GroupRam, RamBreakdown};
use crate::engine::{Activities, Effective, GroupStatus, Level, Status, SystemStatus, UeProcess, UeStatus};

pub trait StatusSource {
    /// The engine's most recent status, `None` before the first tick.
    fn latest(&self) -> Option<Status>;
}

/// Always returns the same status.
pub struct FakeStatus(pub Option<Status>);

impl Default for FakeStatus {
    fn default() -> FakeStatus {
        FakeStatus(Some(sample_status()))
    }
}

impl StatusSource for FakeStatus {
    fn latest(&self) -> Option<Status> {
        self.0.clone()
    }
}

/// Any closure works as a source.
impl<F: Fn() -> Option<Status>> StatusSource for F {
    fn latest(&self) -> Option<Status> {
        self()
    }
}

/// The numbers drawn in docs/design/mockup.html: editor + build, normal pressure,
/// 45.1 of 127.7 GB RAM free, 27.6 of 144 GB commit free.
pub fn sample_status() -> Status {
    let group = |name: &str, processes: usize, ram_mb: u64, cap_mb: u64, priority: MemPriority, cpu: Cpu| GroupStatus {
        name: name.to_string(),
        processes,
        ram_mb,
        committed_mb: ram_mb + ram_mb / 3,
        policy: Effective { priority, trim_above_mb: cap_mb, cpu, suspend: false },
        hard_cap_mb: 0,
        hard_cap_per_process_mb: 0,
        suspended: false,
        pids: Vec::new(),
    };
    Status {
        time: "2026-11-14 14:02:00".into(),
        daemon: true,
        paused: false,
        mode: "editor+build".into(),
        activities: Activities { editor: true, build: true, cook: false },
        pressure: Level::Normal,
        system: SystemStatus {
            ram_total_gb: 127.7,
            ram_free_gb: 45.1,
            ram_free_pct: 35.3,
            commit_limit_gb: 144.0,
            commit_used_gb: 116.4,
            commit_free_gb: 27.6,
            commit_free_pct: 19.2,
            file_cache_gb: 9.2,
            nonpaged_pool_gb: 1.1,
        },
        ue: UeStatus {
            processes: vec![UeProcess {
                pid: 21640,
                name: "UnrealEditor".into(),
                kind: "editor",
                committed_gb: 43.8,
                ram_gb: 31.2,
                uptime_min: 30,
                created: 0,
            }],
            workers: 3,
            workers_committed_gb: 2.4,
            workers_paused: 0,
            total_committed_gb: 47.9,
            total_ram_gb: 34.0,
        },
        groups: vec![
            group("Browsers", 52, 4198, 3174, MemPriority::Low, Cpu::Normal),
            group("Chat", 34, 1126, 1946, MemPriority::VeryLow, Cpu::Normal),
            group("Agents", 26, 6554, 0, MemPriority::Low, Cpu::Normal),
            group("Background", 19, 819, 614, MemPriority::VeryLow, Cpu::Eco),
        ],
        ram: sample_breakdown(),
        advice: Vec::new(),
        worker_pids: Vec::new(),
    }
}

/// The sample's RAM in use (82.6 GB) split up; the parts add up.
pub fn sample_breakdown() -> RamBreakdown {
    let app = |name: &str, processes: usize, ram_gb: f64| AppRam { name: name.into(), processes, ram_gb };
    let group = |name: &str, ram_gb: f64| GroupRam { name: name.into(), ram_gb };
    RamBreakdown {
        in_use_gb: 82.6,
        unreal_gb: 34.0,
        unreal_editor_gb: 30.1,
        unreal_workers_gb: 2.4,
        groups: vec![group("Browsers", 3.6), group("Chat", 0.9), group("Agents", 5.8), group("Background", 0.6)],
        other_apps_gb: 6.2,
        other_apps_count: 61,
        top_apps: vec![app("rider64", 1, 2.8), app("vmmem", 1, 1.9), app("devenv", 2, 0.7), app("Discord", 6, 0.3), app("Spotify", 5, 0.2)],
        file_cache_gb: 22.4,
        windows_gb: 5.3,
        compressed_gb: 1.6,
        unattributed_gb: 2.2,
    }
}

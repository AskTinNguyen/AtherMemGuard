//! Time the side-effect-free parts of an engine tick on this PC.
//! `cargo run --release --example tick_bench -- [iterations]` (default 50).
//!
//! Only reads: the process list, analysis and name matching. Nothing is trimmed, suspended
//! or re-prioritized.

use std::time::Instant;

use athermemguard::config::{any_matches, Config};
use athermemguard::engine::{self, Snapshot};
use athermemguard::win;

fn median_ms(iters: usize, mut f: impl FnMut()) -> f64 {
    f(); // warm up
    let mut v: Vec<f64> = (0..iters)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn main() {
    let iters: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50).max(1);
    let cfg = Config::builtin();
    let procs = win::list_processes();
    let patterns: usize = cfg.active_groups().map(|g| g.processes.len()).sum::<usize>() + cfg.ue.protect.len() + cfg.ue.workers.len();
    println!("{} processes, {} patterns, {} iterations (median)", procs.len(), patterns, iters);

    let ms = median_ms(iters, || {
        std::hint::black_box(win::list_processes());
    });
    println!("list_processes            {:8.3} ms", ms);
    let ms = median_ms(iters, || {
        std::hint::black_box(win::list_processes_toolhelp());
    });
    println!("list_processes (Toolhelp) {:8.3} ms", ms);

    let snap = Snapshot { sys: win::sys_info(), procs: procs.clone(), now: win::now_filetime() };
    let ms = median_ms(iters, || {
        std::hint::black_box(engine::analyze(&cfg, &snap));
    });
    println!("analyze                   {:8.3} ms", ms);

    // The matching analyze does per tick, on its own: every group, protect and worker
    // pattern against every process name.
    let ms = median_ms(iters, || {
        let mut n = 0usize;
        for p in &procs {
            n += any_matches(&cfg.ue.protect, &p.name) as usize + any_matches(&cfg.ue.workers, &p.name) as usize;
            for g in cfg.active_groups() {
                n += (any_matches(&g.processes, &p.name) && !engine::is_protected(&cfg, p)) as usize;
            }
        }
        std::hint::black_box(n);
    });
    println!("name matching (as before) {:8.3} ms", ms);
}

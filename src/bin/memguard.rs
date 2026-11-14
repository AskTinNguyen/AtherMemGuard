//! `memguard`: the command line for people, scripts and agents.

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use athermemguard::config::{Config, Cpu, MemPriority, Target};
use athermemguard::engine::{analyze, snapshot, Status};
use athermemguard::{install, ops, paths, tray, win};

#[derive(Parser)]
#[command(name = "memguard", version, about = "Keeps this PC primed for agentic Unreal Engine work")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Workload {
    Editor,
    Build,
    Cook,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Workload::Editor => "editor",
            Workload::Build => "build",
            Workload::Cook => "cook",
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum PriorityArg {
    Normal,
    Low,
    Verylow,
}

#[derive(Clone, Copy, ValueEnum)]
enum CpuArg {
    Normal,
    Below,
    Idle,
    Eco,
}

// ---------------------------------------------------------------- config subcommands (0.2)

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the settings in effect (built-in defaults, team.toml and settings.toml merged)
    Show,
    /// Write the settings in effect to a file, to share or keep
    Export { file: std::path::PathBuf },
    /// Replace settings.toml with a file (checked first; the old one is kept as .bak)
    Import { file: std::path::PathBuf },
    /// Install a file as team.toml: defaults for everyone, under your own settings
    Team { file: std::path::PathBuf },
    /// Set one value in settings.toml, e.g. `general.preset aggressive` or
    /// `group.Browsers.trim_above_pct 3`
    Set { path: String, value: String },
    /// Remove your value for a setting, so the team or built-in value applies again
    Reset { path: String },
}

/// A value from the command line: TOML if it parses (`3`, `true`, `["a", "b"]`, `"x"`),
/// otherwise a plain string (`aggressive`).
fn cli_value(s: &str) -> toml_edit::Item {
    match s.trim().parse::<toml_edit::Value>() {
        Ok(v) => toml_edit::Item::Value(v),
        Err(_) => toml_edit::value(s),
    }
}

fn config_cmd(action: ConfigCmd) -> ExitCode {
    use athermemguard::config as c;
    let result = match action {
        ConfigCmd::Show => c::effective_toml().map(|text| print!("{}", text)),
        ConfigCmd::Export { file } => {
            c::export_to(&file).map(|_| println!("Wrote the settings in effect to {}.", file.display()))
        }
        ConfigCmd::Import { file } => c::import_from(&file).map(|_| {
            println!("Imported {}. Your previous settings are in settings.toml.bak.", file.display())
        }),
        ConfigCmd::Team { file } => c::install_team_file(&file).map(|_| {
            println!("Installed {} as {}. Your own settings still win over it.", file.display(), c::team_file().display())
        }),
        ConfigCmd::Set { path, value } => c::set_user_value(&path, cli_value(&value)).map(|_| {
            println!("Set {} = {}. Ather MemGuard picks it up within one interval.", path, value)
        }),
        ConfigCmd::Reset { path } => c::reset_user_value(&path).map(|_| {
            let from = match c::value_layer(&path) {
                c::Layer::Team => "team.toml",
                _ => "the built-in default",
            };
            println!("{} now comes from {}.", path, from)
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e);
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------- history subcommand (0.2)

fn history_cmd(hours: u64, json: bool) -> ExitCode {
    let samples = athermemguard::history::load_since(Duration::from_secs(hours.max(1) * 3600));
    if json {
        print_json(&samples);
        return ExitCode::SUCCESS;
    }
    if samples.is_empty() {
        println!(
            "No memory history for the last {} h. The background app records it every interval in {}.",
            hours.max(1),
            athermemguard::history::history_dir().display()
        );
        return ExitCode::SUCCESS;
    }
    // Keep the table short: at most about 120 rows, always ending with the latest sample.
    let step = samples.len().div_ceil(120).max(1);
    let last = samples.len() - 1;
    let rows: Vec<&athermemguard::history::Sample> =
        samples.iter().enumerate().filter(|(i, _)| (last - i) % step == 0).map(|(_, s)| s).collect();
    let groups: Vec<String> = rows.last().map(|s| s.groups.iter().map(|g| g.name.clone()).collect()).unwrap_or_default();
    let mut header = format!(
        "{:<19}  {:<12} {:<8} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "time", "mode", "pressure", "RAM free", "commit", "UE", "editor", "workers"
    );
    for g in &groups {
        header.push_str(&format!(" {:>10}", g));
    }
    println!("{}", header);
    for s in rows {
        let mut line = format!(
            "{:<19}  {:<12} {:<8} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>7}",
            s.time,
            s.mode,
            s.pressure.label(),
            s.ram_free_gb,
            s.commit_free_gb,
            s.ue_committed_gb,
            s.editor_committed_gb,
            s.workers
        );
        for g in &groups {
            match s.groups.iter().find(|x| &x.name == g) {
                Some(x) => line.push_str(&format!(" {:>10.2}", x.ram_gb)),
                None => line.push_str(&format!(" {:>10}", "-")),
            }
        }
        println!("{}", line);
    }
    println!(
        "{} samples{}; GB. RAM free and commit are free memory; UE, editor and groups are memory in use.",
        samples.len(),
        if step > 1 { format!(", every {}th shown", step) } else { String::new() }
    );
    ExitCode::SUCCESS
}

#[derive(Subcommand)]
enum Cmd {
    /// Show memory, what Unreal is doing, and what each app group is getting
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Free RAM for Unreal now: trim every app group and report the headroom
    Prime {
        /// Check the result against the headroom this workload needs
        #[arg(long = "for", value_enum)]
        workload: Option<Workload>,
        /// End leftover Unreal helper processes whose parent has exited
        #[arg(long)]
        kill_orphans: bool,
        #[arg(long)]
        json: bool,
    },
    /// Wait until there is enough free RAM and commit. Exit code 0 = ready, 2 = timed out
    Wait {
        /// Use the headroom configured for this workload
        #[arg(long = "for", value_enum)]
        workload: Option<Workload>,
        /// Free RAM needed, in GB (overrides --for)
        #[arg(long)]
        ram_gb: Option<f64>,
        /// Free commit needed, in GB (overrides --for)
        #[arg(long)]
        commit_gb: Option<f64>,
        /// Give up after this many seconds
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Prime once before waiting
        #[arg(long)]
        prime: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run a command under a hard memory cap; its child processes are capped with it
    Run {
        /// Hard cap on committed memory for the command and its children, in GB
        #[arg(long, default_value_t = 0.0)]
        cap_gb: f64,
        /// Hard cap per process, in GB
        #[arg(long, default_value_t = 0.0)]
        per_process_cap_gb: f64,
        /// Memory priority for the command
        #[arg(long, value_enum)]
        priority: Option<PriorityArg>,
        /// CPU throttle for the command
        #[arg(long, value_enum)]
        cpu: Option<CpuArg>,
        /// The command and its arguments, after `--`
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Suggest build parallelism and other changes for the memory available now
    Advise {
        #[arg(long)]
        json: bool,
    },
    /// Run the background engine in this console, without the tray icon
    Daemon,
    /// Install for this user, start at sign-in and start now
    Install {
        /// Also put `memguard` on your PATH so agents and scripts can call it
        #[arg(long)]
        add_to_path: bool,
        /// Don't start the tray app after installing
        #[arg(long)]
        no_start: bool,
    },
    /// Stop Ather MemGuard, remove it from sign-in and delete the program (settings are kept)
    Uninstall,
    /// Stop the running background app; apps go back to normal
    Stop,
    /// Write the app icon to a file (used by the build)
    #[command(hide = true)]
    WriteIcon { path: std::path::PathBuf },
    /// Show where the settings file is, check it, or change it
    #[command(args_conflicts_with_subcommands = true)]
    Config {
        /// Validate the settings (built-in, team.toml, settings.toml) and report the first problem
        #[arg(long)]
        check: bool,
        /// Replace settings.toml with the default template (the old file is kept as .bak)
        #[arg(long)]
        reset: bool,
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Show the memory history the background app records
    History {
        /// How far back, in hours
        #[arg(long, default_value_t = 1)]
        hours: u64,
        /// Machine-readable output (every sample)
        #[arg(long)]
        json: bool,
    },

    // ---- Part C: MCP server for agents
    /// Serve MemGuard's tools to a coding agent over stdio (Model Context Protocol)
    Mcp,
    /// Print ready-to-paste setup for Claude Code and Codex
    AgentSetup,
}

fn load_config() -> Config {
    let (cfg, err) = Config::load_or_default();
    if let Some(e) = err {
        eprintln!("warning: settings file has a problem, using built-in defaults: {}", e);
    }
    cfg
}

fn print_json<T: serde::Serialize>(value: &T) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e)));
}

fn current_status(cfg: &Config) -> Status {
    let mut st = analyze(cfg, &snapshot());
    st.daemon = win::instance_running(paths::mutex());
    // Paused / suspended state only the background app knows: take it from its last report.
    if st.daemon {
        if let Ok(text) = std::fs::read_to_string(paths::status_file()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                st.paused = v["paused"].as_bool().unwrap_or(false);
                st.ue.workers_paused = v["ue"]["workers_paused"].as_u64().unwrap_or(0) as usize;
            }
        }
    }
    st
}

fn print_status(st: &Status) {
    println!(
        "Ather MemGuard {}   background app: {}",
        env!("CARGO_PKG_VERSION"),
        if !st.daemon {
            "NOT RUNNING (nothing is being enforced)"
        } else if st.paused {
            "paused"
        } else {
            "running"
        }
    );
    print!("{}", tray::status_text(st));
    println!("\nPolicy now");
    for g in &st.groups {
        let p = &g.policy;
        let mut parts = vec![format!("priority {:?}", p.priority).to_lowercase()];
        if p.trim_above_mb > 0 {
            parts.push(format!("trim above {} MB", p.trim_above_mb));
        }
        if p.cpu != Cpu::Normal {
            parts.push(format!("cpu {:?}", p.cpu).to_lowercase());
        }
        if p.suspend {
            parts.push("suspend".into());
        }
        if g.hard_cap_mb > 0 {
            parts.push(format!("hard cap {} MB", g.hard_cap_mb));
        }
        if g.hard_cap_per_process_mb > 0 {
            parts.push(format!("hard cap {} MB per process", g.hard_cap_per_process_mb));
        }
        println!("  {}: {}", g.name, parts.join(", "));
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Status { json } => {
            let st = current_status(&load_config());
            if json {
                print_json(&st);
            } else {
                print_status(&st);
            }
            ExitCode::SUCCESS
        }

        Cmd::Prime { workload, kill_orphans, json } => {
            let cfg = load_config();
            let report = ops::prime(&cfg, workload.map(Workload::name), kill_orphans);
            if json {
                print_json(&report);
            } else {
                println!(
                    "Trimmed {} processes: app groups went from {:.1} GB to {:.1} GB of RAM.",
                    report.trimmed_processes, report.groups_ram_before_gb, report.groups_ram_after_gb
                );
                println!(
                    "RAM free:    {:.1} GB -> {:.1} GB ({:+.1} GB)",
                    report.before.ram_free_gb, report.after.ram_free_gb, report.ram_freed_gb
                );
                println!("Commit free: {:.1} GB of {:.1} GB", report.after.commit_free_gb, report.after.commit_limit_gb);
                for o in &report.orphans {
                    println!(
                        "Leftover: {} (PID {}), {} MB committed{}",
                        o.name,
                        o.pid,
                        o.committed_mb,
                        if o.killed { ", ended" } else { "" }
                    );
                }
                if let Some(r) = &report.readiness {
                    println!(
                        "{} for {}: needs {:.0} GB RAM and {:.0} GB commit free.",
                        if r.ready { "READY" } else { "NOT READY" },
                        r.target,
                        r.need_ram_gb,
                        r.need_commit_gb
                    );
                }
                for a in &report.advice {
                    println!("- {}", a);
                }
            }
            match report.readiness {
                Some(r) if !r.ready => ExitCode::from(2),
                _ => ExitCode::SUCCESS,
            }
        }

        Cmd::Wait { workload, ram_gb, commit_gb, timeout, prime, json } => {
            let cfg = load_config();
            let base = workload.and_then(|w| cfg.prime.target(w.name())).unwrap_or(Target { ram_gb: 0.0, commit_gb: 0.0 });
            let need = Target { ram_gb: ram_gb.unwrap_or(base.ram_gb), commit_gb: commit_gb.unwrap_or(base.commit_gb) };
            if need.ram_gb <= 0.0 && need.commit_gb <= 0.0 {
                eprintln!("Say what to wait for: --for editor|build|cook, or --ram-gb / --commit-gb.");
                return ExitCode::from(1);
            }
            if prime {
                ops::prime(&cfg, None, false);
            }
            let mut last_print = std::time::Instant::now();
            let result = ops::wait_for(need, Duration::from_secs(timeout), |sys| {
                if !json && last_print.elapsed().as_secs() >= 10 {
                    last_print = std::time::Instant::now();
                    eprintln!(
                        "waiting: RAM free {:.1}/{:.0} GB, commit free {:.1}/{:.0} GB",
                        sys.ram_free_gb, need.ram_gb, sys.commit_free_gb, need.commit_gb
                    );
                }
            });
            if json {
                print_json(&result);
            } else {
                println!(
                    "{} after {} s: RAM free {:.1} GB (need {:.0}), commit free {:.1} GB (need {:.0}).",
                    if result.ready { "READY" } else { "TIMED OUT" },
                    result.waited_sec,
                    result.ram_free_gb,
                    result.need_ram_gb,
                    result.commit_free_gb,
                    result.need_commit_gb
                );
            }
            if result.ready {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }

        Cmd::Run { cap_gb, per_process_cap_gb, priority, cpu, command } => {
            let opts = ops::RunOptions {
                hard_cap_gb: cap_gb,
                per_process_cap_gb,
                priority: priority.map(|p| match p {
                    PriorityArg::Normal => MemPriority::Normal,
                    PriorityArg::Low => MemPriority::Low,
                    PriorityArg::Verylow => MemPriority::VeryLow,
                }),
                cpu: cpu.map(|c| match c {
                    CpuArg::Normal => Cpu::Normal,
                    CpuArg::Below => Cpu::Below,
                    CpuArg::Idle => Cpu::Idle,
                    CpuArg::Eco => Cpu::Eco,
                }),
            };
            match ops::run(&opts, &command) {
                Ok(code) => ExitCode::from((code & 0xff) as u8),
                Err(e) => {
                    eprintln!("memguard run: {}", e);
                    ExitCode::from(125)
                }
            }
        }

        Cmd::Advise { json } => {
            let advice = ops::advise(&load_config());
            if json {
                print_json(&advice);
            } else {
                println!("Mode {}, pressure {}.", advice.mode, advice.pressure.label());
                println!(
                    "Memory Unreal can get now: about {:.0} GB RAM, {:.0} GB commit.",
                    advice.ram_budget_gb, advice.commit_free_gb
                );
                println!(
                    "Suggested compile parallelism: {} of {} logical cores (estimate, 3 GB per action).",
                    advice.suggested_max_parallel_actions, advice.logical_cores
                );
                for n in &advice.notes {
                    println!("- {}", n);
                }
            }
            ExitCode::SUCCESS
        }

        Cmd::WriteIcon { path } => match athermemguard::logo::write_ico(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}: {}", path.display(), e);
                ExitCode::from(1)
            }
        },

        Cmd::Daemon => {
            tray::run_headless();
            ExitCode::SUCCESS
        }

        Cmd::Install { add_to_path, no_start } => match install::install(add_to_path, !no_start) {
            Ok(r) => {
                println!("Installed to {}", r.dir.display());
                if r.migrated_from_memcap {
                    println!("Replaced MemCap, the earlier name of this app. Its settings were carried over.");
                }
                if r.stopped_previous {
                    println!("Stopped the copy that was running.");
                }
                println!("Starts at sign-in: yes");
                if add_to_path {
                    println!(
                        "PATH: {}",
                        if r.path_changed { "added (new terminals will find `memguard`)" } else { "already there" }
                    );
                }
                println!("Running now: {}", if r.started { "yes, look for the Ather icon in the tray" } else { "no" });
                println!("Settings: {}", paths::config_file().display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Install failed: {}", e);
                ExitCode::from(1)
            }
        },

        Cmd::Uninstall => match install::uninstall() {
            Ok(()) => {
                println!("Ather MemGuard is removed. Settings and log are kept in {}", paths::data_dir().display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Uninstall failed: {}", e);
                ExitCode::from(1)
            }
        },

        Cmd::Stop => {
            if install::stop_running() {
                println!("Ather MemGuard stopped. Apps are back to normal.");
            } else if win::instance_running(paths::mutex()) {
                eprintln!("Ather MemGuard did not stop within 8 seconds.");
                return ExitCode::from(1);
            } else {
                println!("Ather MemGuard was not running.");
            }
            ExitCode::SUCCESS
        }

        Cmd::Config { action: Some(action), .. } => config_cmd(action),

        Cmd::History { hours, json } => history_cmd(hours, json),

        Cmd::Config { check, reset, action: None } => {
            let path = paths::config_file();
            if reset {
                if path.exists() {
                    let _ = std::fs::copy(&path, path.with_extension("toml.bak"));
                }
                if let Err(e) = std::fs::create_dir_all(paths::data_dir())
                    .and_then(|_| std::fs::write(&path, athermemguard::config::DEFAULT_TOML))
                {
                    eprintln!("Could not write {}: {}", path.display(), e);
                    return ExitCode::from(1);
                }
                println!("Reset {} to the defaults.", path.display());
                return ExitCode::SUCCESS;
            }
            if check {
                return match athermemguard::config::load_layered() {
                    Ok((cfg, warnings)) => {
                        println!("{} is valid: {} groups.", path.display(), cfg.groups.len());
                        for w in warnings {
                            println!("note: {}", w);
                        }
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("{}", e);
                        ExitCode::from(1)
                    }
                };
            }
            println!("{}", path.display());
            ExitCode::SUCCESS
        }

        // ---- Part C: MCP server for agents
        Cmd::Mcp => match athermemguard::mcp::run_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("memguard mcp: {}", e);
                ExitCode::from(1)
            }
        },

        Cmd::AgentSetup => {
            print_agent_setup();
            ExitCode::SUCCESS
        }
    }
}

// ---- Part C: MCP server for agents

/// The memguard.exe agents should start: the installed copy if there is one, else this one.
fn agent_cli_path() -> std::path::PathBuf {
    let installed = paths::install_dir().join(paths::CLI_EXE);
    if installed.exists() {
        return installed;
    }
    std::env::current_exe().unwrap_or_else(|_| paths::CLI_EXE.into())
}

fn print_agent_setup() {
    let exe = agent_cli_path().display().to_string();
    let json_exe = serde_json::to_string(&exe).unwrap_or_else(|_| format!("\"{}\"", exe));
    println!("Ather MemGuard for AI agents: an MCP server your agent starts by itself (no port, nothing to enable).");
    println!("Tools: memguard_status, memguard_prime, memguard_wait, memguard_advise.");
    println!();
    println!("Claude Code, for all your projects:");
    println!("  claude mcp add --scope user ather-memguard -- \"{}\" mcp", exe);
    println!("If `memguard` is on your PATH, this short form works too:");
    println!("  claude mcp add ather-memguard -- memguard mcp");
    println!();
    println!("Claude Code, shared with a project (.mcp.json in the repo root):");
    println!("  {{ \"mcpServers\": {{ \"ather-memguard\": {{ \"command\": {}, \"args\": [\"mcp\"] }} }} }}", json_exe);
    println!();
    println!(r"Codex (%USERPROFILE%\.codex\config.toml):");
    println!("  [mcp_servers.ather-memguard]");
    println!("  command = '{}'", exe);
    println!("  args = [\"mcp\"]");
    println!("  tool_timeout_sec = 3700   # memguard_wait can block for up to an hour");
    println!();
    println!("Before a local build or cook, have the agent call memguard_wait with prime: true.");
    println!("More: docs/AI-AGENTS.md");
}

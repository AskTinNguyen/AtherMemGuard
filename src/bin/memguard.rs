//! `memguard`: the command line for people, scripts and agents.

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use athermemguard::config::{Config, Cpu, MemPriority, Target};
use athermemguard::engine::{analyze, snapshot, Status};
use athermemguard::{install, ipc, ops, paths, win};

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
enum DiskCmd {
    /// What could go and what removing it would really free (predicted; deletes nothing)
    Checkup {
        /// Only this folder: a project, a lane or a folder of them
        #[arg(long)]
        path: Option<String>,
        /// Count again instead of using the saved result
        #[arg(long)]
        refresh: bool,
        /// How long counting may take, in seconds (default: [disk] scan_budget_sec)
        #[arg(long)]
        budget_sec: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Fresh numbers for exactly these items of the last checkup (all of them if none given)
    Plan {
        /// Item ids from the checkup: d1 d2 ...
        ids: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Clean items of the last checkup: <id>[:now|bin|tool]. Makes a fresh plan, lists what goes, asks first
    Clean {
        /// Item ids from the checkup, each with an optional action: d1 d3:bin d7:tool
        #[arg(long, num_args = 1.., value_name = "ID[:ACTION]", required = true)]
        fix: Vec<String>,
        /// Check everything and show what would happen, without doing it
        #[arg(long)]
        dry_run: bool,
        /// Don't ask (scripts)
        #[arg(long)]
        yes: bool,
        /// Stay and watch the drive until the space shows up (up to 30 minutes), then print it
        #[arg(long)]
        wait: bool,
        /// The checkup the ids come from (memguard disk checkup prints it). Required with --yes
        #[arg(long, value_name = "CHECKUP_ID")]
        checkup: Option<String>,
        /// What each item was when you saw it: <id>=<fingerprint> (the checkup window passes these)
        #[arg(long, value_name = "ID=FINGERPRINT")]
        expect: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Put back what the last cleanup moved to the bin (or one entry by its undo id)
    Undo {
        undo_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show what is in Ather MemGuard's bin; --empty empties it for good
    Bin {
        /// Empty the bin (everything in it, or only on --volume)
        #[arg(long)]
        empty: bool,
        /// Empty only what has been in the bin longer than [disk] hold_hours
        #[arg(long, conflicts_with = "empty")]
        expired: bool,
        /// Only this drive, like D:
        #[arg(long)]
        volume: Option<String>,
        /// Don't ask (scripts)
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Move caches off the system drive by setting user environment variables (prints the plan)
    Relocate {
        /// Put them under this folder instead of the drive Ather MemGuard picks
        #[arg(long)]
        to: Option<String>,
        /// Set the variables (asks first)
        #[arg(long, conflicts_with = "undo")]
        apply: bool,
        /// Put the previous values back
        #[arg(long)]
        undo: bool,
        /// Don't ask (scripts)
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Internal: count a scan job from stdin at background priority
    #[command(hide = true)]
    ScanChild,
    /// Internal: watch a drive's free space after a cleanup and record what came back
    #[command(hide = true)]
    WatchChild { id: String },
}

/// Ask on the console. Without a console, only `--yes` goes ahead.
fn confirm(question: &str, yes: bool) -> Result<bool, String> {
    use std::io::{IsTerminal, Write};
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err("Nothing was changed: add --yes to go ahead without a prompt.".into());
    }
    print!("{} [y/N] ", question);
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn disk_clean_cmd(fix: Vec<String>, dry_run: bool, yes: bool, wait: bool, checkup: Option<String>, expect: Vec<String>, json: bool) -> ExitCode {
    use athermemguard::disk_clean::{self as dc, CleanAction, DiskFiles};
    use athermemguard::disk_scan;
    let fail = |e: String| {
        eprintln!("{}", e);
        ExitCode::from(1)
    };
    let cfg = load_config();
    let Some(cache) = disk_scan::load_cache() else {
        return fail("Run memguard disk checkup first: clean uses its item ids.".into());
    };
    let mut picks: Vec<(String, Option<CleanAction>)> = Vec::new();
    for f in &fix {
        match dc::parse_pick(f) {
            Ok(p) => picks.push(p),
            Err(e) => return fail(e),
        }
    }
    // What each pick will do, so the plan totals only what frees space now.
    let mut together = Vec::new();
    for (id, a) in &picks {
        let it = id[1..].parse::<usize>().ok().and_then(|n| cache.items.get(n.wrapping_sub(1)));
        let Some(it) = it else { return fail(format!("{} is not an item of the last disk checkup.", id)) };
        let tool = disk_scan::catalog_info(&it.key).and_then(|c| c.tool);
        let act = a.or_else(|| dc::offered_actions(&it.key, &it.class, tool.as_deref(), true).first().copied());
        if matches!(act, Some(CleanAction::Now) | Some(CleanAction::Tool)) {
            together.push(id.clone());
        }
    }
    let ids: Vec<String> = picks.iter().map(|(i, _)| i.clone()).collect();
    // What the person (or the checkup window) saw: without it, --yes could act on ids that a
    // newer checkup gave to other folders.
    let mut expects = Vec::new();
    for e in &expect {
        match dc::parse_expect(e) {
            Ok(x) => expects.push(x),
            Err(e) => return fail(e),
        }
    }
    match &checkup {
        Some(cid) => {
            if let Err(e) = dc::check_identity(&cache, cid, &expects, &ids) {
                return fail(e);
            }
        }
        None if !expects.is_empty() => return fail("--expect goes with --checkup <id>. Nothing was changed.".into()),
        None if yes && !dry_run => {
            return fail(format!(
                "Nothing was changed: with --yes, also pass --checkup {} (the checkup these ids come from), so a newer checkup can't change what they mean.",
                cache.ident()
            ))
        }
        None => {}
    }
    let mut p = |files: u64, secs: f64| {
        if !json {
            eprintln!("counting: {} files, {:.0} s", files, secs);
        }
    };
    let (_, rec) = match disk_scan::plan_record(&cfg, &cache, &ids, Some(&together), &disk_scan::run_child, &mut p) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    if checkup.is_some() {
        if let Err(e) = dc::check_live_ids(&cache, &rec) {
            return fail(e);
        }
    }
    let files = DiskFiles::default();
    let _ = files.save_plan(&rec);
    if !json {
        print!("{}", clean_listing(&rec, &picks));
    }
    if !dry_run {
        if json && !yes {
            return fail("Nothing was changed: with --json, add --yes (there is no prompt).".into());
        }
        match confirm("Go ahead?", yes) {
            Ok(true) => {}
            Ok(false) => {
                println!("Nothing was changed.");
                return ExitCode::SUCCESS;
            }
            Err(e) => return fail(e),
        }
    }
    let opts = dc::CleanOpts { dry_run, presample_secs: 5, start_watcher: !wait && !dry_run, ..Default::default() };
    let r = dc::clean(&cfg, &files, &rec, &picks, &opts);
    if json && !wait {
        print_json(&r.to_json());
    } else if !json {
        println!();
        print!("{}", dc::render_clean(&r));
    }
    let mut measured = Vec::new();
    if wait {
        for v in r.volumes.iter().filter(|v| v.watch_id.is_some()) {
            if !json {
                println!("Watching {} for up to 30 minutes...", v.volume_root.trim_end_matches('\\'));
            }
            let m = dc::watch(&files, v.watch_id.as_deref().unwrap_or(""), &mut |line| {
                if !json {
                    println!("  {}", line);
                }
            });
            measured.push(serde_json::json!({ "volume": v.volume_root.trim_end_matches('\\'), "predicted_freed_gb": athermemguard::disk_space::gb2(v.predicted_freed), "measured": m }));
        }
        if json {
            let mut j = r.to_json();
            j["measured_results"] = serde_json::json!(measured);
            print_json(&j);
        }
    }
    let bad = r.outcomes.iter().any(|o| matches!(o.result.as_str(), "refused" | "failed" | "partial"));
    if bad {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// The confirmation's listing: what goes, how, and the predicted frees.
fn clean_listing(rec: &athermemguard::disk_clean::PlanRecord, picks: &[(String, Option<athermemguard::disk_clean::CleanAction>)]) -> String {
    use athermemguard::disk_clean::{self as dc, CleanAction};
    let mut groups: [(CleanAction, &str, Vec<String>); 3] = [
        (CleanAction::Now, "Delete for good (can't be undone):", Vec::new()),
        (CleanAction::Bin, "Move to Ather MemGuard's bin (frees nothing until the bin is emptied; undo with memguard disk undo):", Vec::new()),
        (CleanAction::Tool, "Clean with the tool's own command (can't be undone):", Vec::new()),
    ];
    for (id, a) in picks {
        let Some(it) = rec.item(id) else { continue };
        let Some(act) = a.or_else(|| dc::offered_actions(&it.key, &it.class, it.tool.as_deref(), true).first().copied()) else {
            continue;
        };
        let frees = if act == CleanAction::Bin { "0 GB now".to_string() } else { format!("frees {} predicted", dc::gb_words(it.frees_alone, &it.confidence)) };
        let extra = if act == CleanAction::Tool { format!(" (runs {})", it.tool.as_deref().and_then(dc::tool_command).map(|c| c.display()).unwrap_or_default()) } else { String::new() };
        if let Some(g) = groups.iter_mut().find(|g| g.0 == act) {
            g.2.push(format!("  {:<4}{:<40} {}{}\n      {}", it.id, it.title, frees, extra, it.paths.join("\n      ")));
        }
    }
    let mut s = String::new();
    for (_, head, lines) in &groups {
        if !lines.is_empty() {
            s.push_str(head);
            s.push('\n');
            for l in lines {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    for v in &rec.volumes {
        s.push_str(&format!(
            "Predicted to free {} on {} ({}, counted once). It is measured after the delete.\n",
            dc::gb_words(v.frees_together, &v.confidence),
            v.root.trim_end_matches('\\'),
            v.confidence
        ));
        if v.fs.eq_ignore_ascii_case("ReFS") {
            s.push_str(&format!("  {}\n", athermemguard::disk_scan::REFS_DELAY_NOTE));
        }
    }
    s
}

fn disk_bin_cmd(empty: bool, expired: bool, volume: Option<String>, yes: bool, json: bool) -> ExitCode {
    use athermemguard::disk_clean::{self as dc, DiskFiles};
    let files = DiskFiles::default();
    let cfg = load_config();
    let mut roots = dc::fixed_roots();
    if let Some(v) = &volume {
        let want = v.trim_end_matches('\\').trim_end_matches(':').to_ascii_uppercase();
        roots.retain(|r| r.trim_end_matches('\\').trim_end_matches(':').eq_ignore_ascii_case(&want));
    }
    let runs = dc::bin_runs(&files, &roots);
    let pick = if expired { dc::expired(&runs, cfg.disk().hold_hours, dc::unix_now()) } else { runs.clone() };
    if !empty && !expired {
        if json {
            print_json(&serde_json::json!({ "runs": runs, "hold_hours": cfg.disk().hold_hours }));
        } else if runs.is_empty() {
            println!("Ather MemGuard's bin is empty.");
        } else {
            for r in &runs {
                let left = (r.moved_unix as f64 + cfg.disk().hold_hours * 3600.0 - dc::unix_now() as f64).max(0.0) / 3600.0;
                println!(
                    "{}  {}  holds {:.1} GB of files; frees {} when emptied (predicted). Emptied in about {:.0} h.\n    {}",
                    r.volume_root.trim_end_matches('\\'),
                    r.run_id,
                    athermemguard::disk_space::gb2(r.holds),
                    dc::gb_words(Some(r.frees_alone), "lower_bound"),
                    left,
                    if r.titles.is_empty() { r.path.clone() } else { r.titles.join(", ") }
                );
            }
            println!("Put it back: memguard disk undo    Empty it for good: memguard disk bin --empty");
        }
        return ExitCode::SUCCESS;
    }
    if pick.is_empty() {
        if json {
            print_json(&serde_json::json!({ "emptied": [] }));
        } else {
            println!("Nothing to empty.");
        }
        return ExitCode::SUCCESS;
    }
    if !json {
        for r in &pick {
            println!("Empty for good: {} {} ({:.1} GB of files)", r.volume_root.trim_end_matches('\\'), r.run_id, athermemguard::disk_space::gb2(r.holds));
        }
    }
    match confirm("Empty these for good? They can't be put back afterwards.", yes) {
        Ok(true) => {}
        Ok(false) => {
            println!("Nothing was changed.");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::from(1);
        }
    }
    let (done, watches) = dc::empty_bins(&files, &pick, 3, true);
    if json {
        print_json(&serde_json::json!({
            "emptied": done.iter().map(|(r, d)| serde_json::json!({ "run_id": r.run_id, "volume": r.volume_root, "removed": d })).collect::<Vec<_>>(),
            "predicted": watches.iter().map(|w| serde_json::json!({ "volume": w.volume_root, "predicted_freed_gb": athermemguard::disk_space::gb2(w.predicted), "frees_confidence": w.predicted_confidence, "measured": "later" })).collect::<Vec<_>>(),
        }));
    } else {
        for (r, d) in &done {
            println!("{} {}: {}", r.volume_root.trim_end_matches('\\'), r.run_id, d.words());
        }
        for w in &watches {
            println!("Predicted to free {} on {}; the measured result comes in a notification and the log.", dc::gb_words(Some(w.predicted), "lower_bound"), w.volume_root.trim_end_matches('\\'));
        }
    }
    ExitCode::SUCCESS
}

fn disk_relocate_cmd(to: Option<String>, apply: bool, undo: bool, yes: bool, json: bool) -> ExitCode {
    use athermemguard::disk_clean::DiskFiles;
    use athermemguard::disk_relocate::{self as dr, EnvStore};
    let fail = |e: String| {
        eprintln!("{}", e);
        ExitCode::from(1)
    };
    let store = match dr::RegistryEnv::from_env() {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let files = DiskFiles::default();
    if undo {
        return match dr::undo(&store, &files) {
            Ok(said) => {
                if json {
                    print_json(&serde_json::json!({ "undone": true, "store": store.describe(), "messages": said }));
                } else {
                    for l in said {
                        println!("{}", l);
                    }
                    println!("Restart open terminals and Unreal to pick the old places up again.");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        };
    }
    if let Some(why) = to.as_deref().and_then(dr::to_path_problem) {
        return fail(format!("Nothing was set: {}", why));
    }
    let cfg = load_config();
    let vars = dr::read_vars(&store, &dr::quick_size);
    let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let system = athermemguard::disk_win::volume_of(&format!("{}\\", sys)).map(|(_, g)| g);
    let vols = dr::dest_volumes();
    let to_vol = to.as_deref().and_then(|t| {
        let e = std::path::Path::new(t).ancestors().find(|a| a.exists())?.display().to_string();
        athermemguard::disk_win::volume_of(&e).map(|(_, g)| g)
    });
    let p = dr::plan(&vars, system.as_deref(), &vols, to.as_deref().zip(to_vol.as_deref()), &cfg.disk());
    if to.is_some() && to_vol.is_none() {
        return fail(format!("{} isn't on a drive this PC has.", to.unwrap_or_default()));
    }
    if json && !apply {
        print_json(&dr::plan_json(&p, &store.describe()));
        return ExitCode::SUCCESS;
    }
    if !json {
        print!("{}", dr::render_plan(&p));
        if !store.is_real() {
            println!("(Test store: {})", store.describe());
        }
    }
    if !apply {
        return ExitCode::SUCCESS;
    }
    if let Some(r) = &p.refused {
        return fail(format!("Nothing was set: {}", r));
    }
    match confirm(&format!("Set these user environment variables in {}?", store.describe()), yes) {
        Ok(true) => {}
        Ok(false) => {
            println!("Nothing was changed.");
            return ExitCode::SUCCESS;
        }
        Err(e) => return fail(e),
    }
    match dr::apply(&p, &store, &files) {
        Ok(rec) => {
            if json {
                print_json(&serde_json::json!({ "applied": true, "store": rec.store, "entries": rec.entries }));
            } else {
                println!("Set {} variable{} in {}. Undo with memguard disk relocate --undo.", rec.entries.len(), if rec.entries.len() == 1 { "" } else { "s" }, rec.store);
                println!("Nothing was moved: the old caches stay until you remove them. Once the new places have been used, the disk checkup lists them as \"Old cache, no longer used\".");
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn disk_cmd(json: bool, refresh: bool, action: Option<DiskCmd>) -> ExitCode {
    use athermemguard::{disk, disk_scan};
    let progress = |json: bool| {
        move |files: u64, secs: f64| {
            if !json {
                eprintln!("counting: {} files, {:.0} s", files, secs);
            }
        }
    };
    match action {
        None => {
            let r = disk::report(&load_config(), refresh);
            if json {
                print_json(&r);
            } else {
                print!("{}", disk::render_text(&r));
            }
            ExitCode::SUCCESS
        }
        Some(DiskCmd::ScanChild) => ExitCode::from(disk_scan::child_main() as u8),
        Some(DiskCmd::WatchChild { id }) => {
            athermemguard::disk_clean::watch(&athermemguard::disk_clean::DiskFiles::default(), &id, &mut |_| {});
            ExitCode::SUCCESS
        }
        Some(DiskCmd::Clean { fix, dry_run, yes, wait, checkup, expect, json }) => disk_clean_cmd(fix, dry_run, yes, wait, checkup, expect, json),
        Some(DiskCmd::Bin { empty, expired, volume, yes, json }) => disk_bin_cmd(empty, expired, volume, yes, json),
        Some(DiskCmd::Relocate { to, apply, undo, yes, json }) => disk_relocate_cmd(to, apply, undo, yes, json),
        Some(DiskCmd::Undo { undo_id, json }) => {
            let outs = athermemguard::disk_clean::undo(&athermemguard::disk_clean::DiskFiles::default(), undo_id.as_deref());
            if json {
                print_json(&serde_json::json!({ "outcomes": outs }));
            } else {
                for o in &outs {
                    println!("{} {}: {}", o.id, o.title, if o.message.is_empty() { o.result.clone() } else { o.message.clone() });
                }
            }
            if outs.iter().any(|o| o.result != "undone") {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Some(DiskCmd::Checkup { path, refresh, budget_sec, json }) => {
            let mut p = progress(json);
            match disk_scan::checkup(&load_config(), path.as_deref(), refresh, budget_sec, &disk_scan::run_child, &mut p) {
                Ok(c) => {
                    if json {
                        print_json(&c);
                    } else {
                        print!("{}", disk_scan::render_checkup(&c));
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", e);
                    ExitCode::from(1)
                }
            }
        }
        Some(DiskCmd::Plan { ids, json }) => {
            let mut p = progress(json);
            match disk_scan::plan(&load_config(), &ids, &disk_scan::run_child, &mut p) {
                Ok(v) => {
                    if json {
                        print_json(&v);
                    } else {
                        print!("{}", disk_scan::render_plan(&v));
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", e);
                    ExitCode::from(1)
                }
            }
        }
    }
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
        /// The project folder whose drive needs free space (default: the current folder)
        #[arg(long, value_name = "FOLDER")]
        disk_path: Option<String>,
        /// Free disk space needed there, in GB (overrides --for's disk_gb)
        #[arg(long)]
        disk_gb: Option<f64>,
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
    /// Empty Windows' file cache so its RAM is free again. Asks for administrator permission.
    /// Exit code 0 = done, 1 = failed, 3 = permission declined
    FlushCache {
        #[arg(long)]
        json: bool,
    },
    /// Run the background engine in this console, without the tray icon
    Daemon,
    /// Pause the running background app; other apps return to normal until resumed
    Pause {
        /// Automatically resume after this many minutes
        #[arg(long, value_name = "N")]
        minutes: Option<u64>,
    },
    /// Resume the running background app after a pause
    Resume,
    /// Reload settings.toml and team.toml in the running background app
    Reload,
    /// Install for this user, start at sign-in and start now
    Install {
        /// Also put `memguard` on your PATH so agents and scripts can call it
        #[arg(long)]
        add_to_path: bool,
        /// Don't start the tray app after installing
        #[arg(long)]
        no_start: bool,
        /// Don't start Ather MemGuard when you sign in to Windows
        #[arg(long)]
        no_startup: bool,
    },
    /// Stop Ather MemGuard, remove it from sign-in and delete the program (settings are kept)
    Uninstall,
    /// Stop the running background app; apps go back to normal
    Stop,
    /// Write the app icon to a file (used by the build)
    #[cfg(feature = "gui")]
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

    // ---- Part F: doctor
    /// Check what is using memory Unreal work doesn't need, and clean up if you ask
    Doctor {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Apply the recommended actions, or the ones named as <id>[:action]
        #[arg(long, num_args = 0.., value_name = "ID[:ACTION]")]
        fix: Option<Vec<String>>,
        /// Allow `end`, which ends apps outright (anything unsaved in them is lost)
        #[arg(long)]
        force: bool,
        /// Show what --fix would do, without doing it
        #[arg(long)]
        dry_run: bool,
        /// Undo the last cleanup, or one action by its undo id
        #[arg(long, num_args = 0..=1, value_name = "UNDO_ID")]
        undo: Option<Option<String>>,
    },

    // ---- Disk and Dev Drive (cleanup asks first; see `disk clean`)
    /// Show your drives, how full they are and Dev Drive health; clean, undo, bin and relocate act
    #[command(args_conflicts_with_subcommands = true)]
    Disk {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Read the Defender and Dev Drive facts again instead of today's saved ones
        #[arg(long)]
        refresh: bool,
        #[command(subcommand)]
        action: Option<DiskCmd>,
    },

    // ---- Part G: RAM and BIOS check
    /// Check whether your RAM runs at its rated speed and how to fix it in the BIOS (changes nothing)
    Ram {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Also write the report as a page you can open in a browser
        #[arg(long, value_name = "FILE")]
        html: Option<std::path::PathBuf>,
    },

    /// Which drivers and Windows processes hold the 'Windows and drivers' memory
    Drivers {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
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
    st.disk = athermemguard::disk::status_brief(cfg);
    // Paused / suspended state only the background app knows: take it from its live channel.
    if st.daemon {
        ipc::apply_live_status(&mut st);
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
    print!("{}", ops::status_text(st));
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
    // This process may keep the disk caches (Defender facts, the last disk checkup).
    athermemguard::disk_scan::allow_cache_writes();
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

        Cmd::Wait { workload, ram_gb, commit_gb, timeout, prime, disk_path, disk_gb, json } => {
            let cfg = load_config();
            let base = workload.and_then(|w| cfg.prime.target(w.name())).unwrap_or(Target { ram_gb: 0.0, commit_gb: 0.0, disk_gb: None });
            let need = Target { ram_gb: ram_gb.unwrap_or(base.ram_gb), commit_gb: commit_gb.unwrap_or(base.commit_gb), disk_gb: disk_gb.or(base.disk_gb) };
            if need.ram_gb <= 0.0 && need.commit_gb <= 0.0 && need.disk_gb.unwrap_or(0.0) <= 0.0 {
                eprintln!("Say what to wait for: --for editor|build|cook, or --ram-gb / --commit-gb / --disk-gb.");
                return ExitCode::from(1);
            }
            // Disk doesn't free itself: a shortfall returns at once, before priming.
            let disk = ops::disk_gate(&cfg, disk_path.as_deref(), need.disk_gb);
            if let Some(checks) = disk.as_ref().filter(|c| c.iter().any(|c| !c.ok)) {
                let result = ops::WaitResult::blocked_by_disk(need, checks.clone());
                if json {
                    print_json(&result);
                } else {
                    for c in checks.iter().filter(|c| !c.ok) {
                        println!(
                            "NOT READY: {} has {:.1} GB free and needs {:.0} GB ({}). Disk space doesn't free itself: run `memguard disk checkup` to see what can go.",
                            c.volume,
                            c.free_gb,
                            c.need_gb,
                            c.roles.join(", ")
                        );
                    }
                }
                return ExitCode::from(2);
            }
            if prime {
                ops::prime(&cfg, None, false);
            }
            let mut last_print = std::time::Instant::now();
            let mut result = ops::wait_for(need, Duration::from_secs(timeout), |sys| {
                if !json && last_print.elapsed().as_secs() >= 10 {
                    last_print = std::time::Instant::now();
                    eprintln!(
                        "waiting: RAM free {:.1}/{:.0} GB, commit free {:.1}/{:.0} GB",
                        sys.ram_free_gb, need.ram_gb, sys.commit_free_gb, need.commit_gb
                    );
                }
            });
            if let Some(checks) = disk {
                result.set_disk(checks);
            }
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

        Cmd::FlushCache { json } => {
            // Not elevated: start this same command elevated (hidden), which then does it here.
            let exe = std::env::current_exe().unwrap_or_else(|_| "memguard.exe".into());
            let r = ops::flush_cache(&exe, "flush-cache");
            if win::is_elevated() && !r.ok {
                // The elevated copy: its details go to the log for the copy that started it.
                athermemguard::log::write(&format!("flush-cache: {}", r.error.as_deref().unwrap_or("failed")));
            }
            if json {
                print_json(&r);
            } else if r.ok {
                println!(
                    "Emptied Windows' file cache: {:.1} GB -> {:.1} GB. RAM free {:.1} GB -> {:.1} GB.",
                    r.file_cache_before_gb, r.file_cache_after_gb, r.ram_free_before_gb, r.ram_free_after_gb
                );
            } else {
                eprintln!("Windows' file cache wasn't emptied: {}.", r.error.as_deref().unwrap_or("unknown error"));
            }
            ExitCode::from(if r.ok { 0 } else if r.declined { 3 } else { 1 })
        }

        #[cfg(feature = "gui")]
        Cmd::WriteIcon { path } => match athermemguard::logo::write_ico(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}: {}", path.display(), e);
                ExitCode::from(1)
            }
        },

        Cmd::Daemon => {
            ops::run_headless();
            ExitCode::SUCCESS
        }

        Cmd::Pause { minutes } => match ipc::send_command(ipc::ControlCmd::Pause { minutes }) {
            Ok(()) => {
                match minutes {
                    Some(m) => println!("Ather MemGuard paused for {} min.", m),
                    None => println!("Ather MemGuard paused."),
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        },

        Cmd::Resume => match ipc::send_command(ipc::ControlCmd::Resume) {
            Ok(()) => {
                println!("Ather MemGuard resumed.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        },

        Cmd::Reload => match ipc::send_command(ipc::ControlCmd::Reload) {
            Ok(()) => {
                println!("Reloaded settings.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        },

        Cmd::Install { add_to_path, no_start, no_startup } => match install::install(install::InstallOptions {
            add_to_path,
            start_with_windows: !no_startup,
            start: !no_start,
        }) {
            Ok(r) => {
                println!("Installed to {}", r.dir.display());
                if r.migrated_from_memcap {
                    println!("Replaced MemCap, the earlier name of this app. Its settings were carried over.");
                }
                if r.stopped_previous {
                    println!("Stopped the copy that was running.");
                }
                println!("Starts at sign-in: {}", if no_startup { "no" } else { "yes" });
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

        // ---- Part F: doctor
        Cmd::Doctor { json, fix, force, dry_run, undo } => run_doctor(json, fix, force, dry_run, undo),

        // ---- Disk and Dev Drive
        Cmd::Disk { json, refresh, action } => disk_cmd(json, refresh, action),

        // ---- Part G: RAM and BIOS check
        Cmd::Ram { json, html } => run_ram(json, html),

        Cmd::Drivers { json } => {
            let report = athermemguard::drivers::report();
            if json {
                print_json(&report);
            } else {
                print!("{}", athermemguard::drivers::render_text(&report));
            }
            ExitCode::SUCCESS
        }
    }
}

// ---- Part C: MCP server for agents

/// The memguard.exe agents should start: the installed copy if there is one, else this one.
fn agent_cli_path() -> std::path::PathBuf {
    athermemguard::mcp::agent_cli_path(std::env::current_exe().unwrap_or_else(|_| paths::CLI_EXE.into()))
}

/// The same text the tray's "Copy setup" puts on the clipboard.
fn print_agent_setup() {
    print!("{}", athermemguard::mcp::agent_setup_text(&agent_cli_path()));
}

// ---- Part F: doctor

fn run_doctor(json: bool, fix: Option<Vec<String>>, force: bool, dry_run: bool, undo: Option<Option<String>>) -> ExitCode {
    use athermemguard::doctor::{self, OutcomeKind};

    if let Some(id) = undo {
        let outcomes = doctor::undo(id.is_none(), id.as_deref());
        if json {
            print_json(&serde_json::json!({ "outcomes": outcomes }));
        } else {
            for o in &outcomes {
                println!("{}", outcome_line(o));
            }
        }
        let bad = outcomes.iter().any(|o| matches!(o.result, OutcomeKind::Failed | OutcomeKind::Refused));
        return if bad { ExitCode::from(1) } else { ExitCode::SUCCESS };
    }

    if fix.is_none() && (dry_run || force) {
        eprintln!("--dry-run and --force go with --fix.");
        return ExitCode::from(1);
    }
    let report = doctor::scan(&load_config());
    let Some(fix) = fix else {
        if json {
            print_json(&report);
        } else {
            print_doctor_report(&report);
        }
        return ExitCode::SUCCESS;
    };

    let picks = if fix.is_empty() {
        report.recommended_picks()
    } else {
        let mut picks = Vec::new();
        for s in &fix {
            match doctor::parse_pick(&report, s) {
                Ok(p) => picks.push(p),
                Err(e) => {
                    eprintln!("{}", e);
                    return ExitCode::from(1);
                }
            }
        }
        picks
    };
    if picks.is_empty() {
        if json {
            print_json(&serde_json::json!({ "dry_run": dry_run, "outcomes": [] }));
        } else {
            println!("Nothing is recommended right now. Run `memguard doctor` to see everything, then pick with --fix <id>.");
        }
        return ExitCode::SUCCESS;
    }
    if !json {
        println!("{}", if dry_run { "Would do:" } else { "Doing:" });
        for (id, action) in &picks {
            let title = report.finding(id).map_or(id.as_str(), |f| f.title.as_str());
            println!("  {}: {} ({})", title, action.name(), id);
        }
        if !dry_run && picks.iter().any(|(_, a)| *a == doctor::ActionKind::Close) {
            println!("Asking apps to close; this can take up to 10 seconds...");
        }
    }
    let outcomes = if dry_run { doctor::dry_run(&report, &picks, force) } else { doctor::apply(&report, &picks, force) };
    if json {
        print_json(&serde_json::json!({ "dry_run": dry_run, "outcomes": outcomes }));
    } else {
        println!();
        for o in &outcomes {
            let title = report.finding(&o.finding_id).map_or(o.finding_id.as_str(), |f| f.title.as_str());
            println!("{}  {}", title, outcome_line(o));
        }
        if outcomes.iter().any(|o| o.undo_id.is_some()) {
            println!("\nUndo with `memguard doctor --undo`.");
        }
        if outcomes.iter().any(|o| o.result == OutcomeKind::StillOpen) && !force {
            println!("An app that stayed open can be ended with `memguard doctor --fix <id>:end --force` (anything unsaved in it is lost).");
        }
    }
    let bad = outcomes.iter().any(|o| matches!(o.result, OutcomeKind::Failed | OutcomeKind::Refused));
    if bad {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn outcome_line(o: &athermemguard::doctor::Outcome) -> String {
    use athermemguard::doctor::OutcomeKind;
    let tag = match o.result {
        OutcomeKind::Done => "done",
        OutcomeKind::StillOpen => "still open",
        OutcomeKind::Refused => "not done",
        OutcomeKind::Failed => "failed",
        OutcomeKind::WouldDo => "dry run",
        OutcomeKind::Undone => "undone",
    };
    match &o.undo_id {
        Some(id) => format!("[{}] {} ({}; undo id {})", tag, o.message, o.finding_id, id),
        None if o.finding_id.is_empty() => format!("[{}] {}", tag, o.message),
        None => format!("[{}] {} ({})", tag, o.message, o.finding_id),
    }
}

type SectionPick = fn(&athermemguard::doctor::Finding) -> bool;

fn print_doctor_report(r: &athermemguard::doctor::Report) {
    use athermemguard::doctor::{self, mem_label, Finding, FindingKind, Verdict};
    let s = &r.system;
    println!("Ather MemGuard checkup   {}", r.time);
    println!(
        "RAM {:.1} GB free of {:.1} GB, commit {:.1} GB free of {:.1} GB, Unreal {}",
        s.ram_free_gb,
        s.ram_total_gb,
        s.commit_free_gb,
        s.commit_limit_gb,
        if r.unreal_active { "running" } else { "not running" }
    );
    let sections: [(&str, SectionPick); 7] = [
        ("NOT NEEDED FOR UNREAL", |f| f.kind == FindingKind::App && f.verdict == Verdict::Close),
        ("STARTS WITH WINDOWS", |f| f.kind == FindingKind::Startup),
        ("BIG TOOLS YOU MIGHT NOT BE USING", |f| f.kind == FindingKind::App && f.verdict == Verdict::HeavyTool),
        ("WORTH A LOOK", |f| f.kind == FindingKind::App && f.verdict == Verdict::Review),
        ("HELD BACK, NOT CLOSED (wanted, but should give way to Unreal)", |f| {
            f.kind == FindingKind::App && f.verdict == Verdict::HoldBack
        }),
        ("UNREAL", |f| f.kind == FindingKind::Unreal),
        ("THIS PC", |f| f.kind == FindingKind::System),
    ];
    let mut ram_hint_shown = false;
    for (title, pick) in sections {
        let items: Vec<&Finding> = r.findings.iter().filter(|f| pick(f)).collect();
        if items.is_empty() {
            continue;
        }
        let total: u64 = items.iter().map(|f| f.ram_mb).sum();
        println!();
        if total > 0 && !title.starts_with("STARTS") {
            println!("{}   {} RAM", title, mem_label(total));
        } else {
            println!("{}", title);
        }
        for f in items {
            let mut head = format!("  {}", f.title);
            if f.processes > 0 && f.kind != FindingKind::System {
                head = format!(
                    "{:<36} {:>8} RAM  {:>8} committed  {} process{}",
                    head,
                    mem_label(f.ram_mb),
                    mem_label(f.committed_mb),
                    f.processes,
                    if f.processes == 1 { "" } else { "es" }
                );
            }
            println!("{}", head.trim_end());
            println!("      {}", f.detail);
            if doctor::is_ram_finding(&f.id) && !ram_hint_shown {
                ram_hint_shown = true;
                println!("      Run `memguard ram` for the steps: what to change in the BIOS, and how to check it worked.");
            }
            if !f.actions.is_empty() {
                let acts: Vec<String> = f
                    .actions
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let mut s = a.kind.name().to_string();
                        if i == 0 && f.recommended {
                            s.push_str(" (recommended)");
                        }
                        if a.needs_force {
                            s.push_str(" (needs --force)");
                        }
                        s
                    })
                    .collect();
                println!("      {}: {}", f.id, acts.join(", "));
            }
        }
    }
    println!();
    if r.reclaimable_ram_gb > 0.0 || r.reclaimable_commit_gb > 0.0 {
        println!(
            "Could be freed: about {:.1} GB of RAM ({:.1} GB committed) by closing what Unreal doesn't need.",
            r.reclaimable_ram_gb, r.reclaimable_commit_gb
        );
    } else {
        println!("All clear: nothing Unreal doesn't need is holding much memory.");
    }
    let recommended = r.recommended_picks().len();
    let actionable = r.findings.iter().find(|f| !f.actions.is_empty());
    match (recommended, actionable) {
        (n, _) if n > 0 => println!(
            "To act: `memguard doctor --fix` does the {} recommended action{}; `memguard doctor --fix <id>[:action]` picks your own. Nothing changes until you run one.",
            n,
            if n == 1 { "" } else { "s" }
        ),
        (_, Some(f)) => println!(
            "To act: `memguard doctor --fix <id>[:action]`, for example `memguard doctor --fix {}`. Nothing changes until you run one.",
            f.id
        ),
        _ => println!("Nothing to do."),
    }
}

// ---- Part G: RAM and BIOS check

/// `memguard ram`: a report, not a test, so it always exits 0. A page that can't be
/// written is said on stderr.
fn run_ram(json: bool, html: Option<std::path::PathBuf>) -> ExitCode {
    use athermemguard::ramcheck;
    let report = ramcheck::check();
    if json {
        print_json(&report);
    } else {
        print!("{}", ramcheck::render_text(&report));
    }
    if let Some(path) = html {
        match std::fs::write(&path, ramcheck::render_html(&report)) {
            Ok(()) if !json => println!("\nReport page written to {}", path.display()),
            Ok(()) => {}
            Err(e) => eprintln!("Couldn't write {}: {}", path.display(), e),
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_flags() {
        let flags = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Cmd::Install { add_to_path, no_start, no_startup } => (add_to_path, no_start, no_startup),
            _ => panic!("not install"),
        };
        assert_eq!(flags(&["memguard", "install"]), (false, false, false));
        assert_eq!(flags(&["memguard", "install", "--no-startup"]), (false, false, true));
        assert_eq!(flags(&["memguard", "install", "--add-to-path", "--no-start", "--no-startup"]), (true, true, true));
    }

    #[test]
    fn ipc_subcommands_parse() {
        assert!(matches!(Cli::try_parse_from(["memguard", "pause"]).unwrap().command, Cmd::Pause { minutes: None }));
        assert!(matches!(Cli::try_parse_from(["memguard", "pause", "--minutes", "5"]).unwrap().command, Cmd::Pause { minutes: Some(5) }));
        assert!(matches!(Cli::try_parse_from(["memguard", "resume"]).unwrap().command, Cmd::Resume));
        assert!(matches!(Cli::try_parse_from(["memguard", "reload"]).unwrap().command, Cmd::Reload));
        assert!(Cli::try_parse_from(["memguard", "pause", "--minutes", "-5"]).is_err());
        assert!(Cli::try_parse_from(["memguard", "pause", "--minutes", "abc"]).is_err());
    }
}

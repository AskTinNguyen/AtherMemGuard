//! MCP server over stdio, so coding agents can call MemGuard as tools.
//!
//! JSON-RPC 2.0, one JSON message per line, protocol version 2025-06-18. The agent starts
//! `memguard mcp` itself, so there is no port, no token and nothing to enable.
//!
//! Only protocol messages go to the writer (stdout). Diagnostics go to stderr.

use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::{Config, Target};
use crate::engine::{analyze, snapshot, Status};
use crate::history::Sample;
use crate::{ops, paths, win};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Older versions we also accept: the tools work the same, `structuredContent` is simply ignored.
const OLDER_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];
pub const SERVER_NAME: &str = "ather-memguard";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

const WAIT_DEFAULT_SEC: u64 = 600;
const WAIT_MAX_SEC: u64 = 3600;
const HISTORY_DEFAULT_HOURS: f64 = 1.0;
const HISTORY_MAX_HOURS: f64 = 168.0;
/// Longer spans are thinned to about this many samples (evenly, always keeping the newest),
/// so a week of history doesn't flood the agent's context.
const HISTORY_MAX_SAMPLES: usize = 720;
const DISK_BUDGET_DEFAULT_SEC: u64 = 60;
const DISK_BUDGET_MAX_SEC: u64 = 300;

const INSTRUCTIONS: &str = "Ather MemGuard keeps this Windows PC's memory free for Unreal Engine work. \
Before starting a local Unreal build, cook or packaging run, call memguard_wait with prime: true and the \
workload (build or cook); start the job only when it returns ready: true. Call memguard_advise to pick \
-MaxParallelActions for UnrealBuildTool. Call memguard_status when memory looks tight or a build failed \
with out-of-memory errors. When the person asks what is using memory or how to free more, call memguard_doctor \
and relay its findings; it changes nothing, and you should ask the person before running `memguard doctor --fix`. \
When the person asks whether their RAM runs at full speed, call memguard_ram; only the person can change BIOS settings. \
Before a cook, pass disk_path (the project folder) to memguard_wait; if blocked_by contains disk, don't start: call \
memguard_disk_checkup and relay it in two lines. Every frees number is predicted, not measured: quote only \
totals[].frees_together_gb or one item's frees_alone_gb, with its confidence word (exact as is, lower_bound as at least, \
estimate as about, upper_bound as at most) and the word predicted; never add numbers yourself, never present logical_gb as \
space that can be freed, and never promise the space right after a delete (on ReFS it can take a long time to show up). \
To check that a set frees enough, call memguard_disk_plan and use free_after_gb, only when exact or lower_bound. \
memguard_disk_clean is the only tool that deletes: prefer it only for regenerable build output and caches (Intermediate, \
DerivedDataCache, crash and shader debug files, tool caches), only on ids from a memguard_disk_plan you made in this session \
less than 10 minutes before, always passing its plan_id (required), and always tell the person what you deleted (titles and paths) and the predicted frees. Its measured \
result comes later, in memguard_disk recent_cleanups: report that as measured, never earlier. Moves to the bin, tool \
cleans, logs, cooked output and the shared caches are the person's call (memguard disk clean --fix); lanes and worktrees \
are never deleted by Ather MemGuard. Mention dev:* findings from memguard_disk once and never claim to have fixed them.";

/// Settings source. The real server reads settings.toml on every call (so edits apply
/// at once); tests pass the built-in defaults.
pub type ConfigLoader = Box<dyn Fn() -> Config>;

/// Memory history source: samples from the last `Duration`. The real server reads the CSV
/// files in the data folder, like `memguard history --json`; tests pass fixed samples.
pub type HistoryLoader = Box<dyn Fn(Duration) -> Vec<Sample>>;

/// How disk counting runs: the low-priority child process, or in-process in tests.
pub type ScanRunner = Box<dyn Fn(&crate::disk_scan::ScanJob, crate::disk_scan::Progress) -> Result<crate::disk_scan::ScanOutput, String>>;

/// The latest disk checkup, for `memguard_disk_plan`.
pub type CheckupLoader = Box<dyn Fn() -> Option<crate::disk_scan::CachedCheckup>>;

pub struct Server {
    load_config: ConfigLoader,
    load_history: HistoryLoader,
    run_scan: ScanRunner,
    load_checkup: CheckupLoader,
    /// Where plans, the undo log and the measured watches are kept.
    disk_files: crate::disk_clean::DiskFiles,
    /// How `memguard_disk_clean` acts (tests: no waiting, no watcher, fixed Unreal facts).
    clean_opts: crate::disk_clean::CleanOpts,
    /// The plans `memguard_disk_plan` made in this server, newest last. `memguard_disk_clean`
    /// acts only on these: a plan file on disk is something an agent could edit.
    agent_plans: std::cell::RefCell<Vec<crate::disk_clean::PlanRecord>>,
}

impl Default for Server {
    fn default() -> Self {
        Server::new(Box::new(|| {
            let (cfg, err) = Config::load_or_default();
            if let Some(e) = err {
                eprintln!("memguard mcp: settings file has a problem, using built-in defaults: {}", e);
            }
            cfg
        }))
    }
}

/// A tool failure: reported to the agent as a result with `isError: true`.
struct ToolError(String);

impl Server {
    pub fn new(load_config: ConfigLoader) -> Self {
        Server {
            load_config,
            load_history: Box::new(crate::history::load_since),
            run_scan: Box::new(|job, progress| crate::disk_scan::run_child(job, progress)),
            load_checkup: Box::new(crate::disk_scan::load_cache),
            disk_files: crate::disk_clean::DiskFiles::default(),
            clean_opts: crate::disk_clean::CleanOpts { agent: true, presample_secs: 5, start_watcher: true, ..Default::default() },
            agent_plans: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Keep plans, the undo log and watches in `files`, and clean with `opts` (tests).
    pub fn with_clean(mut self, files: crate::disk_clean::DiskFiles, opts: crate::disk_clean::CleanOpts) -> Self {
        self.disk_files = files;
        self.clean_opts = crate::disk_clean::CleanOpts { agent: true, ..opts };
        self
    }

    /// Use another disk counter and checkup store (tests count in-process).
    pub fn with_disk(mut self, run_scan: ScanRunner, load_checkup: CheckupLoader) -> Self {
        self.run_scan = run_scan;
        self.load_checkup = load_checkup;
        self
    }

    /// Use another history source (tests).
    pub fn with_history(mut self, load_history: HistoryLoader) -> Self {
        self.load_history = load_history;
        self
    }

    /// Read messages until the reader ends. Each reply is one line on the writer.
    pub fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_line(&line, &mut writer)? {
                write_message(&mut writer, &reply)?;
            }
        }
        Ok(())
    }

    /// Handle one incoming line. Returns the reply, or `None` for notifications and for
    /// responses the client sends us. Progress notifications go straight to `out`.
    pub fn handle_line<W: Write>(&self, line: &str, out: &mut W) -> io::Result<Option<Value>> {
        // Some shells put a byte-order mark before the first line they pipe.
        let line = line.trim_start_matches('\u{feff}');
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Ok(Some(error_reply(Value::Null, PARSE_ERROR, &format!("Parse error: {}", e)))),
        };
        let Some(obj) = msg.as_object() else {
            // Batches were removed from MCP in 2025-06-18.
            return Ok(Some(error_reply(Value::Null, INVALID_REQUEST, "Invalid request: expected one JSON object")));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) {
                return Ok(None); // a response to something we sent; we send no requests
            }
            return Ok(Some(error_reply(id.unwrap_or(Value::Null), INVALID_REQUEST, "Invalid request: no method")));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);

        let Some(id) = id else {
            // Notification: never answered. `notifications/initialized` and
            // `notifications/cancelled` need no action.
            return Ok(None);
        };

        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_list() })),
            "tools/call" => self.call(&params, out),
            _ => Err((METHOD_NOT_FOUND, format!("Method not found: {}", method))),
        };
        Ok(Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => error_reply(id, code, &message),
        }))
    }

    fn call<W: Write>(&self, params: &Value, out: &mut W) -> Result<Value, (i64, String)> {
        let bad = |m: &str| (INVALID_PARAMS, m.to_string());
        let params = params.as_object().ok_or_else(|| bad("tools/call needs params with a tool name"))?;
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| bad("tools/call needs a tool name"))?;
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(m)) => m,
            Some(_) => return Err(bad("arguments must be an object")),
        };
        if !tool_list().iter().any(|t| t["name"] == name) {
            return Err((INVALID_PARAMS, format!("Unknown tool: {}", name)));
        }
        let progress_token = params.get("_meta").and_then(|m| m.get("progressToken")).cloned();

        let outcome = catch_unwind(AssertUnwindSafe(|| self.run_tool(name, args, progress_token, out)))
            .unwrap_or_else(|_| Err(ToolError(format!("{} failed unexpectedly; see stderr for details", name))));
        Ok(match outcome {
            Ok(value) => json!({
                "content": [{ "type": "text", "text": pretty(&value) }],
                "structuredContent": value,
                "isError": false,
            }),
            Err(ToolError(message)) => json!({
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            }),
        })
    }

    fn run_tool<W: Write>(
        &self,
        name: &str,
        args: &Map<String, Value>,
        progress_token: Option<Value>,
        out: &mut W,
    ) -> Result<Value, ToolError> {
        match name {
            "memguard_status" => {
                check_keys(args, &[])?;
                to_value(&current_status(&(self.load_config)()))
            }
            "memguard_prime" => {
                check_keys(args, &["workload", "kill_orphans"])?;
                let workload = workload_arg(args)?;
                let kill_orphans = bool_arg(args, "kill_orphans")?.unwrap_or(false);
                to_value(&ops::prime(&(self.load_config)(), workload, kill_orphans))
            }
            "memguard_wait" => {
                check_keys(args, &["workload", "ram_gb", "commit_gb", "timeout_sec", "prime", "disk_gb", "disk_path"])?;
                let workload = workload_arg(args)?;
                let ram_gb = gb_arg(args, "ram_gb")?;
                let commit_gb = gb_arg(args, "commit_gb")?;
                let disk_gb = gb_arg(args, "disk_gb")?;
                let disk_path = str_arg(args, "disk_path")?;
                let timeout = timeout_arg(args)?;
                let prime = bool_arg(args, "prime")?.unwrap_or(false);
                if workload.is_none() && ram_gb.is_none() && commit_gb.is_none() && disk_gb.is_none() {
                    return Err(ToolError(
                        "Say what to wait for: workload (editor, build or cook), or ram_gb / commit_gb / disk_gb.".into(),
                    ));
                }
                if let Some(p) = &disk_path {
                    if !std::path::Path::new(p).exists() {
                        return Err(ToolError(format!("disk_path {} doesn't exist on this PC.", p)));
                    }
                }
                let cfg = (self.load_config)();
                let base = workload.and_then(|w| cfg.prime.target(w)).unwrap_or(Target { ram_gb: 0.0, commit_gb: 0.0, disk_gb: None });
                let need = Target { ram_gb: ram_gb.unwrap_or(base.ram_gb), commit_gb: commit_gb.unwrap_or(base.commit_gb), disk_gb: disk_gb.or(base.disk_gb) };
                if need.ram_gb <= 0.0 && need.commit_gb <= 0.0 && need.disk_gb.unwrap_or(0.0) <= 0.0 {
                    return Err(ToolError("Nothing to wait for: the RAM, commit and disk needed are all 0.".into()));
                }
                // Disk doesn't free itself: a shortfall returns at once, before priming.
                let disk = ops::disk_gate(&cfg, disk_path.as_deref(), need.disk_gb);
                if let Some(checks) = disk.as_ref().filter(|c| c.iter().any(|c| !c.ok)) {
                    return to_value(&ops::WaitResult::blocked_by_disk(need, checks.clone()));
                }
                if prime {
                    ops::prime(&cfg, None, false);
                }
                let total = timeout.as_secs_f64();
                let started = std::time::Instant::now();
                let mut result = ops::wait_for(need, timeout, |sys| {
                    if let Some(token) = &progress_token {
                        let note = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": started.elapsed().as_secs_f64().min(total),
                                "total": total,
                                "message": format!(
                                    "Waiting: RAM free {:.1} of {:.1} GB, commit free {:.1} of {:.1} GB",
                                    sys.ram_free_gb, need.ram_gb, sys.commit_free_gb, need.commit_gb
                                ),
                            }
                        });
                        if let Err(e) = write_message(out, &note) {
                            eprintln!("memguard mcp: could not send progress: {}", e);
                        }
                    }
                });
                if let Some(checks) = disk {
                    result.set_disk(checks);
                }
                to_value(&result)
            }
            // ---- Disk and Dev Drive (report only; nothing here deletes or moves anything)
            "memguard_disk" => {
                check_keys(args, &["refresh"])?;
                let refresh = bool_arg(args, "refresh")?.unwrap_or(false);
                to_value(&crate::disk::report(&(self.load_config)(), refresh))
            }
            "memguard_disk_checkup" => {
                check_keys(args, &["path", "refresh", "budget_sec"])?;
                let path = str_arg(args, "path")?;
                let refresh = bool_arg(args, "refresh")?.unwrap_or(false);
                let budget = match args.get("budget_sec") {
                    None | Some(Value::Null) => DISK_BUDGET_DEFAULT_SEC,
                    Some(v) => match v.as_f64() {
                        Some(n) if n.is_finite() && n >= 1.0 => (n.round() as u64).min(DISK_BUDGET_MAX_SEC),
                        _ => return Err(ToolError("budget_sec must be a number of seconds, 1 or more (at most 300).".into())),
                    },
                };
                let cfg = (self.load_config)();
                let mut note = progress_writer(&progress_token, out, budget as f64, "Counting");
                crate::disk_scan::checkup(&cfg, path.as_deref(), refresh, Some(budget), &*self.run_scan, &mut note).map_err(ToolError).and_then(|c| to_value(&c))
            }
            "memguard_disk_plan" => {
                check_keys(args, &["ids"])?;
                let ids: Vec<String> = match args.get("ids") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(a)) if a.iter().all(Value::is_string) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                    Some(_) => return Err(ToolError("ids must be a list of item ids from memguard_disk_checkup, like [\"d1\", \"d7\"].".into())),
                };
                let cfg = (self.load_config)();
                let mut note = progress_writer(&progress_token, out, 300.0, "Counting");
                let cache = (self.load_checkup)().ok_or_else(|| {
                    ToolError("Call memguard_disk_checkup first: a plan uses the item ids of the latest checkup.".into())
                })?;
                let (v, rec) = crate::disk_scan::plan_record(&cfg, &cache, &ids, None, &*self.run_scan, &mut note).map_err(ToolError)?;
                if let Err(e) = self.disk_files.save_plan(&rec) {
                    eprintln!("memguard mcp: could not save the plan: {}", e);
                }
                let mut mine = self.agent_plans.borrow_mut();
                mine.push(rec);
                if mine.len() > 10 {
                    mine.remove(0);
                }
                Ok(v)
            }
            // ---- The one tool that deletes: regenerable items of a fresh plan, rules re-checked now.
            "memguard_disk_clean" => {
                check_keys(args, &["ids", "plan_id"])?;
                let ids: Vec<String> = match args.get("ids") {
                    Some(Value::Array(a)) if !a.is_empty() && a.iter().all(Value::is_string) => a.iter().filter_map(|v| v.as_str().map(|s| s.trim().to_string())).collect(),
                    _ => return Err(ToolError("ids must be a non-empty list of item ids from your memguard_disk_plan, like [\"d1\", \"d2\"].".into())),
                };
                let Some(plan_id) = str_arg(args, "plan_id")?.filter(|p| !p.trim().is_empty()) else {
                    return Err(ToolError("plan_id is required: call memguard_disk_plan with exactly the ids you want to clean and pass the plan_id it returns.".into()));
                };
                // Only a plan this server made (in memory); never one read back from disk.
                let plans = self.agent_plans.borrow().clone();
                let plan = crate::disk_clean::plan_for_agent(&plans, plan_id.trim(), &ids, crate::disk_clean::unix_now()).map_err(ToolError)?;
                let picks: Vec<(String, Option<crate::disk_clean::CleanAction>)> = ids.iter().map(|i| (i.clone(), Some(crate::disk_clean::CleanAction::Now))).collect();
                let r = crate::disk_clean::clean(&(self.load_config)(), &self.disk_files, plan, &picks, &self.clean_opts);
                let mut v = r.to_json();
                let deleted: Vec<String> = r.outcomes.iter().filter(|o| matches!(o.result.as_str(), "done" | "partial")).map(|o| o.title.clone()).collect();
                v["tell_the_person"] = json!(if deleted.is_empty() {
                    "Nothing was deleted; tell the person why (outcomes[].message).".to_string()
                } else {
                    format!("Tell the person you deleted: {}. Give the predicted frees as predicted; the measured result comes later in memguard_disk recent_cleanups.", deleted.join("; "))
                });
                Ok(v)
            }
            "memguard_advise" => {
                check_keys(args, &[])?;
                to_value(&ops::advise(&(self.load_config)()))
            }
            // ---- Part F: doctor (report only; fixing stays with the person)
            "memguard_doctor" => {
                check_keys(args, &[])?;
                to_value(&crate::doctor::scan(&(self.load_config)()))
            }
            // ---- Part G: RAM and BIOS check (report only; the BIOS is the person's to change)
            "memguard_ram" => {
                check_keys(args, &[])?;
                to_value(&crate::ramcheck::check())
            }
            "memguard_history" => {
                check_keys(args, &["hours"])?;
                let hours = hours_arg(args)?;
                let samples = (self.load_history)(Duration::from_secs_f64(hours * 3600.0));
                Ok(history_result(hours, samples))
            }
            _ => Err(ToolError(format!("Unknown tool: {}", name))),
        }
    }
}

/// Serve on this process's stdin and stdout until stdin closes.
pub fn run_stdio() -> io::Result<()> {
    eprintln!("Ather MemGuard {} MCP server: reading JSON-RPC from stdin", env!("CARGO_PKG_VERSION"));
    let stdin = io::stdin();
    let stdout = io::stdout();
    Server::default().serve(stdin.lock(), stdout.lock())
}

/// Status as `memguard status --json` reports it, including whether the background app runs.
/// Kept in step with `current_status` in `src/bin/memguard.rs` (duplicated on purpose so
/// the CLI and `ops.rs` stay untouched by Part C).
pub fn current_status(cfg: &Config) -> Status {
    let mut st = analyze(cfg, &snapshot());
    st.daemon = win::instance_running(paths::mutex());
    st.disk = crate::disk::status_brief(cfg);
    // Paused / suspended state only the background app knows: take it from its last report.
    if st.daemon {
        if let Ok(text) = std::fs::read_to_string(paths::status_file()) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                st.paused = v["paused"].as_bool().unwrap_or(false);
                st.ue.workers_paused = v["ue"]["workers_paused"].as_u64().unwrap_or(0) as usize;
            }
        }
    }
    st
}

// ---------------------------------------------------------------- protocol helpers

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = if OLDER_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSION };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": SERVER_NAME,
            "title": "Ather MemGuard",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_message<W: Write>(out: &mut W, msg: &Value) -> io::Result<()> {
    // serde_json escapes newlines inside strings, so one message is always one line.
    serde_json::to_writer(&mut *out, msg)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, ToolError> {
    serde_json::to_value(v).map_err(|e| ToolError(format!("could not encode the result: {}", e)))
}

// ---------------------------------------------------------------- tool arguments

fn check_keys(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolError> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        None => Ok(()),
        Some(k) if allowed.is_empty() => Err(ToolError(format!("Unknown argument `{}`: this tool takes no arguments.", k))),
        Some(k) => Err(ToolError(format!("Unknown argument `{}`. Allowed: {}.", k, allowed.join(", ")))),
    }
}

fn workload_arg(args: &Map<String, Value>) -> Result<Option<&'static str>, ToolError> {
    match args.get("workload") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.as_str() {
            "editor" => Ok(Some("editor")),
            "build" => Ok(Some("build")),
            "cook" => Ok(Some("cook")),
            _ => Err(ToolError(format!("workload must be \"editor\", \"build\" or \"cook\", not \"{}\".", s))),
        },
        Some(_) => Err(ToolError("workload must be a string: \"editor\", \"build\" or \"cook\".".into())),
    }
}

fn bool_arg(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError(format!("{} must be true or false.", key))),
    }
}

fn gb_arg(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() && n >= 0.0 => Ok(Some(n)),
            _ => Err(ToolError(format!("{} must be a number of GB, 0 or more.", key))),
        },
    }
}

fn str_arg(args: &Map<String, Value>, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => Err(ToolError(format!("{} must be a folder path as a string.", key))),
    }
}

/// Progress notifications for a disk count, when the client sent a progress token.
fn progress_writer<'a, W: Write>(token: &'a Option<Value>, out: &'a mut W, total: f64, what: &'a str) -> impl FnMut(u64, f64) + 'a {
    move |files, secs| {
        if let Some(token) = token {
            let note = json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": { "progressToken": token, "progress": secs.min(total), "total": total, "message": format!("{}: {} files", what, files) }
            });
            if let Err(e) = write_message(out, &note) {
                eprintln!("memguard mcp: could not send progress: {}", e);
            }
        }
    }
}

fn hours_arg(args: &Map<String, Value>) -> Result<f64, ToolError> {
    match args.get("hours") {
        None | Some(Value::Null) => Ok(HISTORY_DEFAULT_HOURS),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() && n > 0.0 => Ok(n.min(HISTORY_MAX_HOURS)),
            _ => Err(ToolError("hours must be a number of hours, more than 0 (at most 168).".into())),
        },
    }
}

/// The `memguard_history` result: the samples `memguard history --json` gives for the same
/// span, thinned evenly to at most `HISTORY_MAX_SAMPLES` (the newest is always kept).
fn history_result(hours: f64, samples: Vec<Sample>) -> Value {
    let total = samples.len();
    let step = total.div_ceil(HISTORY_MAX_SAMPLES).max(1);
    let last = total.saturating_sub(1);
    let kept: Vec<&Sample> = samples.iter().enumerate().filter(|(i, _)| (last - i) % step == 0).map(|(_, s)| s).collect();
    json!({
        "hours": hours,
        "total_samples": total,
        "step": step,
        "samples": kept,
    })
}

fn timeout_arg(args: &Map<String, Value>) -> Result<Duration, ToolError> {
    let secs = match args.get("timeout_sec") {
        None | Some(Value::Null) => WAIT_DEFAULT_SEC as f64,
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() && n >= 0.0 => n,
            _ => return Err(ToolError("timeout_sec must be a number of seconds, 0 or more.".into())),
        },
    };
    Ok(Duration::from_secs((secs.round() as u64).min(WAIT_MAX_SEC)))
}

// ---------------------------------------------------------------- tool list

fn tool_list() -> Vec<Value> {
    let workload = json!({
        "type": "string",
        "enum": ["editor", "build", "cook"],
        "description": "The Unreal workload about to run. Uses the free RAM and commit configured for it in settings.toml [prime]."
    });
    vec![
        json!({
            "name": "memguard_status",
            "title": "Memory status",
            "description": "Read-only snapshot of this PC's memory for Unreal work: free RAM and commit, \
pressure level (normal, elevated, critical), what Unreal is doing (editor, build, cook), how much each app \
group (browsers, chat, agent apps) holds and what MemGuard does to it, whether the background app is running, \
and advice. Call it when memory looks tight, before deciding whether to start heavy work, or after a build or \
cook failed with out-of-memory errors.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_prime",
            "title": "Free RAM now",
            "description": "Free RAM for Unreal right now: pushes the memory of background apps (browsers, chat, \
agent apps) to the page file and gives them low memory priority. Nothing is closed and no data is lost; those \
apps may lag briefly when next used. Unreal's own processes are never touched. Returns RAM free before and \
after, and with `workload`, whether the PC now has the headroom that workload needs. Before a build or cook, \
prefer memguard_wait with prime: true, which primes and then waits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workload": workload,
                    "kill_orphans": {
                        "type": "boolean",
                        "default": false,
                        "description": "Also end leftover Unreal helper processes (shader workers, crash reporters) whose parent editor, build or cook has exited."
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_wait",
            "title": "Wait for memory",
            "description": "Call this before starting a local Unreal build, cook or packaging run. Blocks until \
the PC has enough free RAM and commit for the workload, or the timeout passes, then returns ready: true or \
false. Use prime: true to free RAM first. Start the build only when ready is true; if it is false, tell the user \
and show memguard_status rather than starting anyway. Give either `workload` or `ram_gb` / `commit_gb` \
(those override the workload's figures).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workload": workload,
                    "ram_gb": { "type": "number", "minimum": 0, "description": "Free RAM needed, in GB. Overrides the workload's figure." },
                    "commit_gb": { "type": "number", "minimum": 0, "description": "Free commit charge needed, in GB. Overrides the workload's figure." },
                    "timeout_sec": { "type": "integer", "minimum": 0, "maximum": WAIT_MAX_SEC, "default": WAIT_DEFAULT_SEC, "description": "Give up after this many seconds. At most 3600." },
                    "prime": { "type": "boolean", "default": false, "description": "Free RAM once (like memguard_prime) before waiting. Recommended before a build or cook." },
                    "disk_gb": { "type": "number", "minimum": 0, "description": "Free disk space needed on the project's drive, in GB. Overrides the workload's disk_gb. The drives holding Unreal's DDC, Zen and TEMP are checked against [disk] cache_min_free_gb too." },
                    "disk_path": { "type": "string", "description": "The project folder whose drive needs the space, e.g. D:\\s2. Default: the server's current folder." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_advise",
            "title": "Build advice",
            "description": "Read-only. Suggests how many parallel compile actions UnrealBuildTool should use for \
the memory available now (-MaxParallelActions=N or <MaxParallelActions> in BuildConfiguration.xml), with notes \
such as closing the editor during a build. Call it before starting a local build, after memguard_wait.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        // ---- Part F: doctor
        json!({
            "name": "memguard_doctor",
            "title": "Checkup",
            "description": "Read-only checkup: which running apps use memory that Unreal work doesn't need, and \
what could be done. Each finding has an id, a verdict (close = not needed for Unreal, hold_back = wanted but \
should give way, review = unknown and large, heavy_tool = a big tool that is probably deliberate), the memory it \
holds, plain-words detail and the proposed actions (close, end, disable_startup, hold_back, block_while_unreal, \
leave_it); `recommended` marks the safe ones. Also startup entries worth switching off, leftover Unreal helpers \
and PC-level problems, plus how much could be freed. It changes nothing. Relay the findings to the person in a \
few lines and ask before acting: the person runs `memguard doctor --fix` (the recommended ones) or \
`memguard doctor --fix <id>[:action]`, and `memguard doctor --undo` reverses it. Never close apps yourself.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_history",
            "title": "Memory history",
            "description": "Read-only. What memory did over the last hours, as recorded by the background app: \
one sample per interval with time, Unreal mode, pressure level, free RAM and commit, what Unreal and the editor \
had committed, compile workers, and RAM per app group. Call it to explain a slow or failed build after the fact, \
to see whether the editor keeps growing (a leak), or to check how long memory has been tight. Spans over about \
720 samples are thinned evenly (`step` says by how much); the newest sample is always included.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hours": { "type": "number", "exclusiveMinimum": 0, "maximum": HISTORY_MAX_HOURS, "default": HISTORY_DEFAULT_HOURS, "description": "How far back to look, in hours. At most 168 (a week)." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        // ---- Part G: RAM and BIOS check
        json!({
            "name": "memguard_ram",
            "title": "RAM and BIOS check",
            "description": "Read-only RAM and BIOS check: whether this PC's memory runs below the speed its kit \
is rated for (XMP / EXPO switched off in the BIOS), whether the sticks are in the right slots, whether the BIOS \
may be out of date, and CPU-specific cautions. Returns the verdict (below_rated, at_rated, no_profile, mixed_kit, \
unknown, not_applicable), a headline, a summary, the sticks and slots, the rated speed with its source \
(confidence), findings with severity, and numbered BIOS steps for this board when there is something to do. It \
changes nothing. Relay the headline in one or two lines and offer the steps; only the person can change BIOS \
settings, so never claim to have changed anything, and don't paraphrase the steps into different settings.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        // ---- Disk and Dev Drive (report only)
        json!({
            "name": "memguard_disk",
            "title": "Drives and Dev Drive",
            "description": "Read-only. Every drive once (by volume id): free space available to this user, level \
(normal, elevated, critical: below the smaller of the GB and percent settings), roles (dev_drive, system, pagefile, \
projects, ddc, zen, temp), Dev Drive and trusted state, block cloning, plus Dev Drive health findings (dev:not_trusted, \
dev:perf_mode_off, dev:av_disallowed, dev:caches_on_c, dev:vhd_host_low, dev:third_party_av) with the exact admin \
command and how to check it. Fast: no folder is counted. Mention dev:* findings once; never claim to have fixed one.",
            "inputSchema": {
                "type": "object",
                "properties": { "refresh": { "type": "boolean", "default": false, "description": "Read the Defender and Dev Drive facts again instead of today's saved ones." } },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_disk_checkup",
            "title": "Disk checkup",
            "description": "Read-only and report-only: what could go from a drive and what removing it would really free, \
counted without double counting (block clones, hard links, nested folders). Items: project build files, caches, logs, \
lanes and worktrees, with ids d1, d2, ... Numbers: logical_gb (holds), footprint_gb (occupies), frees_alone_gb with \
frees_confidence (exact, lower_bound = at least, estimate = about, upper_bound = at most, unknown) and \
frees_basis: predicted. Quote only totals[].frees_together_gb or one item's frees_alone_gb, with its confidence word, \
and say it is predicted; never add numbers yourself and never present logical_gb as space that can be freed. Answers \
from the saved result (with its age) unless refresh is true. Deletes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Only this folder: a project, a lane, or a folder of them. Default: the configured project folders, projects on Dev Drives and the shared caches." },
                    "refresh": { "type": "boolean", "default": false, "description": "Count again instead of answering from the saved result." },
                    "budget_sec": { "type": "integer", "minimum": 1, "maximum": DISK_BUDGET_MAX_SEC, "default": DISK_BUDGET_DEFAULT_SEC, "description": "How long counting may take, in seconds. At most 300. Items not finished are lower_bound." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_disk_plan",
            "title": "Disk plan",
            "description": "Read-only. Counts exactly the items you name (ids from the latest memguard_disk_checkup) \
again, fresh, and returns per drive frees_together_gb (counted once across the set), free_now_gb and free_after_gb, \
all predicted. Use it to decide whether removing a set makes enough room for a job; rely only on exact or lower_bound. \
On ReFS, deleted space can take a long time to show up as free: never promise the space right after a delete. \
Ather MemGuard deletes nothing; the person removes the folders (or runs git worktree remove for a lane).",
            "inputSchema": {
                "type": "object",
                "properties": { "ids": { "type": "array", "items": { "type": "string" }, "description": "Item ids from the latest disk checkup, e.g. [\"d7\", \"d9\"]. Empty = all of them." } },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_disk_clean",
            "title": "Delete regenerable build output",
            "description": "DELETES FILES for good (no undo). Only for regenerable items (class regenerable: Intermediate, \
DerivedDataCache, crash reports, shader debug files, tool caches) whose ids are in the memguard_disk_plan named by plan_id \
(required; made by this server less than 10 minutes ago). The class and the allowed actions come from Ather MemGuard's \
built-in list, never from the plan. Every safety rule is checked again now and an item is refused whole if anything in it was written in the last \
2 hours or since the plan, if an Unreal editor, build or cook works in that project, if git tracks a file in it, if its folder \
changed (file id), or if it is a link, in a cloud folder, or outside the allow-list. Lanes, worktrees, Content, Config, \
Source, Saved\\Autosaves and the like are never touched, and bin moves and tool cleans are not available here. Returns \
the predicted frees per drive (predicted, not measured) and outcomes per item; the measured result comes later in \
memguard_disk recent_cleanups. Tell the person what you deleted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Ids of regenerable items from your latest memguard_disk_plan, e.g. [\"d1\", \"d2\"]." },
                    "plan_id": { "type": "string", "description": "Required. The plan_id memguard_disk_plan returned in this session, less than 10 minutes ago." }
                },
                "required": ["ids", "plan_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false },
        }),
    ]
}

/// The memguard.exe agents should start, for `agent_setup_text`: the installed copy if there
/// is one, else `fallback` (the CLI passes itself; the tray app the memguard.exe next to it).
pub fn agent_cli_path(fallback: std::path::PathBuf) -> std::path::PathBuf {
    let installed = paths::install_dir().join(paths::CLI_EXE);
    if installed.exists() {
        installed
    } else {
        fallback
    }
}

/// What `memguard agent-setup` prints and the tray's "Copy setup" puts on the clipboard:
/// ready-to-paste setup for Claude Code and Codex, starting `exe`.
pub fn agent_setup_text(exe: &std::path::Path) -> String {
    let exe = exe.display().to_string();
    let json_exe = serde_json::to_string(&exe).unwrap_or_else(|_| format!("\"{}\"", exe));
    let mut s = String::new();
    let mut line = |l: &str| {
        s.push_str(l);
        s.push('\n');
    };
    line("Ather MemGuard for AI agents: an MCP server your agent starts by itself (no port, nothing to enable).");
    line("Tools: memguard_status, memguard_prime, memguard_wait, memguard_advise, memguard_doctor, memguard_history, memguard_ram,");
    line("       memguard_disk, memguard_disk_checkup, memguard_disk_plan, memguard_disk_clean (deletes regenerable build output only).");
    line("");
    line("Claude Code, for all your projects:");
    line(&format!("  claude mcp add --scope user ather-memguard -- \"{}\" mcp", exe));
    line("If `memguard` is on your PATH, this short form works too:");
    line("  claude mcp add ather-memguard -- memguard mcp");
    line("");
    line("Claude Code, shared with a project (.mcp.json in the repo root):");
    line(&format!("  {{ \"mcpServers\": {{ \"ather-memguard\": {{ \"command\": {}, \"args\": [\"mcp\"] }} }} }}", json_exe));
    line("");
    line(r"Codex (%USERPROFILE%\.codex\config.toml):");
    line("  [mcp_servers.ather-memguard]");
    line(&format!("  command = '{}'", exe));
    line("  args = [\"mcp\"]");
    line("  tool_timeout_sec = 3700   # memguard_wait can block for up to an hour");
    line("");
    line("Before a local build or cook, have the agent call memguard_wait with prime: true.");
    line("More: docs/AI-AGENTS.md");
    s
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new(Box::new(Config::builtin))
    }

    /// Feed lines through `serve` and return every line written, parsed.
    fn drive(input: &str) -> Vec<Value> {
        let mut out = Vec::new();
        server().serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every output line is one JSON message"))
            .collect()
    }

    fn one(input: &str) -> Value {
        let mut replies = drive(input);
        assert_eq!(replies.len(), 1, "expected one reply, got {:?}", replies);
        replies.remove(0)
    }

    fn call(id: u32, name: &str, args: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } })
            .to_string()
    }

    fn tool_error_text(reply: &Value) -> String {
        assert_eq!(reply["result"]["isError"], true, "expected a tool error: {}", reply);
        assert!(reply.get("error").is_none());
        reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn initialize_reports_server_and_tools_capability() {
        let r = one(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#);
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 1);
        let res = &r["result"];
        assert_eq!(res["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(res["serverInfo"]["name"], "ather-memguard");
        assert_eq!(res["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(res["capabilities"], json!({ "tools": {} }));
    }

    #[test]
    fn initialize_negotiates_version() {
        let r = one(r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#);
        assert_eq!(r["id"], "a");
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let r = one(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#);
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn ping_returns_empty_result() {
        assert_eq!(one(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#)["result"], json!({}));
    }

    #[test]
    fn tools_list_has_the_tools_with_schemas() {
        let r = one(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["memguard_status", "memguard_prime", "memguard_wait", "memguard_advise", "memguard_doctor", "memguard_history", "memguard_ram", "memguard_disk", "memguard_disk_checkup", "memguard_disk_plan", "memguard_disk_clean"]
        );
        for t in tools.iter().filter(|t| t["name"].as_str().unwrap().starts_with("memguard_disk")) {
            let deletes = t["name"] == "memguard_disk_clean";
            assert_eq!(t["annotations"]["readOnlyHint"], !deletes, "{}", t["name"]);
            assert_eq!(t["annotations"]["destructiveHint"] == true, deletes, "{}", t["name"]);
        }
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            assert!(t["inputSchema"]["properties"].is_object());
            assert!(t["description"].as_str().unwrap().len() > 80);
        }
        let wait = &tools[2]["inputSchema"]["properties"];
        assert_eq!(wait["timeout_sec"]["maximum"], 3600);
        assert_eq!(wait["workload"]["enum"], json!(["editor", "build", "cook"]));
    }

    #[test]
    fn status_call_returns_cli_json_as_text_and_structured() {
        let r = one(&call(3, "memguard_status", json!({})));
        let res = &r["result"];
        assert_eq!(res["isError"], false);
        let s = &res["structuredContent"];
        for key in ["time", "daemon", "paused", "mode", "activities", "pressure", "system", "ue", "groups", "advice"] {
            assert!(s.get(key).is_some(), "status lacks {}", key);
        }
        assert!(s["daemon"].is_boolean());
        assert!(s["system"]["ram_total_gb"].as_f64().unwrap() > 0.0);
        assert_eq!(res["content"][0]["type"], "text");
        let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, s);
        // Arguments may also be left out entirely.
        let r = one(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memguard_status"}}"#);
        assert_eq!(r["result"]["isError"], false);
    }

    #[test]
    fn advise_call_works() {
        let r = one(&call(5, "memguard_advise", json!({})));
        let s = &r["result"]["structuredContent"];
        assert!(s["suggested_max_parallel_actions"].as_u64().unwrap() >= 1);
        assert!(s["notes"].is_array());
    }

    // ---- Part F: doctor. Read-only: scans this PC and changes nothing.
    #[test]
    fn doctor_call_returns_the_report() {
        let r = one(&call(14, "memguard_doctor", json!({})));
        let res = &r["result"];
        assert_eq!(res["isError"], false, "{}", r);
        let s = &res["structuredContent"];
        for key in ["time", "system", "unreal_active", "findings", "reclaimable_ram_gb", "reclaimable_commit_gb"] {
            assert!(s.get(key).is_some(), "doctor report lacks {}", key);
        }
        for f in s["findings"].as_array().unwrap() {
            for key in ["id", "kind", "verdict", "title", "detail", "processes", "ram_mb", "committed_mb", "actions", "recommended"] {
                assert!(f.get(key).is_some(), "finding lacks {}: {}", key, f);
            }
            assert_ne!(f["verdict"], "keep");
        }
        let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, s);
        let tools = one(r#"{"jsonrpc":"2.0","id":15,"method":"tools/list"}"#);
        let doctor = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "memguard_doctor").unwrap().clone();
        assert_eq!(doctor["annotations"]["readOnlyHint"], true);
        assert!(doctor["description"].as_str().unwrap().contains("ask before acting"));
        let t = tool_error_text(&one(&call(16, "memguard_doctor", json!({ "fix": true }))));
        assert!(t.contains("fix"), "{}", t);
    }

    // ---- Part G: RAM and BIOS check. Read-only: reads the firmware tables and changes nothing.
    #[test]
    fn ram_call_returns_the_report() {
        let r = one(&call(17, "memguard_ram", json!({})));
        let res = &r["result"];
        assert_eq!(res["isError"], false, "{}", r);
        let s = &res["structuredContent"];
        for key in ["verdict", "headline", "summary", "sticks", "slots_total", "rated", "current_mhz", "board", "cpu", "findings", "steps", "fallback_mhz", "fingerprint"] {
            assert!(s.get(key).is_some(), "ram report lacks {}", key);
        }
        let verdicts = ["below_rated", "at_rated", "no_profile", "mixed_kit", "unknown", "not_applicable"];
        assert!(verdicts.contains(&s["verdict"].as_str().unwrap()), "{}", s["verdict"]);
        assert!(!s["headline"].as_str().unwrap().is_empty());
        let text: Value = serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, s);
        let tools = one(r#"{"jsonrpc":"2.0","id":18,"method":"tools/list"}"#);
        let ram = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "memguard_ram").unwrap().clone();
        assert_eq!(ram["annotations"]["readOnlyHint"], true);
        let d = ram["description"].as_str().unwrap();
        assert!(d.contains("headline") && d.contains("only the person can change BIOS"), "{}", d);
        let t = tool_error_text(&one(&call(19, "memguard_ram", json!({ "apply": true }))));
        assert!(t.contains("apply"), "{}", t);
    }

    // ---- Disk: read-only. The checkup counts a small tree this test creates, in-process.
    #[test]
    fn disk_tools_over_stdio() {
        let root = std::env::temp_dir().join(format!("amg-mcp-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"Game\Intermediate")).unwrap();
        std::fs::write(root.join(r"Game\Game.uproject"), b"{}").unwrap();
        std::fs::write(root.join(r"Game\Intermediate\x.bin"), vec![3u8; 200_000]).unwrap();
        let srv = server().with_disk(
            Box::new(|job, p| Ok(crate::disk_scan::run_job(job, p))),
            Box::new(|| None),
        );
        let run = |line: &str| -> Vec<Value> {
            let mut out = Vec::new();
            srv.serve(line.as_bytes(), &mut out).unwrap();
            String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
        };
        // memguard_disk
        let r = run(&call(1, "memguard_disk", json!({})));
        let s = &r[0]["result"]["structuredContent"];
        assert_eq!(r[0]["result"]["isError"], false, "{}", r[0]);
        assert!(s["volumes"].as_array().unwrap().iter().any(|v| v["free_basis"] == "available_to_caller"));
        assert!(s["dev_drive"]["av_filters"].is_string());
        // memguard_disk_checkup on the test tree, with progress allowed
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "memguard_disk_checkup", "arguments": { "path": root.display().to_string(), "refresh": true, "budget_sec": 30 }, "_meta": { "progressToken": "d" } } });
        let out = run(&req.to_string());
        let reply = out.iter().find(|m| m.get("id").is_some()).unwrap();
        assert_eq!(reply["result"]["isError"], false, "{}", reply);
        let c = &reply["result"]["structuredContent"];
        assert_eq!(c["basis"], "predicted");
        assert_eq!(c["items"][0]["key"], "ue:intermediate");
        assert_eq!(c["items"][0]["frees_basis"], "predicted");
        assert!(c["do_not_sum"].as_str().unwrap().contains("never promise"));
        // memguard_disk_plan without a saved checkup is a tool error that says what to do
        let t = tool_error_text(&run(&call(3, "memguard_disk_plan", json!({ "ids": ["d1"] })))[0]);
        assert!(t.contains("memguard_disk_checkup"), "{}", t);
        tool_error_text(&run(&call(4, "memguard_disk_plan", json!({ "ids": "d1" })))[0]);
        tool_error_text(&run(&call(5, "memguard_disk_checkup", json!({ "budget_sec": 0 })))[0]);
        tool_error_text(&run(&call(6, "memguard_disk", json!({ "clean": true })))[0]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Set every file's and folder's write time under `root` to `secs` ago (deepest first).
    fn age_tree(root: &std::path::Path, secs: u64) {
        use std::os::windows::fs::OpenOptionsExt;
        let when = std::time::SystemTime::now() - Duration::from_secs(secs);
        fn walk(p: &std::path::Path, when: std::time::SystemTime) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path(), when);
                }
            }
            // FILE_WRITE_ATTRIBUTES, and FILE_FLAG_BACKUP_SEMANTICS so folders open too.
            if let Ok(f) = std::fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(p) {
                let _ = f.set_modified(when);
            }
        }
        walk(root, when);
    }

    // ---- The one tool that deletes, over stdio, on a tree this test creates.
    #[test]
    fn disk_clean_over_stdio_deletes_only_what_the_rules_allow() {
        use crate::disk_clean::{CleanOpts, DiskFiles, PlanRecord};
        let root = std::env::temp_dir().join(format!("amg-mcp-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let game = root.join("Game");
        for d in [r"Intermediate\Build\Win64", r"Saved\Logs", r"Saved\Autosaves", "Content", "Config"] {
            std::fs::create_dir_all(game.join(d)).unwrap();
        }
        std::fs::write(game.join("Game.uproject"), b"{}").unwrap();
        std::fs::write(game.join(r"Intermediate\Build\Win64\a.obj"), vec![1u8; 300_000]).unwrap();
        std::fs::write(game.join(r"Saved\Logs\Game.log"), vec![2u8; 50_000]).unwrap();
        std::fs::write(game.join(r"Saved\Autosaves\Map_auto1.umap"), b"autosave").unwrap();
        std::fs::write(game.join(r"Content\Hero.uasset"), b"asset").unwrap();
        std::fs::write(game.join(r"Config\DefaultGame.ini"), b"[x]").unwrap();
        age_tree(&root, 3 * 3600);
        // The checkup the plan builds on, counted in-process.
        let cfg = Config::builtin();
        let items = crate::disk_scan::discover(&cfg, Some(&root.display().to_string()));
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["ue:intermediate", "ue:logs"]);
        let job = crate::disk_scan::ScanJob { items: items.iter().map(|i| i.paths.clone()).collect(), sets: vec![], threads: 2, budget_sec: 30, stop_on_build: false };
        let out = crate::disk_scan::run_job(&job, &mut |_, _| {});
        let rep = crate::disk_scan::build_checkup(&items, &out, &[], &cfg.disk(), 0, false, &[], "now");
        let cached = crate::disk_scan::CachedCheckup { scope: "t".into(), unix: 0, time: String::new(), items: items.clone(), root_ids: vec![], last_writes: vec![], report: rep };
        let files = DiskFiles::in_dir(root.join("data"));
        let srv = server()
            .with_disk(Box::new(|job, p| Ok(crate::disk_scan::run_job(job, p))), Box::new(move || Some(cached.clone())))
            .with_clean(files.clone(), CleanOpts { presample_secs: 1, start_watcher: false, busy: Some((vec![], false)), ..Default::default() });
        let run = |id: u32, name: &str, args: Value| -> Value {
            let mut o = Vec::new();
            srv.serve(call(id, name, args).as_bytes(), &mut o).unwrap();
            let msgs: Vec<Value> = String::from_utf8(o).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
            msgs.into_iter().find(|m| m.get("id").is_some()).unwrap()
        };
        let plan = |id: u32| -> String {
            let r = run(id, "memguard_disk_plan", json!({ "ids": ["d1", "d2"] }));
            assert_eq!(r["result"]["isError"], false, "{}", r);
            r["result"]["structuredContent"]["plan_id"].as_str().unwrap().to_string()
        };
        // No plan yet: refused, and nothing is touched. plan_id is required.
        assert!(tool_error_text(&run(1, "memguard_disk_clean", json!({ "ids": ["d1"] }))).contains("plan_id is required"));
        assert!(tool_error_text(&run(1, "memguard_disk_clean", json!({ "ids": ["d1"], "plan_id": "p1" }))).contains("memguard_disk_plan"));
        let p1 = plan(2);
        assert!(tool_error_text(&run(1, "memguard_disk_clean", json!({ "ids": ["d1"] }))).contains("plan_id is required"), "no newest-plan fallback");
        // Logs are disposable: not for agents.
        let r = run(3, "memguard_disk_clean", json!({ "ids": ["d2"], "plan_id": p1 }));
        let s = &r["result"]["structuredContent"];
        assert_eq!(s["outcomes"][0]["result"], "refused", "{}", s);
        assert!(s["outcomes"][0]["message"].as_str().unwrap().contains("regenerable"));
        assert!(game.join(r"Saved\Logs\Game.log").is_file());
        // Found in review: editing disk-plans.json (class -> regenerable) let the agent delete
        // Saved\Logs. The file is never read for agents; the class comes from the catalogue.
        let mut on_disk = files.plans();
        for p in on_disk.iter_mut() {
            for it in p.items.iter_mut() {
                it.class = "regenerable".into();
            }
        }
        let mut forged_file = on_disk.last().unwrap().clone();
        forged_file.id = "pfile".into();
        on_disk.push(forged_file);
        std::fs::write(root.join(r"data\disk-plans.json"), serde_json::to_string(&on_disk).unwrap()).unwrap();
        let r = run(3, "memguard_disk_clean", json!({ "ids": ["d2"], "plan_id": p1 }));
        assert_eq!(r["result"]["structuredContent"]["outcomes"][0]["result"], "refused", "{}", r);
        assert!(tool_error_text(&run(3, "memguard_disk_clean", json!({ "ids": ["d2"], "plan_id": "pfile" }))).contains("no plan pfile"), "a plan only in the file is unknown");
        assert!(game.join(r"Saved\Logs\Game.log").is_file());
        // A forged class in the server's own plan (it can't happen through the tools, but the
        // catalogue decides anyway), and cache:old: refused.
        {
            let mut mine = srv.agent_plans.borrow_mut();
            let mut f = mine.last().unwrap().clone();
            f.id = "pclass".into();
            f.items[1].class = "regenerable".into();
            let mut old = f.items[0].clone();
            old.id = "d9".into();
            old.key = "cache:old".into();
            old.class = "disposable".into();
            f.items.push(old);
            mine.push(f);
        }
        let r = run(3, "memguard_disk_clean", json!({ "ids": ["d2", "d9"], "plan_id": "pclass" }));
        let s = &r["result"]["structuredContent"];
        assert_eq!(s["outcomes"][0]["result"], "refused", "{}", s);
        assert!(s["outcomes"][0]["message"].as_str().unwrap().contains("class was changed"), "{}", s);
        assert_eq!(s["outcomes"][1]["result"], "refused", "{}", s);
        assert!(s["outcomes"][1]["message"].as_str().unwrap().contains("regenerable"), "{}", s);
        assert!(game.join(r"Saved\Logs\Game.log").is_file() && game.join(r"Intermediate\Build\Win64\a.obj").is_file());
        // A stale plan (older than 10 minutes) is refused.
        for p in srv.agent_plans.borrow_mut().iter_mut() {
            p.unix -= 700;
        }
        assert!(tool_error_text(&run(4, "memguard_disk_clean", json!({ "ids": ["d1"], "plan_id": p1 }))).contains("under 10 minutes"));
        // A recent write refuses the whole item.
        let p2 = plan(5);
        std::fs::write(game.join(r"Intermediate\Build\fresh.txt"), b"just now").unwrap();
        let r = run(6, "memguard_disk_clean", json!({ "ids": ["d1"], "plan_id": p2 }));
        let s = &r["result"]["structuredContent"];
        assert_eq!(s["outcomes"][0]["result"], "refused", "{}", s);
        assert!(s["outcomes"][0]["message"].as_str().unwrap().contains("last 2 hours"), "{}", s);
        assert!(game.join(r"Intermediate\Build\Win64\a.obj").is_file(), "nothing in it was touched");
        // Old again, but written after the plan: refused until a new plan.
        age_tree(&game.join("Intermediate"), 2 * 3600 + 600);
        let r = run(7, "memguard_disk_clean", json!({ "ids": ["d1"], "plan_id": p2 }));
        assert!(r["result"]["structuredContent"]["outcomes"][0]["message"].as_str().unwrap().contains("after the plan"), "{}", r);
        // A protected path smuggled into a plan record is refused by the rules at apply time.
        let mut forged: PlanRecord = srv.agent_plans.borrow().last().unwrap().clone();
        forged.id = "pforged".into();
        forged.unix = crate::disk_clean::unix_now();
        forged.items[0].paths = vec![game.join("Content").display().to_string()];
        forged.items[0].path_ids = vec![crate::disk_win::identity(&game.join("Content").display().to_string()).map(|x| x.1)];
        forged.items[0].last_write = u64::MAX;
        srv.agent_plans.borrow_mut().push(forged);
        let r = run(8, "memguard_disk_clean", json!({ "ids": ["d1"], "plan_id": "pforged" }));
        let s = &r["result"]["structuredContent"];
        assert_eq!(s["outcomes"][0]["result"], "refused", "{}", s);
        assert!(s["outcomes"][0]["message"].as_str().unwrap().contains("never touches"), "{}", s);
        assert!(game.join(r"Content\Hero.uasset").is_file());
        // A fresh plan: deleted, predicted (not measured), and the person is to be told.
        let p3 = plan(9);
        let r = run(10, "memguard_disk_clean", json!({ "ids": ["d1"], "plan_id": p3 }));
        let s = &r["result"]["structuredContent"];
        assert_eq!(r["result"]["isError"], false, "{}", r);
        assert_eq!(s["outcomes"][0]["result"], "done", "{}", s);
        assert_eq!(s["outcomes"][0]["basis"], "predicted");
        assert!(s["volumes"][0]["predicted_freed_gb"].is_number());
        assert_eq!(s["volumes"][0]["measured"], "later");
        assert!(s["measured_note"].as_str().unwrap().contains("not yet measured"));
        assert!(s["tell_the_person"].as_str().unwrap().contains("Build files"), "{}", s);
        assert!(!game.join("Intermediate").exists());
        for keep in [r"Content\Hero.uasset", r"Config\DefaultGame.ini", r"Saved\Autosaves\Map_auto1.umap", r"Saved\Logs\Game.log", "Game.uproject"] {
            assert!(game.join(keep).is_file(), "{} must survive", keep);
        }
        // The delete is logged as can't-be-undone, and a measured watch waits for the watcher.
        assert!(!files.undo_log().can_undo());
        let w = files.watches();
        assert_eq!(w.len(), 1);
        assert!(w[0].measured.is_none() && w[0].predicted > 0);
        tool_error_text(&run(11, "memguard_disk_clean", json!({ "ids": [] })));
        tool_error_text(&run(12, "memguard_disk_clean", json!({ "ids": ["d1"], "force": true })));
        let _ = std::fs::remove_dir_all(&root);
    }

    // The wait gate: a disk need nobody can meet returns at once, blocked by disk.
    #[test]
    fn wait_blocked_by_disk_returns_at_once() {
        let started = std::time::Instant::now();
        let r = one(&call(20, "memguard_wait", json!({ "disk_gb": 1.0e9, "disk_path": std::env::temp_dir().display().to_string(), "timeout_sec": 600 })));
        let s = &r["result"]["structuredContent"];
        assert_eq!(r["result"]["isError"], false, "{}", r);
        assert_eq!(s["ready"], false);
        assert_eq!(s["blocked_by"], json!(["disk"]));
        assert!(s["disk_free_gb"].is_number() && s["need_disk_gb"] == 1.0e9);
        assert!(started.elapsed() < Duration::from_secs(20));
        tool_error_text(&one(&call(21, "memguard_wait", json!({ "disk_gb": 10, "disk_path": r"Q:\no\such\folder" }))));
    }

    fn sample(unix: i64) -> Sample {
        Sample {
            unix,
            time: format!("t{}", unix),
            mode: "editor".into(),
            pressure: crate::engine::Level::Normal,
            ram_free_gb: 40.0,
            commit_free_gb: 30.0,
            ue_committed_gb: 20.0,
            editor_committed_gb: 18.0,
            workers: 0,
            groups: vec![crate::history::GroupRam { name: "Browsers".into(), ram_gb: 2.5 }],
            editors: Vec::new(),
        }
    }

    #[test]
    fn history_call_returns_the_samples() {
        use std::sync::{Arc, Mutex};
        let asked: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let a = asked.clone();
        let srv = server().with_history(Box::new(move |d| {
            a.lock().unwrap().push(d);
            (0..3).map(|i| sample(1000 + i * 10)).collect()
        }));
        let run = |line: &str| {
            let mut out = Vec::new();
            srv.serve(line.as_bytes(), &mut out).unwrap();
            serde_json::from_slice::<Value>(String::from_utf8(out).unwrap().trim().as_bytes()).unwrap()
        };
        let r = run(&call(1, "memguard_history", json!({})));
        let s = &r["result"]["structuredContent"];
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(s["hours"], 1.0);
        assert_eq!(s["total_samples"], 3);
        let samples = s["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 3);
        // The same fields as `memguard history --json`.
        assert_eq!(samples[0], serde_json::to_value(sample(1000)).unwrap());
        assert_eq!(samples[2]["groups"][0]["name"], "Browsers");
        run(&call(2, "memguard_history", json!({ "hours": 500 })));
        run(&call(3, "memguard_history", json!({ "hours": 6 })));
        let asked = asked.lock().unwrap().clone();
        assert_eq!(asked, [Duration::from_secs(3600), Duration::from_secs(168 * 3600), Duration::from_secs(6 * 3600)]);
        for bad in [json!({ "hours": 0 }), json!({ "hours": -1 }), json!({ "hours": "a day" }), json!({ "days": 1 })] {
            tool_error_text(&run(&call(4, "memguard_history", bad)));
        }
    }

    #[test]
    fn long_history_is_thinned_keeping_the_newest() {
        let samples: Vec<Sample> = (0..2000).map(sample).collect();
        let v = history_result(168.0, samples);
        let kept = v["samples"].as_array().unwrap();
        assert_eq!(v["step"], 3);
        assert!(kept.len() <= HISTORY_MAX_SAMPLES);
        assert_eq!(kept.last().unwrap()["unix"], 1999);
        assert_eq!(history_result(1.0, (0..5).map(sample).collect())["samples"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn agent_setup_text_names_the_exe_and_every_tool() {
        let t = agent_setup_text(std::path::Path::new(r"C:\Tools\memguard.exe"));
        assert!(t.contains(r#"claude mcp add --scope user ather-memguard -- "C:\Tools\memguard.exe" mcp"#), "{}", t);
        assert!(t.contains(r#""command": "C:\\Tools\\memguard.exe""#), "{}", t);
        assert!(t.contains(r"command = 'C:\Tools\memguard.exe'"), "{}", t);
        for tool in tool_list() {
            assert!(t.contains(tool["name"].as_str().unwrap()), "{}", tool["name"]);
        }
        assert!(t.ends_with("More: docs/AI-AGENTS.md\n"));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let r = one("{not json\n");
        assert_eq!(r["id"], Value::Null);
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        // The server keeps going after a bad line.
        let replies = drive("garbage\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n");
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["id"], 9);
    }

    #[test]
    fn non_object_and_missing_method_are_invalid_requests() {
        assert_eq!(one("[1,2]")["error"]["code"], INVALID_REQUEST);
        assert_eq!(one(r#"{"jsonrpc":"2.0","id":3}"#)["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let r = one(r#"{"jsonrpc":"2.0","id":11,"method":"resources/list"}"#);
        assert_eq!(r["id"], 11);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn notifications_and_client_responses_get_no_reply() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"no/such/method"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":99,"result":{}}"#,
            "\n",
            "\n",
        );
        assert!(drive(input).is_empty());
    }

    #[test]
    fn bad_call_params_are_protocol_errors() {
        let r = one(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"arguments":{}}}"#);
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = one(&call(2, "memguard_nope", json!({})));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = one(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memguard_status","arguments":[1]}}"#);
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = one(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":"memguard_status"}"#);
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    // These fail validation before anything is primed or waited on.
    #[test]
    fn bad_tool_arguments_are_tool_errors() {
        let t = tool_error_text(&one(&call(1, "memguard_prime", json!({ "workload": "shaders" }))));
        assert!(t.contains("editor"), "{}", t);
        tool_error_text(&one(&call(2, "memguard_prime", json!({ "kill_orphans": "yes" }))));
        tool_error_text(&one(&call(3, "memguard_prime", json!({ "workload": 3 }))));
        tool_error_text(&one(&call(4, "memguard_status", json!({ "verbose": true }))));
        let t = tool_error_text(&one(&call(5, "memguard_wait", json!({}))));
        assert!(t.contains("Say what to wait for"), "{}", t);
        tool_error_text(&one(&call(6, "memguard_wait", json!({ "workload": "build", "timeout_sec": -5 }))));
        tool_error_text(&one(&call(7, "memguard_wait", json!({ "ram_gb": "lots" }))));
        tool_error_text(&one(&call(8, "memguard_wait", json!({ "workload": "cook", "prime": 1 }))));
        tool_error_text(&one(&call(9, "memguard_wait", json!({ "ram_gb": 0, "commit_gb": 0 }))));
        let t = tool_error_text(&one(&call(10, "memguard_wait", json!({ "workload": "build", "timout_sec": 5 }))));
        assert!(t.contains("timout_sec"), "{}", t);
    }

    #[test]
    fn timeout_is_clamped() {
        let args = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(timeout_arg(&args(json!({}))).ok().unwrap(), Duration::from_secs(600));
        assert_eq!(timeout_arg(&args(json!({ "timeout_sec": 99999 }))).ok().unwrap(), Duration::from_secs(3600));
        assert_eq!(timeout_arg(&args(json!({ "timeout_sec": 30 }))).ok().unwrap(), Duration::from_secs(30));
    }

    // Read-only: waits for an impossible amount of free RAM (no prime), times out and
    // sends progress notifications on the way.
    #[test]
    fn wait_times_out_with_progress() {
        let req = json!({
            "jsonrpc": "2.0", "id": 12, "method": "tools/call",
            "params": { "name": "memguard_wait", "arguments": { "ram_gb": 1.0e9, "timeout_sec": 3 }, "_meta": { "progressToken": "p1" } }
        });
        let out = drive(&req.to_string());
        let (notes, replies): (Vec<_>, Vec<_>) = out.into_iter().partition(|m| m.get("id").is_none());
        assert!(!notes.is_empty());
        assert!(notes.iter().all(|n| n["method"] == "notifications/progress" && n["params"]["progressToken"] == "p1"));
        assert_eq!(replies.len(), 1);
        let s = &replies[0]["result"]["structuredContent"];
        assert_eq!(replies[0]["result"]["isError"], false);
        assert_eq!(s["ready"], false);
        assert!(s["waited_sec"].as_u64().unwrap() >= 3);
        // A wait that is satisfied at once (no prime) returns ready.
        let r = one(&call(13, "memguard_wait", json!({ "commit_gb": 0.001, "timeout_sec": 0 })));
        assert_eq!(r["result"]["structuredContent"]["ready"], true);
    }
}

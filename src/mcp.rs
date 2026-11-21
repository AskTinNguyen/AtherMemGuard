//! MCP server over stdio, so coding agents can call MemGuard as tools.
//!
//! JSON-RPC 2.0, one JSON message per line, protocol version 2025-06-18. The agent starts
//! `memguard mcp` itself, so there is no port, no token and nothing to enable.
//!
//! Only protocol messages go to the writer (stdout). Diagnostics go to stderr.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

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
result comes later, in memguard_disk recent_cleanups: report it as measured, never earlier, and only as what was \
observed (\"free space on D: rose by X GB in the N minutes after the cleanup\"), never as freed by it; couldnt_measure \
is not 0. Moves to the bin, tool \
cleans, logs, cooked output and the shared caches are the person's call (memguard disk clean --fix); lanes and worktrees \
are never deleted by Ather MemGuard. Mention dev:* findings from memguard_disk once and never claim to have fixed them.";

/// Settings source. The real server reads settings.toml (cached by file mtime+size);
/// tests pass the built-in defaults.
pub type ConfigLoader = Box<dyn Fn() -> Config + Send + Sync>;

/// Memory history source: samples from the last `Duration`. The real server reads the CSV
/// files in the data folder, like `memguard history --json`; tests pass fixed samples.
pub type HistoryLoader = Box<dyn Fn(Duration) -> Vec<Sample> + Send + Sync>;

/// How disk counting runs: the low-priority child process, or in-process in tests.
pub type ScanRunner = Box<dyn Fn(&crate::disk_scan::ScanJob, crate::disk_scan::Progress) -> Result<crate::disk_scan::ScanOutput, String> + Send + Sync>;

/// The latest disk checkup, for `memguard_disk_plan`.
pub type CheckupLoader = Box<dyn Fn() -> Option<crate::disk_scan::CachedCheckup> + Send + Sync>;

type LayerResolver = Box<dyn Fn(&str) -> crate::config::Layer + Send + Sync>;
type TeamActiveFn = Box<dyn Fn() -> bool + Send + Sync>;
type ConfigWarningFn = Box<dyn Fn() -> Option<String> + Send + Sync>;

type FileStamp = Option<(Option<SystemTime>, u64)>;

fn file_stamp(path: &std::path::Path) -> FileStamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

struct CachedConfig {
    cfg_path: PathBuf,
    team_path: PathBuf,
    cfg_stamp: FileStamp,
    team_stamp: FileStamp,
    cfg: Config,
}

fn cached_config_loader_for(
    cfg_path_fn: impl Fn() -> PathBuf + Send + Sync + 'static,
    team_path_fn: impl Fn() -> PathBuf + Send + Sync + 'static,
    loader: impl Fn() -> Config + Send + Sync + 'static,
) -> ConfigLoader {
    let cache: Mutex<Option<CachedConfig>> = Mutex::new(None);
    Box::new(move || {
        let cfg_path = cfg_path_fn();
        let team_path = team_path_fn();
        let cfg_stamp = file_stamp(&cfg_path);
        let team_stamp = file_stamp(&team_path);
        if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if hit.cfg_path == cfg_path
                && hit.team_path == team_path
                && hit.cfg_stamp == cfg_stamp
                && hit.team_stamp == team_stamp
            {
                return hit.cfg.clone();
            }
        }
        let cfg = loader();
        // Re-stat after loading: Config::load_or_default() may create settings.toml on first run.
        let new_cfg_stamp = file_stamp(&cfg_path);
        let new_team_stamp = file_stamp(&team_path);
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedConfig {
            cfg_path,
            team_path,
            cfg_stamp: new_cfg_stamp,
            team_stamp: new_team_stamp,
            cfg: cfg.clone(),
        });
        cfg
    })
}

struct CancelToken {
    cancelled: AtomicBool,
    mu: Mutex<()>,
    cv: Condvar,
}

impl CancelToken {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            mu: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify();
    }

    fn notify(&self) {
        let _g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Wait up to `dur`, returning `true` immediately if cancelled.
    fn wait_timeout(&self, dur: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        if dur.is_zero() {
            return self.is_cancelled();
        }
        let guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        let (_guard, _) = self
            .cv
            .wait_timeout_while(guard, dur, |_| !self.is_cancelled())
            .unwrap_or_else(|e| e.into_inner());
        self.is_cancelled()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RequestId {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Str(String),
    Other(String),
}

impl RequestId {
    fn from_value(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Self::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Self::Uint(u)
                } else {
                    Self::Other(n.to_string())
                }
            }
            Value::String(s) => Self::Str(s.clone()),
            other => Self::Other(other.to_string()),
        }
    }
}

struct ToolCallWork {
    id: Value,
    req_id: RequestId,
    name: String,
    args: Map<String, Value>,
    progress_token: Option<Value>,
    cancel: Arc<CancelToken>,
}

pub struct Server {
    load_config: ConfigLoader,
    resolve_layer: LayerResolver,
    team_active: TeamActiveFn,
    config_warning: ConfigWarningFn,
    load_history: HistoryLoader,
    custom_scan: Option<ScanRunner>,
    load_checkup: CheckupLoader,
    /// Where plans, the undo log and the measured watches are kept.
    disk_files: crate::disk_clean::DiskFiles,
    /// How `memguard_disk_clean` acts (tests: no waiting, no watcher, fixed Unreal facts).
    clean_opts: crate::disk_clean::CleanOpts,
    /// The plans `memguard_disk_plan` made in this server, newest last. `memguard_disk_clean`
    /// acts only on these: a plan file on disk is something an agent could edit.
    agent_plans: Mutex<Vec<crate::disk_clean::PlanRecord>>,
}

impl Default for Server {
    fn default() -> Self {
        let last_warning: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let last_warning_loader = Arc::clone(&last_warning);
        let mut srv = Server::new(cached_config_loader_for(
            paths::config_file,
            crate::config::team_file,
            move || {
                let (cfg, err) = Config::load_or_default();
                if let Some(e) = &err {
                    if crate::config::has_valid_team_file() {
                        eprintln!("memguard mcp: settings file has a problem, using team/built-in defaults: {}", e);
                    } else {
                        eprintln!("memguard mcp: settings file has a problem, using built-in defaults: {}", e);
                    }
                }
                *last_warning_loader.lock().unwrap_or_else(|e| e.into_inner()) = err;
                cfg
            },
        ));
        srv.resolve_layer = Box::new(|p| {
            if paths::config_file().is_file() || crate::config::team_file().is_file() {
                crate::config::value_layer(p)
            } else {
                crate::config::Layer::Builtin
            }
        });
        srv.team_active = Box::new(crate::config::has_valid_team_file);
        srv.config_warning = Box::new(move || last_warning.lock().unwrap_or_else(|e| e.into_inner()).clone());
        srv
    }
}

/// A tool failure: reported to the agent as a result with `isError: true`.
struct ToolError(String);

impl Server {
    pub fn new(load_config: ConfigLoader) -> Self {
        Server {
            load_config,
            resolve_layer: Box::new(|_| crate::config::Layer::Builtin),
            team_active: Box::new(|| false),
            config_warning: Box::new(|| None),
            load_history: Box::new(crate::history::load_since),
            custom_scan: None,
            load_checkup: Box::new(crate::disk_scan::load_cache),
            disk_files: crate::disk_clean::DiskFiles::default(),
            clean_opts: crate::disk_clean::CleanOpts { agent: true, presample_secs: 5, start_watcher: true, ..Default::default() },
            agent_plans: Mutex::new(Vec::new()),
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
        self.custom_scan = Some(run_scan);
        self.load_checkup = load_checkup;
        self
    }

    /// Use another history source (tests).
    pub fn with_history(mut self, load_history: HistoryLoader) -> Self {
        self.load_history = load_history;
        self
    }

    /// Read messages until the reader ends, draining queued tool calls on EOF.
    pub fn serve<R: BufRead + Send, W: Write + Send>(&self, reader: R, writer: W) -> io::Result<()> {
        self.serve_inner(reader, writer, false)
    }

    fn serve_inner<R: BufRead + Send, W: Write + Send>(&self, reader: R, writer: W, cancel_on_eof: bool) -> io::Result<()> {
        let out = Mutex::new(writer);
        let in_flight: Mutex<HashMap<RequestId, Vec<Arc<CancelToken>>>> = Mutex::new(HashMap::new());
        let (tx, rx) = mpsc::channel::<ToolCallWork>();

        std::thread::scope(|scope| {
            let reader_handle = scope.spawn(|| -> io::Result<()> {
                let mut reader = reader;
                let mut buf = String::new();
                loop {
                    buf.clear();
                    let n = match reader.read_line(&mut buf) {
                        Ok(n) => n,
                        Err(e) => {
                            if cancel_on_eof {
                                cancel_all(&in_flight);
                            }
                            drop(tx);
                            return Err(e);
                        }
                    };
                    if n == 0 {
                        if cancel_on_eof {
                            cancel_all(&in_flight);
                        }
                        drop(tx);
                        return Ok(());
                    }
                    let line = buf.trim_end_matches(['\r', '\n']);
                    if line.trim().trim_start_matches('\u{feff}').trim().is_empty() {
                        continue;
                    }
                    match parse_incoming_line(line) {
                        Incoming::Reply(reply) => {
                            if let Err(e) = write_sync(&out, &reply) {
                                if cancel_on_eof {
                                    cancel_all(&in_flight);
                                }
                                drop(tx);
                                return Err(e);
                            }
                        }
                        Incoming::Ignore => {}
                        Incoming::Cancel(req_id) => {
                            let toks = in_flight
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&req_id)
                                .cloned()
                                .unwrap_or_default();
                            for t in toks {
                                t.cancel();
                            }
                        }
                        Incoming::Immediate { id, method, params } => {
                            let reply = match method.as_str() {
                                "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": initialize(&params) }),
                                "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                                "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tool_list() } }),
                                _ => error_reply(id, METHOD_NOT_FOUND, &format!("Method not found: {}", method)),
                            };
                            if let Err(e) = write_sync(&out, &reply) {
                                if cancel_on_eof {
                                    cancel_all(&in_flight);
                                }
                                drop(tx);
                                return Err(e);
                            }
                        }
                        Incoming::ToolCall { id, params } => match validate_tool_call(&params) {
                            Err((code, message)) => {
                                let reply = error_reply(id, code, &message);
                                if let Err(e) = write_sync(&out, &reply) {
                                    if cancel_on_eof {
                                        cancel_all(&in_flight);
                                    }
                                    drop(tx);
                                    return Err(e);
                                }
                            }
                            Ok((name, args, progress_token)) => {
                                let req_id = RequestId::from_value(&id);
                                let tok = Arc::new(CancelToken::new());
                                {
                                    let mut map = in_flight.lock().unwrap_or_else(|e| e.into_inner());
                                    map.entry(req_id.clone()).or_default().push(tok.clone());
                                }
                                let work = ToolCallWork {
                                    id,
                                    req_id: req_id.clone(),
                                    name,
                                    args,
                                    progress_token,
                                    cancel: tok.clone(),
                                };
                                if tx.send(work).is_err() {
                                    remove_if_same(&in_flight, &req_id, &tok);
                                    break Ok(());
                                }
                            }
                        },
                    }
                }
            });

            let mut worker_err: Option<io::Error> = None;
            while let Ok(work) = rx.recv() {
                let outcome = if work.cancel.is_cancelled() {
                    Err(ToolError("Cancelled.".into()))
                } else {
                    self.run_tool(&work.name, &work.args, work.progress_token.clone(), &out, &work.cancel)
                };
                remove_if_same(&in_flight, &work.req_id, &work.cancel);
                let reply = format_tool_reply(work.id, outcome);
                if let Err(e) = write_sync(&out, &reply) {
                    if worker_err.is_none() {
                        worker_err = Some(e);
                    }
                    break;
                }
            }
            drop(rx);

            let reader_res = match reader_handle.join() {
                Ok(r) => r,
                Err(_) => Err(io::Error::other("MCP reader thread exited unexpectedly")),
            };
            reader_res?;
            if let Some(e) = worker_err {
                return Err(e);
            }
            Ok(())
        })
    }

    /// Handle one incoming line. Returns the reply, or `None` for notifications and for
    /// responses the client sends us. Progress notifications go straight to `out`.
    pub fn handle_line<W: Write + Send>(&self, line: &str, out: &mut W) -> io::Result<Option<Value>> {
        let cancel = CancelToken::new();
        let sync_out = Mutex::new(out);
        match parse_incoming_line(line) {
            Incoming::Reply(r) => Ok(Some(r)),
            Incoming::Ignore | Incoming::Cancel(_) => Ok(None),
            Incoming::Immediate { id, method, params } => {
                let reply = match method.as_str() {
                    "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": initialize(&params) }),
                    "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                    "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tool_list() } }),
                    _ => error_reply(id, METHOD_NOT_FOUND, &format!("Method not found: {}", method)),
                };
                Ok(Some(reply))
            }
            Incoming::ToolCall { id, params } => match validate_tool_call(&params) {
                Err((code, message)) => Ok(Some(error_reply(id, code, &message))),
                Ok((name, args, progress_token)) => {
                    let outcome = self.run_tool(&name, &args, progress_token, &sync_out, &cancel);
                    Ok(Some(format_tool_reply(id, outcome)))
                }
            },
        }
    }

    fn run_tool<W: Write + Send>(
        &self,
        name: &str,
        args: &Map<String, Value>,
        progress_token: Option<Value>,
        out: &Mutex<W>,
        cancel: &CancelToken,
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
                if cancel.is_cancelled() {
                    return Err(ToolError("Cancelled.".into()));
                }
                if prime {
                    ops::prime(&cfg, None, false);
                }
                let total = timeout.as_secs_f64();
                let started = Instant::now();
                let mut result = loop {
                    if cancel.is_cancelled() {
                        return Err(ToolError("Cancelled.".into()));
                    }
                    let sys = crate::engine::SystemStatus::from(&win::sys_info());
                    let ready = sys.ram_free_gb >= need.ram_gb && sys.commit_free_gb >= need.commit_gb;
                    let elapsed = started.elapsed();
                    if ready || elapsed >= timeout {
                        break ops::WaitResult {
                            ready,
                            waited_sec: elapsed.as_secs(),
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
                    if let Some(token) = &progress_token {
                        let note = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": elapsed.as_secs_f64().min(total),
                                "total": total,
                                "message": format!(
                                    "Waiting: RAM free {:.1} of {:.1} GB, commit free {:.1} of {:.1} GB",
                                    sys.ram_free_gb, need.ram_gb, sys.commit_free_gb, need.commit_gb
                                ),
                            }
                        });
                        if let Err(e) = write_sync(out, &note) {
                            eprintln!("memguard mcp: could not send progress: {}", e);
                        }
                    }
                    if cancel.wait_timeout(Duration::from_secs(2)) {
                        return Err(ToolError("Cancelled.".into()));
                    }
                };
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
                if cancel.is_cancelled() {
                    return Err(ToolError("Cancelled.".into()));
                }
                let cfg = (self.load_config)();
                let mut note = progress_writer(&progress_token, out, budget as f64, "Counting");
                let runner = |job: &crate::disk_scan::ScanJob, progress: crate::disk_scan::Progress| -> Result<crate::disk_scan::ScanOutput, String> {
                    if cancel.is_cancelled() {
                        return Err("Cancelled.".into());
                    }
                    let res = match &self.custom_scan {
                        Some(custom) => custom(job, progress),
                        None => run_child_cancellable(job, progress, cancel),
                    };
                    if cancel.is_cancelled() {
                        return Err("Cancelled.".into());
                    }
                    res
                };
                let checkup = crate::disk_scan::checkup(&cfg, path.as_deref(), refresh, Some(budget), &runner, &mut note)
                    .map_err(ToolError)?;
                if cancel.is_cancelled() {
                    return Err(ToolError("Cancelled.".into()));
                }
                to_value(&checkup)
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
                let runner = |job: &crate::disk_scan::ScanJob, progress: crate::disk_scan::Progress| -> Result<crate::disk_scan::ScanOutput, String> {
                    if cancel.is_cancelled() {
                        return Err("Cancelled.".into());
                    }
                    match &self.custom_scan {
                        Some(custom) => custom(job, progress),
                        None => run_child_cancellable(job, progress, cancel),
                    }
                };
                let (v, rec) = crate::disk_scan::plan_record(&cfg, &cache, &ids, None, &runner, &mut note).map_err(ToolError)?;
                if let Err(e) = self.disk_files.save_plan(&rec) {
                    eprintln!("memguard mcp: could not save the plan: {}", e);
                }
                let mut mine = self.agent_plans.lock().unwrap_or_else(|e| e.into_inner());
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
                let plans = self.agent_plans.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
            // ---- M3b read-only tools
            "memguard_drivers" => {
                check_keys(args, &[])?;
                to_value(&crate::drivers::report())
            }
            "memguard_disk_relocate" => {
                check_keys(args, &["to"])?;
                let to = str_arg(args, "to")?;
                if let Some(why) = to.as_deref().and_then(crate::disk_relocate::to_path_problem) {
                    return Err(ToolError(why));
                }
                let to_vol = match to.as_deref() {
                    Some(t) => {
                        let existing = std::path::Path::new(t)
                            .ancestors()
                            .find(|a| a.exists())
                            .map(|a| a.display().to_string())
                            .and_then(|e| crate::disk_win::volume_of(&e).map(|(_, g)| g));
                        Some(existing.ok_or_else(|| ToolError(format!("{} isn't on a drive this PC has.", t)))?)
                    }
                    None => None,
                };
                if cancel.is_cancelled() {
                    return Err(ToolError("Cancelled.".into()));
                }
                let store = crate::disk_relocate::RegistryEnv::from_env().map_err(ToolError)?;
                let cfg = (self.load_config)();
                let vars = relocate_read_vars_cancellable(&store, cancel)?;
                let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
                let sys_root = if sys.ends_with('\\') { sys } else { format!("{}\\", sys) };
                let system = crate::disk_win::volume_of(&sys_root).map(|(_, g)| g);
                let vols = crate::disk_relocate::dest_volumes();
                let plan = crate::disk_relocate::plan(
                    &vars,
                    system.as_deref(),
                    &vols,
                    to.as_deref().zip(to_vol.as_deref()),
                    &cfg.disk(),
                );
                Ok(crate::disk_relocate::plan_json(&plan, &crate::disk_relocate::EnvStore::describe(&store)))
            }
            "memguard_disk_bin" => {
                check_keys(args, &["volume"])?;
                let volume = match args.get("volume") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
                    Some(_) => return Err(ToolError("volume must be a drive letter string like \"D:\"".into())),
                };
                let mut roots = crate::disk_clean::fixed_roots();
                if let Some(v) = &volume {
                    let drive_byte = match v.as_bytes() {
                        [c] if c.is_ascii_alphabetic() => *c,
                        [c, b':'] if c.is_ascii_alphabetic() => *c,
                        [c, b':', b'\\' | b'/'] if c.is_ascii_alphabetic() => *c,
                        _ => return Err(ToolError(format!("volume \"{}\" must be a drive letter like \"D:\".", v))),
                    };
                    let want = (drive_byte as char).to_ascii_uppercase().to_string();
                    roots.retain(|r| r.trim_end_matches(['\\', '/']).trim_end_matches(':').eq_ignore_ascii_case(&want));
                }
                let cfg = (self.load_config)();
                let hold_hours = cfg.disk().hold_hours;
                let runs = crate::disk_clean::bin_runs(&self.disk_files, &roots);
                let expired_run_ids: Vec<String> = crate::disk_clean::expired(&runs, hold_hours, crate::disk_clean::unix_now())
                    .into_iter()
                    .map(|r| r.run_id)
                    .collect();
                let total_holds_bytes: u64 = runs.iter().map(|r| r.holds).sum();
                let total_frees_alone_bytes: u64 = runs.iter().map(|r| r.frees_alone).sum();
                Ok(json!({
                    "runs": runs,
                    "hold_hours": hold_hours,
                    "expired_run_ids": expired_run_ids,
                    "total_holds_gb": crate::disk_space::gb2(total_holds_bytes),
                    "total_frees_alone_gb": crate::disk_space::gb2(total_frees_alone_bytes),
                }))
            }
            "memguard_config" => {
                check_keys(args, &["path"])?;
                let path = match args.get("path") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
                    Some(_) => return Err(ToolError("path must be a dotted setting path string, like \"general.preset\".".into())),
                };
                let mut cfg = (self.load_config)();
                if cfg.disk.is_none() {
                    cfg.disk = Some(cfg.disk());
                }
                if let Some(p) = path {
                    let segs: Vec<&str> = p.split('.').map(str::trim).collect();
                    if segs.iter().any(|s| s.is_empty()) {
                        return Err(ToolError(format!(
                            "\"{}\" is not a settings path (like general.preset or group.Browsers.trim_above_pct).",
                            p
                        )));
                    }
                    if segs[0] == "group" && segs.len() < 2 {
                        return Err(ToolError("say which group: group.<name>.<key>".into()));
                    }
                    let cfg_val = to_value(&cfg)?;
                    let found_val = lookup_config_value(&cfg_val, &segs).cloned();
                    let layer = (self.resolve_layer)(&p);
                    Ok(json!({
                        "path": p,
                        "found": found_val.is_some(),
                        "value": found_val,
                        "layer": layer,
                    }))
                } else {
                    let settings_file = paths::config_file();
                    let team_file = crate::config::team_file();
                    let mut out = json!({
                        "settings_file": settings_file.display().to_string(),
                        "team_file": team_file.display().to_string(),
                        "team_active": (self.team_active)(),
                        "config": cfg,
                    });
                    if let Some(warn) = (self.config_warning)() {
                        out["warning"] = Value::String(warn);
                    }
                    Ok(out)
                }
            }
            "memguard_session_summary" => {
                check_keys(args, &["hours"])?;
                let hours = hours_arg(args)?;
                let samples = (self.load_history)(Duration::from_secs_f64(hours * 3600.0));
                to_value(&crate::history::summarize_samples(&samples))
            }
            _ => Err(ToolError(format!("Unknown tool: {}", name))),
        }
    }
}

/// Serve on this process's stdin and stdout until stdin closes.
pub fn run_stdio() -> io::Result<()> {
    eprintln!("Ather MemGuard {} MCP server: reading JSON-RPC from stdin", env!("CARGO_PKG_VERSION"));
    Server::default().serve_inner(io::BufReader::new(io::stdin()), io::stdout(), true)
}

/// Status as `memguard status --json` reports it, including whether the background app runs.
pub fn current_status(cfg: &Config) -> Status {
    let mut st = analyze(cfg, &snapshot());
    st.daemon = win::instance_running(paths::mutex());
    st.disk = crate::disk::status_brief(cfg);
    // Paused / suspended state only the background app knows: take it from its live channel.
    if st.daemon {
        crate::ipc::apply_live_status(&mut st);
    }
    st
}

// ---------------------------------------------------------------- cancellation & concurrency helpers

enum Incoming {
    Reply(Value),
    Ignore,
    Cancel(RequestId),
    Immediate { id: Value, method: String, params: Value },
    ToolCall { id: Value, params: Value },
}

fn parse_incoming_line(line: &str) -> Incoming {
    // Some shells put a byte-order mark before the first line they pipe.
    let line = line.trim_start_matches('\u{feff}');
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Incoming::Reply(error_reply(Value::Null, PARSE_ERROR, &format!("Parse error: {}", e))),
    };
    let Some(obj) = msg.as_object() else {
        // Batches were removed from MCP in 2025-06-18.
        return Incoming::Reply(error_reply(Value::Null, INVALID_REQUEST, "Invalid request: expected one JSON object"));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) {
            return Incoming::Ignore; // a response to something we sent; we send no requests
        }
        return Incoming::Reply(error_reply(id.unwrap_or(Value::Null), INVALID_REQUEST, "Invalid request: no method"));
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    let Some(id) = id else {
        if method == "notifications/cancelled" {
            if let Some(req_id) = params.get("requestId") {
                return Incoming::Cancel(RequestId::from_value(req_id));
            }
        }
        return Incoming::Ignore;
    };

    if method == "tools/call" {
        Incoming::ToolCall { id, params }
    } else {
        Incoming::Immediate { id, method: method.to_string(), params }
    }
}

fn validate_tool_call(params: &Value) -> Result<(String, Map<String, Value>, Option<Value>), (i64, String)> {
    let bad = |m: &str| (INVALID_PARAMS, m.to_string());
    let params = params.as_object().ok_or_else(|| bad("tools/call needs params with a tool name"))?;
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| bad("tools/call needs a tool name"))?;
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(bad("arguments must be an object")),
    };
    if !is_known_tool(name) {
        return Err((INVALID_PARAMS, format!("Unknown tool: {}", name)));
    }
    let progress_token = params.get("_meta").and_then(|m| m.get("progressToken")).cloned();
    Ok((name.to_string(), args, progress_token))
}

fn format_tool_reply(id: Value, outcome: Result<Value, ToolError>) -> Value {
    let result = match outcome {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": pretty(&value) }],
            "structuredContent": value,
            "isError": false,
        }),
        Err(ToolError(message)) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn cancel_all(in_flight: &Mutex<HashMap<RequestId, Vec<Arc<CancelToken>>>>) {
    let tokens: Vec<Arc<CancelToken>> = in_flight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .flat_map(|v| v.iter().cloned())
        .collect();
    for tok in tokens {
        tok.cancel();
    }
}

fn remove_if_same(in_flight: &Mutex<HashMap<RequestId, Vec<Arc<CancelToken>>>>, req_id: &RequestId, tok: &Arc<CancelToken>) {
    let mut map = in_flight.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(vec) = map.get_mut(req_id) {
        vec.retain(|cur| !Arc::ptr_eq(cur, tok));
        if vec.is_empty() {
            map.remove(req_id);
        }
    }
}

fn run_child_cancellable(
    job: &crate::disk_scan::ScanJob,
    progress: crate::disk_scan::Progress,
    cancel: &CancelToken,
) -> Result<crate::disk_scan::ScanOutput, String> {
    let exe = crate::disk_scan::cli_exe().ok_or("memguard.exe was not found, so the disk could not be counted")?;
    let input = serde_json::to_vec(job).map_err(|e| e.to_string())?;
    run_child_cancellable_cmd(&exe, &["disk", "scan-child"], Some(&input), progress, cancel)
}

fn run_child_cancellable_cmd(
    exe: &std::path::Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    progress: crate::disk_scan::Progress,
    cancel: &CancelToken,
) -> Result<crate::disk_scan::ScanOutput, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if cancel.is_cancelled() {
        return Err("Cancelled.".into());
    }
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start the disk counter: {}", e))?;
    if let Some(input) = stdin_bytes {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(input) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        } else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no stdin".into());
        }
    } else {
        drop(child.stdin.take());
    }
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no stdout".into());
        }
    };
    let stderr = match child.stderr.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no stderr".into());
        }
    };

    let (ptx, prx) = mpsc::channel::<(u64, f64)>();
    std::thread::scope(|scope| {
        let stdout_handle = scope.spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            s
        });
        let stderr_handle = scope.spawn(move || {
            for line in io::BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut it = line.split_whitespace();
                if it.next() == Some("progress") {
                    if let (Some(f), Some(s)) = (it.next().and_then(|v| v.parse().ok()), it.next().and_then(|v| v.parse().ok())) {
                        if ptx.send((f, s)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut cancelled = false;
        loop {
            if cancel.is_cancelled() {
                cancelled = true;
                let _ = child.kill();
                break;
            }
            match prx.recv_timeout(Duration::from_millis(50)) {
                Ok((f, s)) => progress(f, s),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(prx);
        let status = child.wait().map_err(|e| e.to_string())?;
        let text = stdout_handle.join().map_err(|_| "the disk counter's output was lost".to_string())?;
        let _ = stderr_handle.join();
        if cancelled || cancel.is_cancelled() {
            return Err("Cancelled.".into());
        }
        if !status.success() {
            return Err(format!("the disk counter stopped with exit code {}", status.code().unwrap_or(-1)));
        }
        serde_json::from_str(text.trim()).map_err(|e| format!("the disk counter's result could not be read: {}", e))
    })
}

fn relocate_read_vars_cancellable(
    store: &dyn crate::disk_relocate::EnvStore,
    cancel: &CancelToken,
) -> Result<Vec<crate::disk_relocate::VarState>, ToolError> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let stop = Arc::new(AtomicBool::new(cancel.is_cancelled()));
    let done = AtomicBool::new(false);
    let vars = std::thread::scope(|s| {
        let stop_bg = stop.clone();
        let done_ref = &done;
        s.spawn(move || {
            let guard = cancel.mu.lock().unwrap_or_else(|e| e.into_inner());
            let _guard = cancel
                .cv
                .wait_while(guard, |_| !done_ref.load(Ordering::SeqCst) && !cancel.is_cancelled())
                .unwrap_or_else(|e| e.into_inner());
            if cancel.is_cancelled() {
                stop_bg.store(true, Ordering::SeqCst);
            }
        });
        let sizer = |path: &str| -> Option<(u64, bool)> {
            if stop.load(Ordering::SeqCst) || cancel.is_cancelled() {
                return None;
            }
            let vs = crate::disk_win::VolumeScan {
                cluster: 4096,
                block_cloning: false,
                level2: false,
                threads: 8,
                deadline,
                stop: stop.clone(),
                started_ft: u64::MAX,
                multi: Mutex::new(HashMap::new()),
                progress_files: Arc::new(AtomicU64::new(0)),
            };
            let (st, _) = crate::disk_win::walk_item(&vs, 0, &[path.to_string()], &[]);
            Some((st.allocated, !st.cut_short && st.dir_failures == 0))
        };
        let res = crate::disk_relocate::read_vars(store, &sizer);
        done.store(true, Ordering::SeqCst);
        cancel.notify();
        res
    });
    if cancel.is_cancelled() {
        return Err(ToolError("Cancelled.".into()));
    }
    Ok(vars)
}

fn lookup_config_value<'a>(root: &'a Value, segs: &[&str]) -> Option<&'a Value> {
    let (mut cur, rest) = if segs.first().copied() == Some("group") {
        let name = *segs.get(1)?;
        let grp = root
            .get("group")?
            .as_array()?
            .iter()
            .find(|g| g.get("name").and_then(Value::as_str) == Some(name))?;
        (grp, &segs[2..])
    } else {
        (root, segs)
    };
    for key in rest {
        cur = cur.get(*key)?;
    }
    Some(cur)
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

fn write_sync<W: Write>(out: &Mutex<W>, msg: &Value) -> io::Result<()> {
    let mut guard = out.lock().unwrap_or_else(|e| e.into_inner());
    write_message(&mut *guard, msg)
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
fn progress_writer<'a, W: Write>(token: &'a Option<Value>, out: &'a Mutex<W>, total: f64, what: &'a str) -> impl FnMut(u64, f64) + 'a {
    move |files, secs| {
        if let Some(token) = token {
            let note = json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": { "progressToken": token, "progress": secs.min(total), "total": total, "message": format!("{}: {} files", what, files) }
            });
            if let Err(e) = write_sync(out, &note) {
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

fn tool_list() -> &'static [Value] {
    static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
    TOOLS.get_or_init(build_tool_list).as_slice()
}

fn is_known_tool(name: &str) -> bool {
    static NAMES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    NAMES
        .get_or_init(|| tool_list().iter().filter_map(|t| t.get("name").and_then(Value::as_str)).collect())
        .contains(name)
}

fn build_tool_list() -> Vec<Value> {
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
        json!({
            "name": "memguard_drivers",
            "title": "Windows and drivers",
            "description": "Read-only breakdown of the RAM used by Windows itself and its drivers: nonpaged pool (memory Windows cannot page out), resident paged pool, Windows kernel code, driver code, top Windows processes, and top kernel pool tags mapped to likely loaded .sys driver files (tags_available is false when not running as administrator). Changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_disk_relocate",
            "title": "Cache relocation plan",
            "description": "Read-only plan for moving developer caches (UE-LocalDataCachePath, UE-ZenSubprocessDataPath, NuGet, npm, pip, uv, pnpm, Yarn, Bun, vcpkg, sccache) off the system drive onto another fixed SSD by setting user environment variables. Returns the destination drive, per-variable sizes and actions, notes (including variables never moved like CARGO_HOME and TEMP), and any refusal reason. Changes no registry values and copies or deletes nothing; the person applies or undoes via `memguard disk relocate --apply` or `--undo`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Optional destination folder on a fixed non-system drive (e.g. E:\\DevCaches). When omitted, picks the fixed non-rotating non-system drive with the most free space."
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_disk_bin",
            "title": "Ather MemGuard bin",
            "description": "Read-only listing of runs currently held in Ather MemGuard's `.AtherMemGuard-bin` folders across fixed drives: volume root, run_id, moved timestamp, item titles, predicted frees_alone bytes when emptied, logical holds bytes, configured hold_hours, expired_run_ids, and totals. Deletes nothing; use `memguard disk bin --empty` or `memguard disk undo` on the CLI.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "volume": {
                        "type": "string",
                        "description": "Optional drive letter to filter by, e.g. \"D:\". When omitted, checks all fixed drives."
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_config",
            "title": "Effective configuration",
            "description": "Read-only view of Ather MemGuard's effective layered settings (built-in defaults merged with team.toml and settings.toml): general preset and flags, pressure thresholds and hysteresis, Unreal protect/worker lists, prime targets, doctor rules, disk rules, and all app groups. Optionally pass `path` (e.g. \"general.preset\" or \"group.Browsers.trim_above_pct\") to inspect a single setting and its layer. Changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional dotted setting path to look up, e.g. \"general.preset\", \"disk.min_age_hours\", or \"group.Browsers.trim_above_pct\"."
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "memguard_session_summary",
            "title": "Memory session summary",
            "description": "Read-only compact summary of recorded memory history over the requested hours (default 1, max 168): sample count, time span, peak and latest Unreal/editor committed GB, minimum and latest free RAM and commit GB, peak compile workers, minutes spent in normal/elevated/critical pressure, pressure transitions, editor growth rate, and per-group peak and latest RAM GB without dumping raw samples.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hours": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "maximum": HISTORY_MAX_HOURS,
                        "default": HISTORY_DEFAULT_HOURS,
                        "description": "How far back to summarize, in hours. Default 1, at most 168 (a week)."
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
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
    line("       memguard_drivers, memguard_config, memguard_session_summary,");
    line("       memguard_disk, memguard_disk_checkup, memguard_disk_plan, memguard_disk_relocate, memguard_disk_bin,");
    line("       memguard_disk_clean (deletes regenerable build output only).");
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
            [
                "memguard_status",
                "memguard_prime",
                "memguard_wait",
                "memguard_advise",
                "memguard_doctor",
                "memguard_history",
                "memguard_ram",
                "memguard_disk",
                "memguard_disk_checkup",
                "memguard_disk_plan",
                "memguard_disk_clean",
                "memguard_drivers",
                "memguard_disk_relocate",
                "memguard_disk_bin",
                "memguard_config",
                "memguard_session_summary",
            ]
        );
        for t in tools.iter().filter(|t| t["name"].as_str().unwrap().starts_with("memguard_disk")) {
            let deletes = t["name"] == "memguard_disk_clean";
            assert_eq!(t["annotations"]["readOnlyHint"], !deletes, "{}", t["name"]);
            assert_eq!(t["annotations"]["destructiveHint"] == true, deletes, "{}", t["name"]);
        }
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", t["name"]);
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
        std::fs::write(root.join(r"Game\Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();
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
        std::fs::write(game.join("Game.uproject"), br#"{ "FileVersion": 3, "EngineAssociation": "5.4" }"#).unwrap();
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
        let cached = crate::disk_scan::CachedCheckup { id: "c1".into(), scope: "t".into(), unix: 0, time: String::new(), items: items.clone(), root_ids: vec![], last_writes: vec![], report: rep };
        let files = DiskFiles::in_dir(root.join("data"));
        let srv = server()
            .with_disk(Box::new(|job, p| Ok(crate::disk_scan::run_job(job, p))), Box::new(move || Some(cached.clone())))
            .with_clean(files.clone(), CleanOpts { presample_secs: 1, start_watcher: false, busy: Some((vec![], None)), ..Default::default() });
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
            let mut mine = srv.agent_plans.lock().unwrap();
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
        for p in srv.agent_plans.lock().unwrap().iter_mut() {
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
        let mut forged: PlanRecord = srv.agent_plans.lock().unwrap().last().unwrap().clone();
        forged.id = "pforged".into();
        forged.unix = crate::disk_clean::unix_now();
        forged.items[0].paths = vec![game.join("Content").display().to_string()];
        forged.items[0].path_ids = vec![crate::disk_win::identity(&game.join("Content").display().to_string()).map(|x| x.1)];
        forged.items[0].last_write = u64::MAX;
        srv.agent_plans.lock().unwrap().push(forged);
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

    // ---- Streaming reader/writer harness for concurrency & cancellation tests

    struct ChanReader {
        rx: mpsc::Receiver<Vec<u8>>,
        cur: Vec<u8>,
        pos: usize,
    }

    impl io::Read for ChanReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            while self.pos >= self.cur.len() {
                match self.rx.recv() {
                    Ok(chunk) => {
                        self.cur = chunk;
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let n = (self.cur.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    struct ChanWriter {
        tx: mpsc::Sender<Value>,
        buf: Vec<u8>,
    }

    impl io::Write for ChanWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(data);
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let s = String::from_utf8(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if !s.trim().is_empty() {
                    let v: Value = serde_json::from_str(s.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let _ = self.tx.send(v);
                }
            }
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn spawn_stream_server(srv: Arc<Server>, cancel_on_eof: bool) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Value>, std::thread::JoinHandle<io::Result<()>>) {
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        let (out_tx, out_rx) = mpsc::channel::<Value>();
        let reader = io::BufReader::new(ChanReader { rx: in_rx, cur: Vec::new(), pos: 0 });
        let writer = ChanWriter { tx: out_tx, buf: Vec::new() };
        let handle = std::thread::spawn(move || srv.serve_inner(reader, writer, cancel_on_eof));
        (in_tx, out_rx, handle)
    }

    fn send_json_line(tx: &mpsc::Sender<Vec<u8>>, msg: &Value) {
        let mut bytes = serde_json::to_vec(msg).unwrap();
        bytes.push(b'\n');
        tx.send(bytes).unwrap();
    }

    fn recv_until_id(rx: &mpsc::Receiver<Value>, id: &Value, timeout: Duration) -> (Vec<Value>, Value) {
        let deadline = Instant::now() + timeout;
        let mut notes = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for id {}", id);
            let msg = rx.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for id {}", id));
            if msg.get("id") == Some(id) {
                return (notes, msg);
            }
            notes.push(msg);
        }
    }

    #[test]
    fn ping_does_not_block_while_wait_is_running() {
        let srv = Arc::new(server());
        let (in_tx, out_rx, handle) = spawn_stream_server(srv, true);

        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "memguard_wait",
                "arguments": { "ram_gb": 1.0e9, "timeout_sec": 15 },
                "_meta": { "progressToken": "wp" }
            }
        }));
        let first_note = out_rx.recv_timeout(Duration::from_secs(2)).expect("first progress note");
        assert_eq!(first_note["method"], "notifications/progress");

        let t0 = Instant::now();
        send_json_line(&in_tx, &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
        let (_notes, ping_reply) = recv_until_id(&out_rx, &json!(2), Duration::from_secs(1));
        assert!(t0.elapsed() < Duration::from_millis(500), "ping blocked: {:?}", t0.elapsed());
        assert_eq!(ping_reply["result"], json!({}));

        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 1 }
        }));
        let (_notes, cancel_reply) = recv_until_id(&out_rx, &json!(1), Duration::from_secs(1));
        assert_eq!(cancel_reply["result"]["isError"], true);
        drop(in_tx);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn cancelled_aborts_wait_with_int_and_string_ids_and_ignores_unknown() {
        let srv = Arc::new(server());
        let (in_tx, out_rx, handle) = spawn_stream_server(srv, true);

        // 1) Active wait with integer ID cancelled after progress starts.
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {
                "name": "memguard_wait",
                "arguments": { "ram_gb": 1.0e9, "timeout_sec": 20 },
                "_meta": { "progressToken": "p42" }
            }
        }));
        let _ = out_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let t0 = Instant::now();
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 42 }
        }));
        let (_notes, r42) = recv_until_id(&out_rx, &json!(42), Duration::from_secs(1));
        assert!(t0.elapsed() < Duration::from_millis(500));
        assert_eq!(r42["result"]["isError"], true);

        // 2) Back-to-back tools/call + notifications/cancelled with string ID.
        let t1 = Instant::now();
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "id": "str-99",
            "method": "tools/call",
            "params": {
                "name": "memguard_wait",
                "arguments": { "ram_gb": 1.0e9, "timeout_sec": 20 }
            }
        }));
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": "str-99" }
        }));
        let (notes, r_str) = recv_until_id(&out_rx, &json!("str-99"), Duration::from_secs(1));
        assert!(notes.is_empty());
        assert!(t1.elapsed() < Duration::from_millis(500));
        assert_eq!(r_str["result"]["isError"], true);

        // 3) Unknown and already-completed requestIds are silent no-ops.
        send_json_line(&in_tx, &json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 99999 } }));
        send_json_line(&in_tx, &json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 42 } }));
        send_json_line(&in_tx, &json!({ "jsonrpc": "2.0", "id": 77, "method": "ping" }));
        let (notes, r77) = recv_until_id(&out_rx, &json!(77), Duration::from_secs(1));
        assert!(notes.is_empty());
        assert_eq!(r77["result"], json!({}));

        drop(in_tx);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn eof_with_cancel_on_eof_aborts_active_wait() {
        let srv = Arc::new(server());
        let (in_tx, out_rx, handle) = spawn_stream_server(srv, true);
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "memguard_wait",
                "arguments": { "ram_gb": 1.0e9, "timeout_sec": 30 },
                "_meta": { "progressToken": "eof" }
            }
        }));
        let _ = out_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let t0 = Instant::now();
        drop(in_tx);
        handle.join().unwrap().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(1), "EOF did not cancel wait promptly");
        let (_notes, reply) = recv_until_id(&out_rx, &json!(10), Duration::from_secs(1));
        assert_eq!(reply["result"]["isError"], true);
    }

    #[test]
    fn cancelled_aborts_disk_checkup_and_kills_child() {
        // 1) Back-to-back cancel aborts memguard_disk_checkup without touching scan runner.
        let root = std::env::temp_dir().join(format!("amg-mcp-chk-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"Game\Intermediate")).unwrap();
        std::fs::write(root.join(r"Game\Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_flag = ran.clone();
        let srv = Arc::new(server().with_disk(
            Box::new(move |job, p| {
                ran_flag.store(true, Ordering::SeqCst);
                Ok(crate::disk_scan::run_job(job, p))
            }),
            Box::new(|| None),
        ));
        let (in_tx, out_rx, handle) = spawn_stream_server(srv, true);
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "id": "chk1",
            "method": "tools/call",
            "params": {
                "name": "memguard_disk_checkup",
                "arguments": { "path": root.display().to_string(), "refresh": true, "budget_sec": 30 }
            }
        }));
        send_json_line(&in_tx, &json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": "chk1" }
        }));
        let (_notes, reply) = recv_until_id(&out_rx, &json!("chk1"), Duration::from_secs(2));
        assert_eq!(reply["result"]["isError"], true);
        assert!(!ran.load(Ordering::SeqCst), "pre-cancelled checkup must not launch scan");
        drop(in_tx);
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);

        // 2) Active long-running child process is killed promptly on CancelToken::cancel().
        let tok = Arc::new(CancelToken::new());
        let tok_bg = tok.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            tok_bg.cancel();
        });
        let started = Instant::now();
        let ps = std::path::Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
        let exe = if ps.is_file() { ps } else { std::path::Path::new("cmd.exe") };
        let args: &[&str] = if ps.is_file() {
            &["-NoProfile", "-Command", "Start-Sleep -Seconds 15"]
        } else {
            &["/C", "ping -n 15 127.0.0.1 >nul"]
        };
        let res = run_child_cancellable_cmd(exe, args, None, &mut |_, _| {}, &tok);
        assert!(res.is_err());
        assert!(started.elapsed() < Duration::from_secs(2), "child kill took {:?}", started.elapsed());
    }

    #[test]
    fn config_mtime_cache_hits_and_invalidates_on_edits() {
        let root = std::env::temp_dir().join(format!("amg-mcp-cfg-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg_path = root.join("settings.toml");
        let team_path = root.join("team.toml");

        let loads = Arc::new(AtomicU64::new(0));
        let loads_c = loads.clone();
        let cp = cfg_path.clone();
        let tp = team_path.clone();
        let loader = cached_config_loader_for(
            move || cp.clone(),
            move || tp.clone(),
            move || {
                loads_c.fetch_add(1, Ordering::SeqCst);
                Config::builtin()
            },
        );

        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 1, "cache hit when neither file exists");

        std::fs::write(&cfg_path, b"interval_sec = 10\n").unwrap();
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 2, "invalidates when settings.toml created");
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 2, "cache hit when settings.toml unchanged");

        std::fs::write(&cfg_path, b"interval_sec = 15\n# changed length\n").unwrap();
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 3, "invalidates when settings.toml edited");

        std::fs::write(&team_path, b"interval_sec = 20\n").unwrap();
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 4, "invalidates when team.toml created");
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 4, "cache hit when both unchanged");

        std::fs::remove_file(&team_path).unwrap();
        let _ = loader();
        assert_eq!(loads.load(Ordering::SeqCst), 5, "invalidates when team.toml removed");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn request_id_normalizes_types_cleanly() {
        assert_eq!(RequestId::from_value(&json!(42)), RequestId::Int(42));
        assert_eq!(RequestId::from_value(&json!(u64::MAX)), RequestId::Uint(u64::MAX));
        assert_eq!(RequestId::from_value(&json!("abc")), RequestId::Str("abc".into()));
        assert_ne!(RequestId::from_value(&json!(42)), RequestId::from_value(&json!("42")));
        assert_eq!(RequestId::from_value(&json!(true)), RequestId::Bool(true));
        assert_eq!(RequestId::from_value(&Value::Null), RequestId::Null);
    }

    #[test]
    fn new_m3_tools_work_and_validate_arguments() {
        // 1) memguard_drivers
        let r = one(&call(1, "memguard_drivers", json!({})));
        assert_eq!(r["result"]["isError"], false, "{}", r);
        let sc = &r["result"]["structuredContent"];
        assert!(sc["total_gb"].is_number());
        assert!(sc["tags_available"].is_boolean());
        tool_error_text(&one(&call(2, "memguard_drivers", json!({ "extra": 1 }))));

        // 2) memguard_disk_relocate (validation & negative paths without touching registry)
        tool_error_text(&one(&call(3, "memguard_disk_relocate", json!({ "to": "relative\\path" }))));
        tool_error_text(&one(&call(4, "memguard_disk_relocate", json!({ "to": r"Q:\NoSuchDrive\Cache" }))));
        tool_error_text(&one(&call(5, "memguard_disk_relocate", json!({ "apply": true }))));

        // 3) memguard_disk_bin (isolated DiskFiles in temp dir)
        let root = std::env::temp_dir().join(format!("amg-mcp-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = crate::disk_clean::DiskFiles::in_dir(root.join("data"));
        let bin_srv = server().with_clean(files, crate::disk_clean::CleanOpts::default());
        let mut out = Vec::new();
        let reqs = format!(
            "{}\n{}\n{}\n",
            call(6, "memguard_disk_bin", json!({})),
            call(7, "memguard_disk_bin", json!({ "volume": "badvol" })),
            call(8, "memguard_disk_bin", json!({ "empty": true }))
        );
        bin_srv.serve(reqs.as_bytes(), &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
        assert!(replies[0]["result"]["structuredContent"]["runs"].is_array());
        assert!(replies[0]["result"]["structuredContent"]["hold_hours"].is_number());
        assert!(replies[0]["result"]["structuredContent"]["expired_run_ids"].is_array());
        assert_eq!(replies[1]["result"]["isError"], true);
        assert_eq!(replies[2]["result"]["isError"], true);
        let _ = std::fs::remove_dir_all(&root);

        // 4) memguard_config (injected ConfigLoader -> never writes %APPDATA%)
        let cfg_srv = Server::new(Box::new(|| {
            let mut c = Config::builtin();
            c.interval_sec = 42;
            c
        }));
        let run_cfg = |line: &str| -> Value {
            let mut o = Vec::new();
            cfg_srv.serve(line.as_bytes(), &mut o).unwrap();
            serde_json::from_str(String::from_utf8(o).unwrap().trim()).unwrap()
        };
        let r_all = run_cfg(&call(9, "memguard_config", json!({})));
        assert_eq!(r_all["result"]["isError"], false, "{}", r_all);
        let sc = &r_all["result"]["structuredContent"];
        assert_eq!(sc["config"]["interval_sec"], 42);
        assert!(sc["config"]["disk"].is_object());
        assert!(sc["settings_file"].is_string());
        assert!(sc["team_file"].is_string());
        assert!(sc["team_active"].is_boolean());

        let r_path = run_cfg(&call(10, "memguard_config", json!({ "path": "interval_sec" })));
        assert_eq!(r_path["result"]["structuredContent"]["found"], true);
        assert_eq!(r_path["result"]["structuredContent"]["value"], 42);
        assert_eq!(r_path["result"]["structuredContent"]["layer"], "builtin");

        let r_grp = run_cfg(&call(11, "memguard_config", json!({ "path": "group.Browsers.trim_above_pct" })));
        assert_eq!(r_grp["result"]["structuredContent"]["found"], true);

        let r_miss = run_cfg(&call(12, "memguard_config", json!({ "path": "no.such.key" })));
        assert_eq!(r_miss["result"]["structuredContent"]["found"], false);
        assert_eq!(r_miss["result"]["structuredContent"]["value"], Value::Null);

        tool_error_text(&run_cfg(&call(13, "memguard_config", json!({ "path": "group." }))));
        tool_error_text(&run_cfg(&call(14, "memguard_config", json!({ "path": "" }))));
        tool_error_text(&run_cfg(&call(15, "memguard_config", json!({ "extra": true }))));

        // 5) memguard_session_summary
        let sum_srv = server().with_history(Box::new(|_| (0..3).map(|i| sample(1000 + i * 60)).collect()));
        let mut o = Vec::new();
        let reqs = format!(
            "{}\n{}\n{}\n",
            call(16, "memguard_session_summary", json!({})),
            call(17, "memguard_session_summary", json!({ "hours": 0 })),
            call(18, "memguard_session_summary", json!({ "days": 1 }))
        );
        sum_srv.serve(reqs.as_bytes(), &mut o).unwrap();
        let sum_replies: Vec<Value> = String::from_utf8(o).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(sum_replies[0]["result"]["isError"], false, "{}", sum_replies[0]);
        let ss = &sum_replies[0]["result"]["structuredContent"];
        assert_eq!(ss["sample_count"], 3);
        assert_eq!(ss["peak_ue_committed_gb"], 20.0);
        assert_eq!(ss["groups"][0]["name"], "Browsers");
        assert_eq!(sum_replies[1]["result"]["isError"], true);
        assert_eq!(sum_replies[2]["result"]["isError"], true);
    }
}

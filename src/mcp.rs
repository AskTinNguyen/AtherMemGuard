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

const INSTRUCTIONS: &str = "Ather MemGuard keeps this Windows PC's memory free for Unreal Engine work. \
Before starting a local Unreal build, cook or packaging run, call memguard_wait with prime: true and the \
workload (build or cook); start the job only when it returns ready: true. Call memguard_advise to pick \
-MaxParallelActions for UnrealBuildTool. Call memguard_status when memory looks tight or a build failed \
with out-of-memory errors. When the person asks what is using memory or how to free more, call memguard_doctor \
and relay its findings; it changes nothing, and you should ask the person before running `memguard doctor --fix`.";

/// Settings source. The real server reads settings.toml on every call (so edits apply
/// at once); tests pass the built-in defaults.
pub type ConfigLoader = Box<dyn Fn() -> Config>;

pub struct Server {
    load_config: ConfigLoader,
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
        Server { load_config }
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
                check_keys(args, &["workload", "ram_gb", "commit_gb", "timeout_sec", "prime"])?;
                let workload = workload_arg(args)?;
                let ram_gb = gb_arg(args, "ram_gb")?;
                let commit_gb = gb_arg(args, "commit_gb")?;
                let timeout = timeout_arg(args)?;
                let prime = bool_arg(args, "prime")?.unwrap_or(false);
                if workload.is_none() && ram_gb.is_none() && commit_gb.is_none() {
                    return Err(ToolError(
                        "Say what to wait for: workload (editor, build or cook), or ram_gb / commit_gb.".into(),
                    ));
                }
                let cfg = (self.load_config)();
                let base = workload.and_then(|w| cfg.prime.target(w)).unwrap_or(Target { ram_gb: 0.0, commit_gb: 0.0 });
                let need = Target { ram_gb: ram_gb.unwrap_or(base.ram_gb), commit_gb: commit_gb.unwrap_or(base.commit_gb) };
                if need.ram_gb <= 0.0 && need.commit_gb <= 0.0 {
                    return Err(ToolError("Nothing to wait for: the RAM and commit needed are both 0.".into()));
                }
                if prime {
                    ops::prime(&cfg, None, false);
                }
                let total = timeout.as_secs_f64();
                let started = std::time::Instant::now();
                let result = ops::wait_for(need, timeout, |sys| {
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
                to_value(&result)
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
            // TODO(part-b): add `memguard_history` (`hours?` default 1) once Part B's
            // `history` module is merged: register it in `tool_list()` and call
            // `crate::history` here, returning the same JSON as `memguard history --json`.
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
                    "prime": { "type": "boolean", "default": false, "description": "Free RAM once (like memguard_prime) before waiting. Recommended before a build or cook." }
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
    ]
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
        assert_eq!(names, ["memguard_status", "memguard_prime", "memguard_wait", "memguard_advise", "memguard_doctor"]);
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

//! Opaque-box E2E tests for Feature Areas F7 (Daemon Control IPC) and F8 (MCP Server).
//!
//! Strictly uses only `std`, `serde_json`, and `env!("CARGO_BIN_EXE_memguard")`.
//! Every test isolates its data folder via `ATHER_MEMGUARD_DIR` (and `ATHER_MEMGUARD_ENV_KEY`
//! when applicable) per child `Command`, and wraps all spawned processes, temp folders,
//! watcher children, and throwaway HKCU keys in RAII `Drop` guards.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MEMGUARD_BIN: &str = env!("CARGO_BIN_EXE_memguard");

static SEQ: AtomicU64 = AtomicU64::new(1);
static DAEMON_SERIAL: Mutex<()> = Mutex::new(());

fn lock_daemon_serial() -> MutexGuard<'static, ()> {
    DAEMON_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn run_with_timeout(tmp_dir: &Path, mut cmd: Command, timeout: Duration) -> Output {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let out_path = tmp_dir.join(format!("_e2e_out_{}_{}.tmp", std::process::id(), seq));
    let err_path = tmp_dir.join(format!("_e2e_err_{}_{}.tmp", std::process::id(), seq));
    let out_file = fs::File::create(&out_path).expect("create stdout temp file");
    let err_file = fs::File::create(&err_path).expect("create stderr temp file");

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
        .expect("spawn memguard command");

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().expect("try_wait child") {
            Some(st) => break st,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = fs::read(&out_path).unwrap_or_default();
                let stderr = fs::read(&err_path).unwrap_or_default();
                let _ = fs::remove_file(&out_path);
                let _ = fs::remove_file(&err_path);
                panic!(
                    "command timed out after {:?}; stdout={}; stderr={}",
                    timeout,
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
        }
    };

    let stdout = fs::read(&out_path).unwrap_or_default();
    let stderr = fs::read(&err_path).unwrap_or_default();
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    Output {
        status,
        stdout,
        stderr,
    }
}

// ---------------------------------------------------------------- RAII isolation guards

struct EnvDir {
    dir: PathBuf,
    env_key: Option<String>,
}

impl EnvDir {
    fn new(tag: &str) -> Self {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("amg_e2e_mcp_ipc_{}_{}_{}", std::process::id(), id, tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp ATHER_MEMGUARD_DIR");
        Self { dir, env_key: None }
    }

    fn with_env_key(mut self, tag: &str) -> Self {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        self.env_key = Some(format!(
            r"Software\AtherMemGuard-test\e2e_mcp_ipc_{}_{}_{}",
            std::process::id(),
            id,
            tag
        ));
        self
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(MEMGUARD_BIN);
        c.args(args)
            .env("ATHER_MEMGUARD_DIR", &self.dir)
            .creation_flags(CREATE_NO_WINDOW);
        if let Some(key) = &self.env_key {
            c.env("ATHER_MEMGUARD_ENV_KEY", key);
        }
        c
    }

    fn run_output(&self, args: &[&str]) -> Output {
        run_with_timeout(&self.dir, self.cmd(args), Duration::from_secs(30))
    }

    fn status_json(&self) -> Value {
        let out = self.run_output(&["status", "--json"]);
        assert!(
            out.status.success(),
            "memguard status --json failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("parse status --json")
    }

    fn write_settings(&self, toml_text: &str) {
        fs::write(self.dir.join("settings.toml"), toml_text).expect("write settings.toml");
    }

    fn read_log(&self) -> String {
        fs::read_to_string(self.dir.join("memguard.log")).unwrap_or_default()
    }
}

impl Drop for EnvDir {
    fn drop(&mut self) {
        let watch_path = self.dir.join("disk-watch.json");
        if let Ok(text) = fs::read_to_string(&watch_path) {
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&text) {
                for item in arr {
                    if let Some(pid) = item.get("watcher_pid").and_then(Value::as_u64) {
                        if pid > 0 {
                            let _ = Command::new("taskkill")
                                .args(["/F", "/PID", &pid.to_string()])
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .creation_flags(CREATE_NO_WINDOW)
                                .status();
                        }
                    }
                }
            }
        }
        if let Some(key) = &self.env_key {
            if key.starts_with(r"Software\AtherMemGuard-test\") {
                let _ = Command::new("reg")
                    .args(["delete", &format!(r"HKCU\{}", key), "/f"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .status();
            }
        }
        for _ in 0..30 {
            if !self.dir.exists() || fs::remove_dir_all(&self.dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_dummy_ping(dir: &Path, exe_stem: &str) -> Kill {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let src = PathBuf::from(windir).join(r"System32\PING.EXE");
    let dst = dir.join(format!("{}.exe", exe_stem));
    if !dst.exists() {
        fs::copy(&src, &dst).expect("copy PING.EXE to dummy exe");
    }
    let child = Command::new(&dst)
        .args(["-t", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("spawn dummy ping child");
    Kill(child)
}

struct DaemonGuard<'a> {
    _lock: MutexGuard<'static, ()>,
    env: &'a EnvDir,
    child: Option<Child>,
}

impl<'a> DaemonGuard<'a> {
    fn start(env: &'a EnvDir) -> Self {
        let lock = lock_daemon_serial();
        let mut child = env
            .cmd(&["daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn memguard daemon");

        let stdout = child.stdout.take().expect("daemon stdout");
        let stderr = child.stderr.take().expect("daemon stderr");

        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(Err("daemon closed stdout immediately".into()));
                }
                Ok(_) => {
                    let _ = tx.send(Ok(line));
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
            // Drain remaining stdout so the daemon never blocks on a full pipe.
            let mut sink = String::new();
            while reader.read_line(&mut sink).unwrap_or(0) > 0 {
                sink.clear();
            }
        });

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(mut b) = stderr_buf_clone.lock() {
                    b.push_str(&line);
                }
                line.clear();
            }
        });

        let mut guard = Self {
            _lock: lock,
            env,
            child: Some(child),
        };

        let first_line = match rx.recv_timeout(Duration::from_secs(12)) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                let err = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
                panic!("daemon failed to start ({}); stderr: {}", e, err);
            }
            Err(_) => {
                let err = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
                panic!("timed out waiting for daemon first tick; stderr: {}", err);
            }
        };
        assert!(
            !first_line.trim().is_empty(),
            "daemon first stdout line should not be empty"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while !env.dir.join("status.json").exists() {
            assert!(
                Instant::now() < deadline,
                "status.json was not written by daemon"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        guard.child.as_mut().expect("child present");
        guard
    }

    fn stop(&mut self) -> Output {
        let out = self.env.run_output(&["stop"]);
        if let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    let _ = child.wait();
                    self.child = None;
                    return out;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            self.child = None;
        }
        out
    }

    fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = self.env.run_output(&["stop"]);
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut exited = false;
                while Instant::now() < deadline {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        exited = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !exited {
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        }
    }
}

struct McpClient {
    stdin: Option<BufWriter<ChildStdin>>,
    rx: mpsc::Receiver<Result<String, String>>,
    stderr_buf: Arc<Mutex<String>>,
    child: Child,
}

impl McpClient {
    fn spawn(env: &EnvDir) -> Self {
        let mut child = env
            .cmd(&["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn memguard mcp");

        let stdin = BufWriter::new(child.stdin.take().expect("mcp stdin"));
        let stdout = child.stdout.take().expect("mcp stdout");
        let stderr = child.stderr.take().expect("mcp stderr");

        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(&['\r', '\n'][..]).to_string();
                        if tx.send(Ok(trimmed)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        });

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_clone = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(mut b) = stderr_clone.lock() {
                    b.push_str(&line);
                }
                line.clear();
            }
        });

        Self {
            stdin: Some(stdin),
            rx,
            stderr_buf,
            child,
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
    }

    fn send_raw_line(&mut self, raw: &str) {
        let w = self.stdin.as_mut().expect("mcp stdin open");
        w.write_all(raw.as_bytes()).expect("write mcp line");
        w.write_all(b"\n").expect("write mcp newline");
        w.flush().expect("flush mcp stdin");
    }

    fn send_json(&mut self, v: &Value) {
        self.send_raw_line(&serde_json::to_string(v).expect("serialize JSON-RPC"));
    }

    fn recv_msg(&self, timeout: Duration) -> Result<Value, String> {
        let line = self
            .rx
            .recv_timeout(timeout)
            .map_err(|e| format!("MCP recv_timeout ({:?}): {}; stderr={}", timeout, e, self.stderr_text()))?
            .map_err(|e| format!("MCP stdout read error: {}", e))?;
        serde_json::from_str(&line).map_err(|e| format!("invalid JSON from MCP: {} (line={:?})", e, line))
    }

    fn recv_until_id(&self, target_id: &Value, timeout: Duration) -> Result<(Vec<Value>, Value), String> {
        let deadline = Instant::now() + timeout;
        let mut notes = Vec::new();
        loop {
            let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                return Err(format!("timed out waiting for MCP id={}", target_id));
            };
            if rem.is_zero() {
                return Err(format!("timed out waiting for MCP id={}", target_id));
            }
            let msg = self.recv_msg(rem)?;
            if msg.get("id").is_none() {
                notes.push(msg);
                continue;
            }
            if msg.get("id") == Some(target_id) {
                return Ok((notes, msg));
            }
            return Err(format!("unexpected MCP response id (wanted {}): {}", target_id, msg));
        }
    }

    fn recv_notification(&self, method: &str, timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                return Err(format!("timed out waiting for notification {}", method));
            };
            if rem.is_zero() {
                return Err(format!("timed out waiting for notification {}", method));
            }
            let msg = self.recv_msg(rem)?;
            if msg.get("id").is_none() && msg.get("method").and_then(Value::as_str) == Some(method) {
                return Ok(msg);
            }
        }
    }

    fn initialize(&mut self, id: i64, protocol_version: &str) -> Value {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": { "name": "e2e_test", "version": "1.0" }
            }
        }));
        let (_notes, reply) = self
            .recv_until_id(&json!(id), Duration::from_secs(5))
            .expect("initialize reply");
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        reply
    }

    fn call_tool(
        &mut self,
        id: i64,
        name: &str,
        args: Value,
        meta: Option<Value>,
        timeout: Duration,
    ) -> Result<(Vec<Value>, Value), String> {
        let mut params = json!({ "name": name, "arguments": args });
        if let Some(m) = meta {
            params["_meta"] = m;
        }
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params
        }));
        self.recv_until_id(&json!(id), timeout)
    }

    fn close_and_wait(&mut self) -> i32 {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(st)) = self.child.try_wait() {
                return st.code().unwrap_or(-1);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        self.child.wait().ok().and_then(|s| s.code()).unwrap_or(-1)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn age_tree(root: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    fn walk(p: &Path, when: SystemTime) {
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                walk(&e.path(), when);
            }
        }
        if let Ok(f) = fs::OpenOptions::new()
            .access_mode(0x100)
            .custom_flags(0x0200_0000)
            .open(p)
        {
            let _ = f.set_modified(when);
        }
    }
    walk(root, when);
}

fn create_uproject_fixture(env: &EnvDir) -> PathBuf {
    let proj = env.dir.join("proj").join("Game");
    for d in [r"Intermediate\Build\Win64", r"Saved\Logs", "Content", "Config"] {
        fs::create_dir_all(proj.join(d)).expect("create fixture dir");
    }
    fs::write(
        proj.join("Game.uproject"),
        br#"{"FileVersion": 3, "EngineAssociation": "5.4"}"#,
    )
    .expect("write .uproject");
    fs::write(proj.join(r"Intermediate\Build\Win64\a.obj"), vec![1u8; 256 * 1024])
        .expect("write a.obj");
    fs::write(proj.join(r"Saved\Logs\Game.log"), vec![2u8; 32 * 1024])
        .expect("write Game.log");
    fs::write(proj.join(r"Content\Hero.uasset"), b"asset").expect("write Hero.uasset");
    fs::write(proj.join(r"Config\DefaultGame.ini"), b"[x]").expect("write ini");
    age_tree(&env.dir.join("proj"), 3 * 3600);
    env.dir.join("proj")
}

fn poll_until<F: FnMut() -> bool>(timeout: Duration, mut pred: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    pred()
}

// ============================================================================
// Feature Area F7: Daemon Control IPC (6 Tier 1 + 6 Tier 2 = 12 Tests)
// ============================================================================

#[test]
fn t1_f7_daemon_start_status_and_stop_lifecycle() {
    let env = EnvDir::new("f7_lifecycle");
    env.write_settings("interval_sec = 2\n");

    {
        let _g = lock_daemon_serial();
        let st_before = env.status_json();
        assert_eq!(st_before["daemon"], false);
    }

    let mut daemon = DaemonGuard::start(&env);
    let st_running = env.status_json();
    assert_eq!(st_running["daemon"], true);
    assert_eq!(st_running["paused"], false);

    let stop_out = daemon.stop();
    assert!(
        stop_out.status.success(),
        "memguard stop failed: {}",
        String::from_utf8_lossy(&stop_out.stderr)
    );

    let log = env.read_log();
    assert!(log.contains("started (headless)"), "log missing start: {}", log);
    assert!(
        log.contains("stopped, apps restored to normal"),
        "log missing stop: {}",
        log
    );

    let st_after = env.status_json();
    assert_eq!(st_after["daemon"], false);
}

#[test]
fn t1_f7_daemon_writes_history_csv_and_status_json() {
    let env = EnvDir::new("f7_history");
    env.write_settings("interval_sec = 2\n[history]\nenabled = true\n");

    let mut daemon = DaemonGuard::start(&env);
    let status_file = env.dir.join("status.json");
    assert!(status_file.is_file(), "status.json should exist");

    // Wait briefly for a second tick and stop cleanly so buffered CSVs flush.
    std::thread::sleep(Duration::from_millis(2300));
    let stop_out = daemon.stop();
    assert!(stop_out.status.success());

    let hist_dir = env.dir.join("history");
    let csvs: Vec<PathBuf> = fs::read_dir(&hist_dir)
        .expect("read history dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("csv"))
        .collect();
    assert!(!csvs.is_empty(), "expected YYYY-MM-DD.csv in history/");
    let csv_text = fs::read_to_string(&csvs[0]).expect("read history csv");
    assert!(
        csv_text.lines().count() >= 2,
        "expected header + >= 1 sample row in CSV, got:\n{}",
        csv_text
    );

    let hist_out = env.run_output(&["history", "--hours", "1", "--json"]);
    assert!(hist_out.status.success());
    let samples: Vec<Value> = serde_json::from_slice(&hist_out.stdout).expect("parse history --json");
    assert!(!samples.is_empty(), "expected >= 1 history sample");
}

#[test]
fn t1_f7_pause_and_resume_ipc() {
    let env = EnvDir::new("f7_pause_resume");
    env.write_settings("interval_sec = 2\n");

    let _daemon = DaemonGuard::start(&env);
    let pause_out = env.run_output(&["pause"]);
    assert!(
        pause_out.status.success(),
        "memguard pause failed (exit {:?}): {}",
        pause_out.status.code(),
        String::from_utf8_lossy(&pause_out.stderr)
    );

    let paused_ok = poll_until(Duration::from_secs(4), || env.status_json()["paused"] == true);
    assert!(paused_ok, "status --json did not report paused: true within 4s");

    let resume_out = env.run_output(&["resume"]);
    assert!(
        resume_out.status.success(),
        "memguard resume failed: {}",
        String::from_utf8_lossy(&resume_out.stderr)
    );

    let resumed_ok = poll_until(Duration::from_secs(4), || env.status_json()["paused"] == false);
    assert!(resumed_ok, "status --json did not report paused: false within 4s");
}

#[test]
fn t1_f7_pause_with_minutes_flag() {
    let env = EnvDir::new("f7_pause_mins");
    env.write_settings("interval_sec = 2\n");

    let _daemon = DaemonGuard::start(&env);
    let pause_out = env.run_output(&["pause", "--minutes", "5"]);
    assert!(
        pause_out.status.success(),
        "memguard pause --minutes 5 failed (exit {:?}): {}",
        pause_out.status.code(),
        String::from_utf8_lossy(&pause_out.stderr)
    );

    let paused_ok = poll_until(Duration::from_secs(4), || env.status_json()["paused"] == true);
    assert!(paused_ok, "status --json did not report paused: true within 4s");

    let resume_out = env.run_output(&["resume"]);
    assert!(resume_out.status.success());

    let resumed_ok = poll_until(Duration::from_secs(4), || env.status_json()["paused"] == false);
    assert!(resumed_ok, "status --json did not report paused: false within 4s");
}

#[test]
fn t1_f7_reload_ipc_applies_updated_settings() {
    let env = EnvDir::new("f7_reload");
    env.write_settings("interval_sec = 30\n[general]\npreset = \"balanced\"\n");

    let _daemon = DaemonGuard::start(&env);
    let set_out = env.run_output(&["config", "set", "general.preset", "aggressive"]);
    assert!(
        set_out.status.success(),
        "config set failed: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );

    let reload_out = env.run_output(&["reload"]);
    assert!(
        reload_out.status.success(),
        "memguard reload failed (exit {:?}): {}",
        reload_out.status.code(),
        String::from_utf8_lossy(&reload_out.stderr)
    );

    // Must take effect quickly (<< 30s interval) via IPC signal.
    let reloaded = poll_until(Duration::from_secs(5), || {
        env.read_log().contains("preset aggressive")
    });
    assert!(
        reloaded,
        "memguard.log did not record `preset aggressive` within 5s after reload:\n{}",
        env.read_log()
    );
}

#[test]
fn t1_f7_daemon_respects_custom_group_and_restores_on_stop() {
    let env = EnvDir::new("f7_custom_group");
    let stem = format!("E2EF7Ping_{}", std::process::id());
    let _dummy = spawn_dummy_ping(&env.dir, &stem);

    env.write_settings(&format!(
        "interval_sec = 2\n[[group]]\nname = \"E2EF7Test\"\nprocesses = [\"{}\"]\npriority = \"low\"\n",
        stem
    ));

    let mut daemon = DaemonGuard::start(&env);
    let st = env.status_json();
    let groups = st["groups"].as_array().expect("groups array");
    let g = groups
        .iter()
        .find(|g| g["name"] == "E2EF7Test")
        .expect("E2EF7Test group in status --json");
    assert!(
        g["processes"].as_u64().unwrap_or(0) >= 1,
        "expected >= 1 process in E2EF7Test: {}",
        g
    );

    let stop_out = daemon.stop();
    assert!(stop_out.status.success());
    let log = env.read_log();
    assert!(
        log.contains("stopped, apps restored to normal"),
        "expected restore log on stop: {}",
        log
    );
}

#[test]
fn t2_f7_second_daemon_rejected_by_single_instance_mutex() {
    let env = EnvDir::new("f7_mutex");
    env.write_settings("interval_sec = 2\n");

    let mut first = DaemonGuard::start(&env);
    let second_out = env.run_output(&["daemon"]);
    assert!(
        second_out.status.success(),
        "second daemon should exit 0 cleanly when mutex is held, got {:?}",
        second_out.status.code()
    );
    let stderr = String::from_utf8_lossy(&second_out.stderr);
    assert!(
        stderr.contains("already running"),
        "expected `already running` on stderr, got: {}",
        stderr
    );
    assert!(first.is_running(), "first daemon should remain alive");
}

#[test]
fn t2_f7_stop_when_no_daemon_running_is_idempotent() {
    let env = EnvDir::new("f7_stop_idle");
    let _g = lock_daemon_serial();
    assert_eq!(env.status_json()["daemon"], false);

    let stop_out = env.run_output(&["stop"]);
    assert!(
        stop_out.status.success(),
        "stop when not running should exit 0: {}",
        String::from_utf8_lossy(&stop_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&stop_out.stdout);
    assert!(
        stdout.contains("not running"),
        "expected `not running` message, got: {}",
        stdout
    );
}

#[test]
fn t2_f7_ipc_commands_fail_when_no_daemon_running() {
    let env = EnvDir::new("f7_ipc_no_daemon");
    let _g = lock_daemon_serial();
    assert_eq!(env.status_json()["daemon"], false);

    for args in [&["pause"][..], &["resume"][..], &["reload"][..]] {
        let out = env.run_output(args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "`memguard {:?}` should fail when no daemon is running",
            args
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "`memguard {:?}` should exit with code 1 (IPC failure, not clap parse error), stderr: {}",
            args,
            stderr
        );
        assert!(
            !stderr.contains("unrecognized subcommand"),
            "`memguard {:?}` hit missing subcommand instead of IPC error: {}",
            args,
            stderr
        );
        assert!(
            !stderr.contains("unexpected argument"),
            "`memguard {:?}` hit unexpected argument instead of IPC error: {}",
            args,
            stderr
        );
        assert!(
            !stderr.contains("Usage:"),
            "`memguard {:?}` printed clap Usage instead of IPC error: {}",
            args,
            stderr
        );
    }
}

#[test]
fn t2_f7_pause_invalid_minutes_rejected() {
    let env = EnvDir::new("f7_pause_bad_mins");
    env.write_settings("interval_sec = 2\n");
    let _daemon = DaemonGuard::start(&env);

    for args in [
        &["pause", "--minutes", "0"][..],
        &["pause", "--minutes", "-5"][..],
        &["pause", "--minutes", "abc"][..],
    ] {
        let out = env.run_output(args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "`memguard {:?}` should be rejected",
            args
        );
        assert!(
            !stderr.contains("unrecognized subcommand"),
            "`memguard {:?}` failed due to missing subcommand: {}",
            args,
            stderr
        );
        assert!(
            !stderr.contains("unexpected argument '--minutes'"),
            "`memguard {:?}` failed due to unexpected `--minutes` argument: {}",
            args,
            stderr
        );
        if args != ["pause", "--minutes", "-5"] {
            assert!(
                !stderr.contains("unexpected argument"),
                "`memguard {:?}` failed due to unexpected argument (e.g. boolean `--minutes`): {}",
                args,
                stderr
            );
        }
        if args == ["pause", "--minutes", "0"] {
            assert!(
                !stderr.to_ascii_lowercase().contains("not running"),
                "`memguard pause --minutes 0` must fail validation, not IPC not-running: {}",
                stderr
            );
        }
        let st = env.status_json();
        assert_eq!(st["daemon"], true, "daemon should remain alive after {:?}", args);
        assert_eq!(st["paused"], false, "daemon must remain unpaused after {:?}", args);
    }
}

#[test]
fn t2_f7_reload_with_invalid_settings_keeps_daemon_alive() {
    let env = EnvDir::new("f7_reload_invalid");
    env.write_settings("interval_sec = 30\n");

    let mut daemon = DaemonGuard::start(&env);
    env.write_settings("interval_sec = [not valid toml\n");

    let reload_out = env.run_output(&["reload"]);
    assert!(
        reload_out.status.success(),
        "memguard reload failed (exit {:?}): {}",
        reload_out.status.code(),
        String::from_utf8_lossy(&reload_out.stderr)
    );

    let logged_err = poll_until(Duration::from_secs(5), || {
        env.read_log().contains("settings error, keeping previous settings")
    });
    assert!(
        logged_err,
        "expected `settings error, keeping previous settings` in log:\n{}",
        env.read_log()
    );
    assert!(daemon.is_running(), "daemon should stay alive after invalid reload");
    assert_eq!(env.status_json()["daemon"], true);
}

#[test]
fn t2_f7_idempotent_double_pause_and_double_resume() {
    let env = EnvDir::new("f7_double_ipc");
    env.write_settings("interval_sec = 2\n");

    let _daemon = DaemonGuard::start(&env);

    for i in 0..2 {
        let out = env.run_output(&["pause"]);
        assert!(
            out.status.success(),
            "pause #{} failed: {}",
            i + 1,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        poll_until(Duration::from_secs(4), || env.status_json()["paused"] == true),
        "expected paused: true after double pause"
    );

    for i in 0..2 {
        let out = env.run_output(&["resume"]);
        assert!(
            out.status.success(),
            "resume #{} failed: {}",
            i + 1,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        poll_until(Duration::from_secs(4), || env.status_json()["paused"] == false),
        "expected paused: false after double resume"
    );
}

// ============================================================================
// Feature Area F8: MCP Server (8 Tier 1 + 7 Tier 2 = 15 Tests)
// ============================================================================

#[test]
fn t1_f8_initialize_protocol_versions_and_ping() {
    let env = EnvDir::new("f8_init");
    let mut mcp = McpClient::spawn(&env);

    for (idx, (asked, expected)) in [
        ("2025-06-18", "2025-06-18"),
        ("2025-03-26", "2025-03-26"),
        ("2024-11-05", "2024-11-05"),
        ("1999-01-01", "2025-06-18"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = (idx + 1) as i64;
        let reply = mcp.initialize(id, asked);
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id);
        let res = &reply["result"];
        assert_eq!(res["protocolVersion"], expected);
        assert_eq!(res["serverInfo"]["name"], "ather-memguard");
        assert_eq!(res["capabilities"], json!({ "tools": {} }));
    }

    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 99, "method": "ping" }));
    let (_notes, ping_reply) = mcp
        .recv_until_id(&json!(99), Duration::from_secs(5))
        .expect("ping reply");
    assert_eq!(ping_reply["result"], json!({}));
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_tools_list_16_tools_and_annotations() {
    let env = EnvDir::new("f8_tools_list");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let (_notes, reply) = mcp
        .recv_until_id(&json!(2), Duration::from_secs(5))
        .expect("tools/list");
    let tools = reply["result"]["tools"]
        .as_array()
        .expect("tools array");

    let expected_names = [
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
    ];
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(
        tools.len(),
        16,
        "expected 16 MCP tools, got {}: {:?}",
        tools.len(),
        names
    );
    for want in expected_names {
        assert!(names.contains(&want), "missing tool {}", want);
    }

    for t in tools {
        let name = t["name"].as_str().unwrap_or("");
        assert_eq!(t["inputSchema"]["type"], "object", "{}", name);
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", name);
        let is_clean = name == "memguard_disk_clean";
        if is_clean {
            assert_eq!(t["annotations"]["destructiveHint"], true, "{}", name);
            assert_eq!(t["annotations"]["readOnlyHint"], false, "{}", name);
        } else {
            assert_ne!(t["annotations"]["destructiveHint"], true, "{}", name);
        }
    }
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_core_readonly_tools_baseline() {
    let env = EnvDir::new("f8_core_readonly");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let cases: &[(&str, Value, &[&str])] = &[
        (
            "memguard_status",
            json!({}),
            &["time", "daemon", "paused", "mode", "pressure", "system", "ue", "groups"],
        ),
        (
            "memguard_advise",
            json!({}),
            &["suggested_max_parallel_actions", "logical_cores", "notes"],
        ),
        (
            "memguard_doctor",
            json!({}),
            &["time", "system", "findings", "reclaimable_ram_gb"],
        ),
        (
            "memguard_ram",
            json!({}),
            &["verdict", "headline", "summary", "sticks", "board", "cpu"],
        ),
        ("memguard_disk", json!({}), &["volumes", "dev_drive"]),
        (
            "memguard_history",
            json!({ "hours": 1 }),
            &["hours", "total_samples", "step", "samples"],
        ),
    ];

    for (idx, (tool, args, keys)) in cases.iter().enumerate() {
        let (_notes, reply) = mcp
            .call_tool((idx + 10) as i64, tool, args.clone(), None, Duration::from_secs(15))
            .unwrap_or_else(|e| panic!("{} failed: {}", tool, e));
        let res = &reply["result"];
        assert_eq!(res["isError"], false, "{} returned error: {}", tool, reply);
        let sc = &res["structuredContent"];
        assert!(sc.is_object(), "{} structuredContent not object", tool);
        for k in *keys {
            assert!(sc.get(*k).is_some(), "{} missing key `{}` in {}", tool, k, sc);
        }
        let text = res["content"][0]["text"]
            .as_str()
            .expect("content[0].text string");
        let parsed: Value = serde_json::from_str(text).expect("content[0].text valid JSON");
        assert_eq!(&parsed, sc, "{} text/structuredContent mismatch", tool);
    }
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_prime_and_wait_satisfied_with_progress() {
    let env = EnvDir::new("f8_prime_wait");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let (_n, prime_r) = mcp
        .call_tool(
            2,
            "memguard_prime",
            json!({ "workload": "editor" }),
            None,
            Duration::from_secs(10),
        )
        .expect("prime");
    assert_eq!(prime_r["result"]["isError"], false, "{}", prime_r);

    let (_n, ready_r) = mcp
        .call_tool(
            3,
            "memguard_wait",
            json!({ "commit_gb": 0.001, "timeout_sec": 0 }),
            None,
            Duration::from_secs(10),
        )
        .expect("wait satisfied");
    assert_eq!(ready_r["result"]["isError"], false);
    assert_eq!(ready_r["result"]["structuredContent"]["ready"], true);

    let (notes, wait_r) = mcp
        .call_tool(
            4,
            "memguard_wait",
            json!({ "ram_gb": 1.0e9, "timeout_sec": 3 }),
            Some(json!({ "progressToken": "p1" })),
            Duration::from_secs(10),
        )
        .expect("wait timeout");
    assert_eq!(wait_r["result"]["isError"], false);
    assert_eq!(wait_r["result"]["structuredContent"]["ready"], false);
    assert!(
        !notes.is_empty(),
        "expected >= 1 notifications/progress during wait"
    );
    assert!(
        notes
            .iter()
            .all(|n| n["method"] == "notifications/progress" && n["params"]["progressToken"] == "p1"),
        "unexpected notifications: {:?}",
        notes
    );
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_disk_checkup_plan_clean_lifecycle() {
    let env = EnvDir::new("f8_disk_lifecycle");
    let proj_root = create_uproject_fixture(&env);
    let game_dir = proj_root.join("Game");

    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let (_n, chk_r) = mcp
        .call_tool(
            2,
            "memguard_disk_checkup",
            json!({ "path": proj_root.display().to_string(), "refresh": true, "budget_sec": 30 }),
            None,
            Duration::from_secs(15),
        )
        .expect("disk_checkup");
    assert_eq!(chk_r["result"]["isError"], false, "{}", chk_r);
    let items = chk_r["result"]["structuredContent"]["items"]
        .as_array()
        .expect("checkup items");
    let int_id = items
        .iter()
        .find(|i| i["key"] == "ue:intermediate")
        .and_then(|i| i["id"].as_str())
        .expect("ue:intermediate id")
        .to_string();

    let (_n, plan_r) = mcp
        .call_tool(
            3,
            "memguard_disk_plan",
            json!({ "ids": [int_id] }),
            None,
            Duration::from_secs(15),
        )
        .expect("disk_plan");
    assert_eq!(plan_r["result"]["isError"], false, "{}", plan_r);
    let plan_id = plan_r["result"]["structuredContent"]["plan_id"]
        .as_str()
        .expect("plan_id")
        .to_string();

    let (_n, clean_r) = mcp
        .call_tool(
            4,
            "memguard_disk_clean",
            json!({ "ids": [int_id], "plan_id": plan_id }),
            None,
            Duration::from_secs(20),
        )
        .expect("disk_clean");
    assert_eq!(clean_r["result"]["isError"], false, "{}", clean_r);
    let sc = &clean_r["result"]["structuredContent"];
    assert_eq!(sc["outcomes"][0]["result"], "done", "{}", sc);
    assert!(!game_dir.join("Intermediate").exists(), "Intermediate must be deleted");
    assert!(game_dir.join(r"Content\Hero.uasset").is_file());
    assert!(game_dir.join(r"Saved\Logs\Game.log").is_file());
    assert!(game_dir.join("Game.uproject").is_file());
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_new_m3_tools_readonly_schemas() {
    let env = EnvDir::new("f8_new_m3").with_env_key("f8_new_m3");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let tools = [
        ("memguard_drivers", &["total_gb", "tags_available"][..]),
        ("memguard_disk_relocate", &["store", "entries"][..]),
        ("memguard_disk_bin", &["runs", "hold_hours"][..]),
        ("memguard_config", &[][..]),
        ("memguard_session_summary", &[][..]),
    ];

    for (idx, (name, expected_keys)) in tools.iter().enumerate() {
        let (_n, reply) = mcp
            .call_tool((idx + 10) as i64, name, json!({}), None, Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("{} call failed: {}", name, e));
        assert!(
            reply.get("error").is_none(),
            "{} returned protocol error: {}",
            name,
            reply
        );
        let res = &reply["result"];
        assert_eq!(res["isError"], false, "{} returned tool error: {}", name, reply);
        let sc = &res["structuredContent"];
        assert!(sc.is_object(), "{} structuredContent not object: {}", name, res);
        for key in *expected_keys {
            assert!(
                sc.get(*key).is_some(),
                "{} structuredContent missing `{}`: {}",
                name,
                key,
                sc
            );
        }
        let text = res["content"][0]["text"]
            .as_str()
            .expect("content[0].text string");
        let parsed: Value = serde_json::from_str(text).expect("content[0].text JSON");
        assert_eq!(&parsed, sc, "{} text/structuredContent mismatch", name);
    }
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_notifications_cancelled_aborts_wait() {
    let env = EnvDir::new("f8_cancel");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    // Use a 15s wait so baseline never hangs for 60s+, while recv_timeout(5s) fails fast.
    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "tools/call",
        "params": {
            "name": "memguard_wait",
            "arguments": { "ram_gb": 1.0e9, "timeout_sec": 15 },
            "_meta": { "progressToken": "c42" }
        }
    }));

    let note = mcp
        .recv_notification("notifications/progress", Duration::from_secs(4))
        .expect("first progress notification before cancel");
    assert_eq!(note["params"]["progressToken"], "c42");

    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 42, "reason": "e2e cancel" }
    }));

    let (_extra_notes, cancel_reply) = mcp
        .recv_until_id(&json!(42), Duration::from_secs(5))
        .expect("cancelled wait must reply within 5s (not run full 15s)");

    // Accept JSON-RPC error, tool isError=true, or WaitResult { ready: false }.
    let aborted_cleanly = cancel_reply.get("error").is_some()
        || cancel_reply["result"]["isError"] == true
        || cancel_reply["result"]["structuredContent"]["ready"] == false;
    assert!(
        aborted_cleanly,
        "unexpected reply after notifications/cancelled: {}",
        cancel_reply
    );

    let (_n, advise_r) = mcp
        .call_tool(43, "memguard_advise", json!({}), None, Duration::from_secs(3))
        .expect("subsequent tool call after cancel");
    assert_eq!(advise_r["result"]["isError"], false);
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t1_f8_bom_blank_lines_and_silent_notifications() {
    let env = EnvDir::new("f8_bom_silent");
    let mut mcp = McpClient::spawn(&env);

    mcp.send_raw_line("\u{feff}{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}");
    mcp.send_raw_line("");
    mcp.send_raw_line("   ");
    mcp.send_json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    mcp.send_json(&json!({ "jsonrpc": "2.0", "method": "tools/list" }));
    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 99, "result": {} }));
    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 100, "method": "ping" }));

    let (_n1, r1) = mcp
        .recv_until_id(&json!(1), Duration::from_secs(5))
        .expect("BOM ping reply");
    assert_eq!(r1["result"], json!({}));

    let (notes2, r2) = mcp
        .recv_until_id(&json!(100), Duration::from_secs(5))
        .expect("second ping reply");
    assert!(notes2.is_empty(), "unexpected notifications: {:?}", notes2);
    assert_eq!(r2["result"], json!({}));
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_protocol_error_codes_and_recovery() {
    let env = EnvDir::new("f8_proto_errors");
    let mut mcp = McpClient::spawn(&env);

    mcp.send_raw_line("{not valid json");
    let parse_err = mcp.recv_msg(Duration::from_secs(5)).expect("parse error");
    assert_eq!(parse_err["id"], Value::Null);
    assert_eq!(parse_err["error"]["code"], -32700);

    mcp.send_raw_line("[1, 2]");
    let batch_err = mcp.recv_msg(Duration::from_secs(5)).expect("batch invalid request");
    assert_eq!(batch_err["error"]["code"], -32600);

    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 2 }));
    let (_n, missing_method) = mcp
        .recv_until_id(&json!(2), Duration::from_secs(5))
        .expect("missing method");
    assert_eq!(missing_method["error"]["code"], -32600);

    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }));
    let (_n, unknown_method) = mcp
        .recv_until_id(&json!(3), Duration::from_secs(5))
        .expect("unknown method");
    assert_eq!(unknown_method["error"]["code"], -32601);

    for (idx, bad_req) in [
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "arguments": {} } }),
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "memguard_nonexistent", "arguments": {} } }),
        json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "memguard_status", "arguments": [1] } }),
    ]
    .into_iter()
    .enumerate()
    {
        mcp.send_json(&bad_req);
        let id = (idx + 4) as i64;
        let (_n, reply) = mcp
            .recv_until_id(&json!(id), Duration::from_secs(5))
            .expect("invalid params");
        assert_eq!(reply["error"]["code"], -32602, "{}", reply);
    }

    // Server recovers cleanly after protocol errors.
    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }));
    let (_n, ping_r) = mcp
        .recv_until_id(&json!(7), Duration::from_secs(5))
        .expect("ping recovery");
    assert_eq!(ping_r["result"], json!({}));
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_tool_argument_validation_and_unknown_keys() {
    let env = EnvDir::new("f8_tool_args");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let bad_calls: &[(&str, Value)] = &[
        ("memguard_status", json!({ "verbose": true })),
        ("memguard_disk", json!({ "clean": true })),
        ("memguard_prime", json!({ "workload": "shaders" })),
        ("memguard_prime", json!({ "kill_orphans": "yes" })),
        ("memguard_wait", json!({})),
        ("memguard_wait", json!({ "workload": "build", "timeout_sec": -5 })),
        ("memguard_wait", json!({ "workload": "build", "timout_sec": 5 })),
        (
            "memguard_wait",
            json!({ "disk_gb": 10, "disk_path": r"Q:\definitely\missing\amg_e2e" }),
        ),
        ("memguard_disk_checkup", json!({ "budget_sec": 0 })),
    ];

    for (idx, (name, args)) in bad_calls.iter().enumerate() {
        let (_n, reply) = mcp
            .call_tool((idx + 10) as i64, name, args.clone(), None, Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{} failed: {}", name, e));
        assert!(
            reply.get("error").is_none(),
            "{} should return tool error not protocol error: {}",
            name,
            reply
        );
        assert_eq!(
            reply["result"]["isError"], true,
            "{} with {:?} should be isError: true: {}",
            name, args, reply
        );
    }
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_disk_clean_rejects_missing_forged_and_disposable_plans() {
    let env = EnvDir::new("f8_clean_forged");
    let proj_root = create_uproject_fixture(&env);
    let game_dir = proj_root.join("Game");

    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let (_n, chk_r) = mcp
        .call_tool(
            2,
            "memguard_disk_checkup",
            json!({ "path": proj_root.display().to_string(), "refresh": true, "budget_sec": 30 }),
            None,
            Duration::from_secs(15),
        )
        .expect("checkup");
    let items = chk_r["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items");
    let logs_id = items
        .iter()
        .find(|i| i["key"] == "ue:logs")
        .and_then(|i| i["id"].as_str())
        .expect("ue:logs id")
        .to_string();

    let (_n, missing_pid) = mcp
        .call_tool(
            3,
            "memguard_disk_clean",
            json!({ "ids": [&logs_id] }),
            None,
            Duration::from_secs(5),
        )
        .expect("missing plan_id");
    assert_eq!(missing_pid["result"]["isError"], true);

    let (_n, unknown_pid) = mcp
        .call_tool(
            4,
            "memguard_disk_clean",
            json!({ "ids": [&logs_id], "plan_id": "p_nonexistent" }),
            None,
            Duration::from_secs(5),
        )
        .expect("unknown plan_id");
    assert_eq!(unknown_pid["result"]["isError"], true);

    let (_n, plan_r) = mcp
        .call_tool(
            5,
            "memguard_disk_plan",
            json!({ "ids": [&logs_id] }),
            None,
            Duration::from_secs(15),
        )
        .expect("plan");
    let real_plan_id = plan_r["result"]["structuredContent"]["plan_id"]
        .as_str()
        .expect("plan_id")
        .to_string();

    // Forge a plan in disk-plans.json with id="pfile" and class="regenerable".
    let plans_path = env.dir.join("disk-plans.json");
    if let Ok(text) = fs::read_to_string(&plans_path) {
        if let Ok(mut arr) = serde_json::from_str::<Vec<Value>>(&text) {
            if let Some(mut forged) = arr.last().cloned() {
                forged["id"] = json!("pfile");
                if let Some(items_arr) = forged["items"].as_array_mut() {
                    for it in items_arr {
                        it["class"] = json!("regenerable");
                    }
                }
                arr.push(forged);
                let _ = fs::write(&plans_path, serde_json::to_string(&arr).unwrap());
            }
        }
    }

    let (_n, forged_r) = mcp
        .call_tool(
            6,
            "memguard_disk_clean",
            json!({ "ids": [&logs_id], "plan_id": "pfile" }),
            None,
            Duration::from_secs(5),
        )
        .expect("forged plan_id");
    assert_eq!(forged_r["result"]["isError"], true, "{}", forged_r);

    // Disposable Saved\Logs must be refused even with real plan_id.
    let (_n, disp_r) = mcp
        .call_tool(
            7,
            "memguard_disk_clean",
            json!({ "ids": [&logs_id], "plan_id": real_plan_id }),
            None,
            Duration::from_secs(20),
        )
        .expect("disposable clean");
    assert_eq!(disp_r["result"]["isError"], false, "{}", disp_r);
    assert_eq!(
        disp_r["result"]["structuredContent"]["outcomes"][0]["result"],
        "refused",
        "{}",
        disp_r
    );
    assert!(game_dir.join(r"Saved\Logs\Game.log").is_file());
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_disk_clean_refuses_recent_writes_and_modified_after_plan() {
    let env = EnvDir::new("f8_clean_recent");
    let proj_root = create_uproject_fixture(&env);
    let game_dir = proj_root.join("Game");

    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let (_n, chk_r) = mcp
        .call_tool(
            2,
            "memguard_disk_checkup",
            json!({ "path": proj_root.display().to_string(), "refresh": true, "budget_sec": 30 }),
            None,
            Duration::from_secs(15),
        )
        .expect("checkup");
    let int_id = chk_r["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["key"] == "ue:intermediate")
        .and_then(|i| i["id"].as_str())
        .expect("intermediate id")
        .to_string();

    let (_n, plan_r) = mcp
        .call_tool(
            3,
            "memguard_disk_plan",
            json!({ "ids": [&int_id] }),
            None,
            Duration::from_secs(15),
        )
        .expect("plan");
    let plan_id = plan_r["result"]["structuredContent"]["plan_id"]
        .as_str()
        .expect("plan_id")
        .to_string();

    // Recent write -> refused ("last 2 hours").
    fs::write(game_dir.join(r"Intermediate\Build\fresh.txt"), b"now").expect("write fresh.txt");
    let (_n, recent_r) = mcp
        .call_tool(
            4,
            "memguard_disk_clean",
            json!({ "ids": [&int_id], "plan_id": &plan_id }),
            None,
            Duration::from_secs(20),
        )
        .expect("clean after recent write");
    assert_eq!(recent_r["result"]["structuredContent"]["outcomes"][0]["result"], "refused");
    let msg = recent_r["result"]["structuredContent"]["outcomes"][0]["message"]
        .as_str()
        .unwrap_or("");
    assert!(msg.contains("last 2 hours"), "expected `last 2 hours` refusal, got: {}", msg);
    assert!(game_dir.join(r"Intermediate\Build\Win64\a.obj").is_file());

    // Re-age Intermediate to 2.5h ago (> 2h old, yet newer than the 3h plan).
    age_tree(&game_dir.join("Intermediate"), 2 * 3600 + 1800);
    let (_n, after_plan_r) = mcp
        .call_tool(
            5,
            "memguard_disk_clean",
            json!({ "ids": [&int_id], "plan_id": &plan_id }),
            None,
            Duration::from_secs(20),
        )
        .expect("clean after plan modified");
    assert_eq!(
        after_plan_r["result"]["structuredContent"]["outcomes"][0]["result"],
        "refused"
    );
    let msg2 = after_plan_r["result"]["structuredContent"]["outcomes"][0]["message"]
        .as_str()
        .unwrap_or("");
    assert!(
        msg2.contains("after the plan"),
        "expected `after the plan` refusal, got: {}",
        msg2
    );
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_history_bounds_and_clamping() {
    let env = EnvDir::new("f8_history_bounds");
    let st = env.status_json();
    let time_str = st["time"].as_str().expect("status.time");
    let date_part = &time_str[..10];
    let hist_dir = env.dir.join("history");
    fs::create_dir_all(&hist_dir).expect("create history dir");

    // Write 900 valid CSV rows matching src/history.rs format for today's local date.
    let mut csv = String::from(
        "time,mode,pressure,ram_free_gb,commit_free_gb,ue_committed_gb,editor_committed_gb,workers,Browsers\n",
    );
    for _ in 0..900u64 {
        csv.push_str(&format!(
            "{},idle,normal,16.00,20.00,0.00,0.00,0,1.50\n",
            time_str
        ));
    }
    fs::write(hist_dir.join(format!("{}.csv", date_part)), csv).expect("write history csv");

    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    for (idx, bad) in [
        json!({ "hours": 0 }),
        json!({ "hours": -1 }),
        json!({ "hours": "bad" }),
        json!({ "days": 1 }),
    ]
    .into_iter()
    .enumerate()
    {
        let (_n, r) = mcp
            .call_tool((idx + 2) as i64, "memguard_history", bad.clone(), None, Duration::from_secs(5))
            .expect("bad history call");
        assert_eq!(r["result"]["isError"], true, "expected error for {:?}", bad);
    }

    let (_n, r) = mcp
        .call_tool(10, "memguard_history", json!({ "hours": 500 }), None, Duration::from_secs(5))
        .expect("clamped history call");
    assert_eq!(r["result"]["isError"], false, "{}", r);
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["hours"], 168.0);
    assert_eq!(sc["total_samples"], 900);
    let samples = sc["samples"].as_array().expect("samples array");
    assert!(samples.len() <= 720, "expected <= 720 thinned samples, got {}", samples.len());
    assert!(sc["step"].as_u64().unwrap_or(0) >= 2, "expected step >= 2: {}", sc);
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_wait_blocked_by_disk_returns_immediately() {
    let env = EnvDir::new("f8_wait_disk");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    let started = Instant::now();
    let (_n, r) = mcp
        .call_tool(
            2,
            "memguard_wait",
            json!({
                "disk_gb": 1.0e9,
                "disk_path": env.dir.display().to_string(),
                "prime": true,
                "timeout_sec": 30
            }),
            None,
            Duration::from_secs(5),
        )
        .expect("wait disk block");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(r["result"]["isError"], false, "{}", r);
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["ready"], false);
    assert_eq!(sc["blocked_by"], json!(["disk"]));
    assert_eq!(sc["waited_sec"], 0);
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t2_f8_cancel_unknown_or_already_completed_request_is_safe() {
    let env = EnvDir::new("f8_cancel_safe");
    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1, "2025-06-18");

    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 99999 }
    }));
    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 1 }
    }));
    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));

    let (notes, r) = mcp
        .recv_until_id(&json!(2), Duration::from_secs(5))
        .expect("ping after safe cancels");
    assert!(notes.is_empty());
    assert_eq!(r["result"], json!({}));
    assert_eq!(mcp.close_and_wait(), 0);
}

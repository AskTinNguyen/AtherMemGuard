//! Opaque-box E2E tests for:
//! - Feature Area F9: Packaging, Feature Gates & Static Invariants (6 Tier 1 + 6 Tier 2)
//! - Tier 3: Pairwise Cross-Feature Combinations (10 tests: `t3_*`)
//! - Tier 4: Real-World Developer Workflows (6 tests: `t4_*`)
//!
//! Strictly uses only `std`, `serde_json`, `env!("CARGO_BIN_EXE_memguard")`, and
//! `env!("CARGO_MANIFEST_DIR")`. Never imports `athermemguard::*`, never mutates
//! process-global env vars, and wraps every artifact in RAII `Drop` guards.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MEMGUARD_BIN: &str = env!("CARGO_BIN_EXE_memguard");
const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

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
        .expect("spawn child command");

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

struct TestDir {
    dir: PathBuf,
    env_key: Option<String>,
}

impl TestDir {
    fn new(tag: &str) -> Self {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("amg_e2e_scen_{}_{}_{}", std::process::id(), id, tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create TestDir");
        Self { dir, env_key: None }
    }

    fn with_env_key(mut self, tag: &str) -> Self {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        self.env_key = Some(format!(
            r"Software\AtherMemGuard-test\e2e_scen_{}_{}_{}",
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

    fn run_cmd(&self, cmd: Command) -> Output {
        run_with_timeout(&self.dir, cmd, Duration::from_secs(30))
    }

    fn run_output(&self, args: &[&str]) -> Output {
        self.run_cmd(self.cmd(args))
    }

    fn status_json(&self) -> Value {
        let out = self.run_output(&["status", "--json"]);
        assert!(
            out.status.success(),
            "status --json failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("parse status --json")
    }

    fn write_settings(&self, toml_text: &str) {
        fs::write(self.dir.join("settings.toml"), toml_text).expect("write settings.toml");
    }
}

impl Drop for TestDir {
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

struct KillChild(Child);

impl KillChild {
    fn is_alive(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }

    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        matches!(self.0.try_wait(), Ok(Some(_)))
    }
}

impl Drop for KillChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TaskkillByName(String);

impl Drop for TaskkillByName {
    fn drop(&mut self) {
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", &self.0, "/T"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        std::thread::sleep(Duration::from_millis(150));
    }
}

struct TempFolderGuard(PathBuf);

impl Drop for TempFolderGuard {
    fn drop(&mut self) {
        for _ in 0..10 {
            if !self.0.exists() || fs::remove_dir_all(&self.0).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct BinCleanupGuard<'a> {
    env: &'a TestDir,
    before_ids: BTreeSet<String>,
    tracked: Vec<(String, String, String)>, // (vol_root, run_id, undo_id)
}

impl<'a> BinCleanupGuard<'a> {
    fn with_baseline(env: &'a TestDir, before_ids: BTreeSet<String>) -> Self {
        Self {
            env,
            before_ids,
            tracked: Vec::new(),
        }
    }

    fn track(&mut self, vol_root: String, run_id: String, undo_id: String) {
        self.tracked.push((vol_root, run_id, undo_id));
    }
}

impl Drop for BinCleanupGuard<'_> {
    fn drop(&mut self) {
        for (vol_root, run_id, undo_id) in &self.tracked {
            let _ = self.env.run_output(&["disk", "undo", undo_id, "--json"]);
            let bin_dir = Path::new(vol_root).join(".AtherMemGuard-bin");
            let run_dir = bin_dir.join(run_id);
            if run_dir.exists() {
                let _ = fs::remove_dir_all(&run_dir);
            }
            // Only removes .AtherMemGuard-bin if completely empty.
            let _ = fs::remove_dir(&bin_dir);
        }

        let bin_out = self.env.run_output(&["disk", "bin", "--json"]);
        if bin_out.status.success() {
            if let Ok(val) = serde_json::from_slice::<Value>(&bin_out.stdout) {
                if let Some(runs) = val["runs"].as_array() {
                    for run in runs {
                        let Some(run_id) = run["run_id"].as_str() else {
                            continue;
                        };
                        if self.before_ids.contains(run_id) {
                            continue;
                        }
                        let _ = self.env.run_output(&["disk", "undo", run_id, "--json"]);
                        if let Some(vol_root) = run["volume_root"].as_str() {
                            let bin_dir = Path::new(vol_root).join(".AtherMemGuard-bin");
                            let run_dir = bin_dir.join(run_id);
                            if run_dir.exists() {
                                let _ = fs::remove_dir_all(&run_dir);
                            }
                            // Only removes .AtherMemGuard-bin if completely empty.
                            let _ = fs::remove_dir(&bin_dir);
                        }
                    }
                }
            }
        }
    }
}

struct DaemonGuard<'a> {
    _lock: MutexGuard<'static, ()>,
    env: &'a TestDir,
    child: Option<Child>,
    ticks: Arc<AtomicUsize>,
}

impl<'a> DaemonGuard<'a> {
    fn start(env: &'a TestDir) -> Self {
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

        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_bg = Arc::clone(&ticks);
        let (tx, rx) = mpsc::channel::<Result<String, String>>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(Err("daemon exited before output".into()));
                }
                Ok(_) => {
                    ticks_bg.fetch_add(1, Ordering::Relaxed);
                    let _ = tx.send(Ok(line));
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
            let mut sink = String::new();
            while reader.read_line(&mut sink).unwrap_or(0) > 0 {
                if !sink.trim().is_empty() {
                    ticks_bg.fetch_add(1, Ordering::Relaxed);
                }
                sink.clear();
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

        let guard = Self {
            _lock: lock,
            env,
            child: Some(child),
            ticks,
        };

        match rx.recv_timeout(Duration::from_secs(12)) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                let err = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
                panic!("daemon failed to start ({}); stderr: {}", e, err);
            }
            Err(_) => {
                let err = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
                panic!("daemon first tick timeout; stderr: {}", err);
            }
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while !env.dir.join("status.json").exists() {
            assert!(Instant::now() < deadline, "status.json missing");
            std::thread::sleep(Duration::from_millis(50));
        }
        guard
    }

    fn tick_count(&self) -> usize {
        self.ticks.load(Ordering::Relaxed)
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
    fn spawn(env: &TestDir) -> Self {
        let mut child = env
            .cmd(&["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn memguard mcp");
        let stdin = BufWriter::new(child.stdin.take().expect("stdin"));
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

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

    fn send_json(&mut self, v: &Value) {
        let w = self.stdin.as_mut().expect("stdin open");
        let s = serde_json::to_string(v).expect("serialize JSON");
        w.write_all(s.as_bytes()).expect("write");
        w.write_all(b"\n").expect("newline");
        w.flush().expect("flush");
    }

    fn recv_until_id(&self, target_id: &Value, timeout: Duration) -> Result<(Vec<Value>, Value), String> {
        let deadline = Instant::now() + timeout;
        let mut notes = Vec::new();
        loop {
            let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                return Err(format!(
                    "MCP timeout waiting for id={}; stderr={}",
                    target_id,
                    self.stderr_buf.lock().map(|b| b.clone()).unwrap_or_default()
                ));
            };
            if rem.is_zero() {
                return Err(format!("MCP timeout waiting for id={}", target_id));
            }
            let line = self
                .rx
                .recv_timeout(rem)
                .map_err(|e| format!("MCP recv_timeout ({}): {}", target_id, e))?
                .map_err(|e| format!("MCP stdout err: {}", e))?;
            let msg: Value = serde_json::from_str(&line).map_err(|e| format!("bad JSON: {}", e))?;
            if msg.get("id").is_none() {
                notes.push(msg);
                continue;
            }
            if msg.get("id") == Some(target_id) {
                return Ok((notes, msg));
            }
            return Err(format!("unexpected MCP id (wanted {}): {}", target_id, msg));
        }
    }

    fn initialize(&mut self, id: i64) -> Value {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e_scen", "version": "1.0" }
            }
        }));
        let (_n, r) = self
            .recv_until_id(&json!(id), Duration::from_secs(5))
            .expect("initialize");
        self.send_json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        r
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

fn copy_sys_exe(dir: &Path, rel: &str, stem: &str) -> PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let src = PathBuf::from(windir).join(rel);
    let dst = dir.join(format!("{}.exe", stem));
    if !dst.exists() {
        fs::copy(&src, &dst).expect("copy system exe");
    }
    dst
}

fn build_dummy_gui_exe(dir: &Path, stem: &str) -> PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let csc = PathBuf::from(windir).join(r"Microsoft.NET\Framework64\v4.0.30319\csc.exe");
    let cs = dir.join(format!("{}.cs", stem));
    let exe = dir.join(format!("{}.exe", stem));
    fs::write(
        &cs,
        format!(
            "using System;\nusing System.Windows.Forms;\npublic class Program {{\n    [STAThread]\n    public static void Main() {{\n        var f = new Form();\n        f.Text = \"{}\";\n        f.WindowState = FormWindowState.Minimized;\n        Application.Run(f);\n    }}\n}}\n",
            stem
        ),
    )
    .expect("write dummy gui cs");
    let mut csc_cmd = Command::new(&csc);
    csc_cmd
        .args([
            "/nologo",
            "/target:winexe",
            "/r:System.Windows.Forms.dll",
            "/r:System.Drawing.dll",
            &format!("/out:{}", exe.display()),
            &cs.display().to_string(),
        ])
        .creation_flags(CREATE_NO_WINDOW);
    let out = run_with_timeout(dir, csc_cmd, Duration::from_secs(30));
    assert!(
        out.status.success(),
        "csc.exe failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    exe
}

fn spawn_ping_exe(dir: &Path, stem: &str) -> KillChild {
    let exe = copy_sys_exe(dir, r"System32\PING.EXE", stem);
    let child = Command::new(&exe)
        .args(["-t", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("spawn dummy ping");
    KillChild(child)
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

fn run_git(repo: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW);
    let out = run_with_timeout(repo, cmd, Duration::from_secs(30));
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

// ============================================================================
// Pure-std Rust Lexer & Static Parsers (Feature Area F9)
// ============================================================================

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn prev_char(src: &str, i: usize) -> Option<char> {
    src[..i].chars().next_back()
}

fn prev_is_ident(src: &str, bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let b = bytes[i - 1];
    if b < 0x80 {
        is_ident_continue(b)
    } else {
        prev_char(src, i).is_some_and(|c| c.is_alphanumeric() || c == '_')
    }
}

/// Scans Rust source for standalone identifier tokens equal to `target`,
/// skipping comments (`//`, nested `/* */`), string/byte/C/raw literals,
/// char/byte-char literals (including `'"'`, `'\\'`, `b'\\'`), raw identifiers
/// (`r#ident`), and lifetimes/labels (`'a`, `'outer`). Returns 1-based line numbers.
fn scan_ident_tokens(src: &str, target: &str) -> Vec<usize> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut hits = Vec::new();

    while i < len {
        let b = bytes[i];

        // 1. Newline
        if b == b'\n' {
            line += 1;
            i += 1;
            continue;
        }

        // 2. Line comment
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // 3. Nested block comment
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < len && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }

        // 4. Raw strings & raw identifiers (`r"..."`, `r#"..."#`, `br"..."`, `cr"..."`, `r#ident`)
        let raw_prefix = if b == b'r' && !prev_is_ident(src, bytes, i) {
            Some(1usize)
        } else if (b == b'b' || b == b'c')
            && bytes.get(i + 1) == Some(&b'r')
            && !prev_is_ident(src, bytes, i)
        {
            Some(2usize)
        } else {
            None
        };
        if let Some(p) = raw_prefix {
            let mut j = i + p;
            let mut hashes = 0usize;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                i = j + 1;
                while i < len {
                    if bytes[i] == b'\n' {
                        line += 1;
                        i += 1;
                    } else if bytes[i] == b'"' {
                        let mut matched = true;
                        for k in 0..hashes {
                            if bytes.get(i + 1 + k) != Some(&b'#') {
                                matched = false;
                                break;
                            }
                        }
                        if matched {
                            i += 1 + hashes;
                            break;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                continue;
            } else if p == 1 && hashes == 1 && bytes.get(j).is_some_and(|&c| is_ident_start(c)) {
                // Raw identifier `r#ident` — never matches keyword `unsafe`.
                i = j + 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                continue;
            }
        }

        // 5. Normal / Byte / C strings (`"..."`, `b"..."`, `c"..."`)
        let is_str_start = b == b'"'
            || (!prev_is_ident(src, bytes, i)
                && (b == b'b' || b == b'c')
                && bytes.get(i + 1) == Some(&b'"'));
        if is_str_start {
            i += if b == b'"' { 1 } else { 2 };
            while i < len {
                if bytes[i] == b'\\' {
                    if i + 1 < len {
                        if bytes[i + 1] == b'\n' {
                            line += 1;
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }

        // 6. Byte char literals (`b'...'`), char literals (`'c'`), vs lifetimes/labels (`'a`)
        if !prev_is_ident(src, bytes, i) && b == b'b' && bytes.get(i + 1) == Some(&b'\'') {
            i += 1;
        }
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\\') {
                let mut j = i + 2;
                if j < len {
                    if bytes[j] == b'\n' {
                        line += 1;
                    }
                    j += 1;
                }
                while j < len && bytes[j] != b'\'' && bytes[j] != b'\n' {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'\'') {
                    j += 1;
                }
                i = j;
                continue;
            } else if let Some(ch) = src[i + 1..].chars().next() {
                let next_idx = i + 1 + ch.len_utf8();
                if ch != '\''
                    && ch != '\\'
                    && ch != '\n'
                    && ch != '\r'
                    && bytes.get(next_idx) == Some(&b'\'')
                {
                    i = next_idx + 1;
                    continue;
                }
            }
            // Otherwise lifetime or loop label (`'a`, `'outer`, `'r#ident`).
            i += 1;
            if bytes.get(i) == Some(&b'r')
                && bytes.get(i + 1) == Some(&b'#')
                && bytes.get(i + 2).is_some_and(|&c| is_ident_start(c))
            {
                i += 2;
            }
            if bytes.get(i).is_some_and(|&c| is_ident_start(c)) {
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
            }
            continue;
        }

        // 7. Identifier tokens
        if is_ident_start(b) {
            let prev_unicode_ident = prev_is_ident(src, bytes, i);
            let start = i;
            i += 1;
            while i < len && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let next_unicode_ident = src[i..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if !prev_unicode_ident && !next_unicode_ident && &src[start..i] == target {
                hits.push(line);
            }
            continue;
        }

        // Advance past non-ASCII UTF-8 char or single ASCII byte.
        if b >= 0x80 {
            i += src[i..].chars().next().map_or(1, char::len_utf8);
        } else {
            i += 1;
        }
    }

    hits
}

fn is_allowlisted_unsafe_path(rel_path: &str) -> bool {
    let norm = rel_path.replace('\\', "/");
    let allow_exact = [
        "src/win.rs",
        "src/tray.rs",
        "src/logo.rs",
        "src/net.rs",
        "src/doctor_win.rs",
        "src/smbios.rs",
        "src/startup.rs",
        "src/disk_win.rs",
    ];
    allow_exact.contains(&norm.as_str()) || norm.starts_with("src/ui/")
}

fn strip_toml_comment(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_dq = false;
    let mut in_sq = false;
    let mut esc = false;
    for ch in line.chars() {
        if in_dq {
            out.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_dq = false;
            }
        } else if in_sq {
            out.push(ch);
            if ch == '\'' {
                in_sq = false;
            }
        } else if ch == '#' {
            break;
        } else {
            if ch == '"' {
                in_dq = true;
            } else if ch == '\'' {
                in_sq = true;
            }
            out.push(ch);
        }
    }
    out
}

fn parse_toml_string_list(val: &str) -> Result<Vec<String>, String> {
    let t = val.trim();
    if !t.starts_with('[') || !t.ends_with(']') {
        return Err(format!("expected TOML array, got `{}`", t));
    }
    let inner = &t[1..t.len() - 1];
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut in_dq = false;
    let mut in_sq = false;
    let mut esc = false;
    for ch in inner.chars() {
        if in_dq {
            if esc {
                cur.push(ch);
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_dq = false;
            } else {
                cur.push(ch);
            }
        } else if in_sq {
            if ch == '\'' {
                in_sq = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_dq = true;
        } else if ch == '\'' {
            in_sq = true;
        } else if ch == ',' {
            let s = cur.trim();
            if !s.is_empty() {
                items.push(s.to_string());
            }
            cur.clear();
        } else if !ch.is_whitespace() {
            cur.push(ch);
        }
    }
    let s = cur.trim();
    if !s.is_empty() {
        items.push(s.to_string());
    }
    Ok(items)
}

fn unquote_toml_str(val: &str) -> Option<String> {
    let t = val.trim();
    if t.len() >= 2 && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\''))) {
        Some(t[1..t.len() - 1].to_string())
    } else {
        None
    }
}

#[derive(Default)]
struct TomlDoc {
    tables: BTreeMap<String, BTreeMap<String, String>>,
    arrays: BTreeMap<String, Vec<BTreeMap<String, String>>>,
}

fn parse_simple_toml(src: &str) -> Result<TomlDoc, String> {
    if src.trim().is_empty() {
        return Err("empty TOML".into());
    }
    let mut doc = TomlDoc::default();
    let mut cur_section: Option<(bool, String, usize)> = None; // (is_array, name, idx)
    let mut pending = String::new();
    let mut bracket_depth: i32 = 0;

    for raw_line in src.lines() {
        let stripped = strip_toml_comment(raw_line);
        let trimmed = stripped.trim();
        if trimmed.is_empty() && pending.is_empty() {
            continue;
        }

        if pending.is_empty() && trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            let name = trimmed[2..trimmed.len() - 2].trim().to_string();
            if name.is_empty() {
                return Err("empty [[table]] header".into());
            }
            let list = doc.arrays.entry(name.clone()).or_default();
            list.push(BTreeMap::new());
            cur_section = Some((true, name, list.len() - 1));
            continue;
        }
        if pending.is_empty() && trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim().to_string();
            if name.is_empty() {
                return Err("empty [table] header".into());
            }
            doc.tables.entry(name.clone()).or_default();
            cur_section = Some((false, name, 0));
            continue;
        }

        if !pending.is_empty() {
            pending.push(' ');
        }
        pending.push_str(trimmed);

        // Count brackets outside quotes to handle multi-line arrays cleanly.
        let mut in_dq = false;
        let mut in_sq = false;
        let mut esc = false;
        bracket_depth = 0;
        for ch in pending.chars() {
            if in_dq {
                if esc {
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else if ch == '"' {
                    in_dq = false;
                }
            } else if in_sq {
                if ch == '\'' {
                    in_sq = false;
                }
            } else if ch == '"' {
                in_dq = true;
            } else if ch == '\'' {
                in_sq = true;
            } else if ch == '[' {
                bracket_depth += 1;
            } else if ch == ']' {
                bracket_depth -= 1;
            }
        }
        if bracket_depth > 0 {
            continue;
        }

        let stmt = std::mem::take(&mut pending);
        let Some((k, v)) = stmt.split_once('=') else {
            return Err(format!("malformed TOML line: `{}`", stmt));
        };
        let key = k.trim().to_string();
        let val = v.trim().to_string();
        if key.is_empty() || val.is_empty() {
            return Err(format!("invalid key/value `{}`", stmt));
        }
        match &cur_section {
            Some((true, name, idx)) => {
                if let Some(m) = doc.arrays.get_mut(name).and_then(|vec| vec.get_mut(*idx)) {
                    m.insert(key, val);
                }
            }
            Some((false, name, _)) => {
                doc.tables.entry(name.clone()).or_default().insert(key, val);
            }
            None => {
                doc.tables.entry(String::new()).or_default().insert(key, val);
            }
        }
    }
    if bracket_depth != 0 || !pending.is_empty() {
        return Err("unclosed TOML array/statement".into());
    }
    Ok(doc)
}

fn validate_cargo_toml(src: &str) -> Result<(), String> {
    let doc = parse_simple_toml(src)?;

    let prof = doc
        .tables
        .get("profile.release")
        .ok_or_else(|| "missing [profile.release]".to_string())?;
    let panic_val = prof
        .get("panic")
        .and_then(|v| unquote_toml_str(v))
        .ok_or_else(|| "[profile.release] missing `panic`".to_string())?;
    if panic_val != "abort" {
        return Err(format!("[profile.release].panic must be \"abort\", got `{}`", panic_val));
    }

    let feats = doc
        .tables
        .get("features")
        .ok_or_else(|| "missing [features]".to_string())?;
    for req_feat in ["default", "gui", "ui-snapshots", "embed-cli"] {
        if !feats.contains_key(req_feat) {
            return Err(format!("[features] missing `{}`", req_feat));
        }
    }
    let default_list = parse_toml_string_list(&feats["default"])?;
    if !default_list.iter().any(|x| x == "gui") {
        return Err("[features].default must include \"gui\"".into());
    }
    let snap_list = parse_toml_string_list(&feats["ui-snapshots"])?;
    if !snap_list.iter().any(|x| x == "gui") {
        return Err("[features].ui-snapshots must include \"gui\"".into());
    }

    for (sec, kv) in &doc.tables {
        let is_winreg_subtable = matches!(
            sec.as_str(),
            "dependencies.winreg" | "dev-dependencies.winreg" | "build-dependencies.winreg"
        ) || (sec.starts_with("target.")
            && (sec.ends_with(".dependencies.winreg")
                || sec.ends_with(".dev-dependencies.winreg")
                || sec.ends_with(".build-dependencies.winreg")));
        if is_winreg_subtable {
            return Err(format!("forbidden dependency table [{}]", sec));
        }

        let is_dep_sec = matches!(
            sec.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) || (sec.starts_with("target.")
            && (sec.ends_with(".dependencies")
                || sec.ends_with(".dev-dependencies")
                || sec.ends_with(".build-dependencies")));
        if is_dep_sec && kv.keys().any(|k| k == "winreg" || k.starts_with("winreg.")) {
            return Err(format!("forbidden dependency `winreg` in [{}]", sec));
        }
    }

    let bins = doc
        .arrays
        .get("bin")
        .ok_or_else(|| "missing [[bin]] sections".to_string())?;
    let gui_bin = bins
        .iter()
        .find(|b| b.get("name").and_then(|v| unquote_toml_str(v)).as_deref() == Some("AtherMemGuard"))
        .ok_or_else(|| "missing [[bin]] name = \"AtherMemGuard\"".to_string())?;
    let req_feats = gui_bin
        .get("required-features")
        .ok_or_else(|| "[[bin]] AtherMemGuard missing `required-features`".to_string())?;
    let req_list = parse_toml_string_list(req_feats)?;
    if !req_list.iter().any(|x| x == "gui") {
        return Err("[[bin]] AtherMemGuard `required-features` must include \"gui\"".into());
    }

    Ok(())
}

fn validate_cargo_lock(src: &str) -> Result<(), String> {
    let doc = parse_simple_toml(src)?;
    let pkgs = doc
        .arrays
        .get("package")
        .ok_or_else(|| "Cargo.lock has no [[package]] entries".to_string())?;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for p in pkgs {
        let name = p
            .get("name")
            .and_then(|v| unquote_toml_str(v))
            .ok_or_else(|| "Cargo.lock [[package]] missing name".to_string())?;
        let version = p
            .get("version")
            .and_then(|v| unquote_toml_str(v))
            .ok_or_else(|| format!("Cargo.lock [[package]] `{}` missing version", name))?;
        if name == "winreg" {
            return Err("Cargo.lock still contains `winreg`".into());
        }
        if name == "windows-sys" && (version == "0.48.0" || version == "0.61.2") {
            return Err(format!("Cargo.lock contains forbidden windows-sys {}", version));
        }
        seen.insert((name, version));
    }
    if !seen.contains(&("windows-sys".into(), "0.59.0".into())) {
        return Err("Cargo.lock missing expected windows-sys 0.59.0".into());
    }
    Ok(())
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_rs_files(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
}

// ============================================================================
// Feature Area F9: Packaging, Feature Gates & Static Invariants (6 T1 + 6 T2)
// ============================================================================

#[test]
fn t1_f9_unsafe_module_boundary_allowlist() {
    let manifest = Path::new(MANIFEST_DIR);
    let mut files = Vec::new();
    collect_rs_files(&manifest.join("src"), &mut files);
    assert!(!files.is_empty(), "expected .rs files under src/");

    let mut win_hits = 0usize;
    let mut disk_win_hits = 0usize;
    let mut violations = Vec::new();

    for path in files {
        let rel = path
            .strip_prefix(manifest)
            .expect("strip manifest prefix")
            .display()
            .to_string()
            .replace('\\', "/");
        let src = fs::read_to_string(&path).expect("read .rs source");
        let lines = scan_ident_tokens(&src, "unsafe");
        if rel == "src/disk_clean.rs" {
            assert!(
                lines.is_empty(),
                "src/disk_clean.rs comment-only `unsafe` should yield 0 hits, got {:?}",
                lines
            );
        }
        if rel == "src/win.rs" {
            win_hits = lines.len();
        }
        if rel == "src/disk_win.rs" {
            disk_win_hits = lines.len();
        }
        if !lines.is_empty() && !is_allowlisted_unsafe_path(&rel) {
            violations.push(format!("{}: lines {:?}", rel, lines));
        }
    }

    assert!(win_hits > 0, "anti-vacuous: expected unsafe tokens in src/win.rs");
    assert!(disk_win_hits > 0, "anti-vacuous: expected unsafe tokens in src/disk_win.rs");
    assert!(
        violations.is_empty(),
        "unsafe boundary violations outside allowlist:\n{}",
        violations.join("\n")
    );
}

#[test]
fn t1_f9_cargo_toml_profile_features_and_deps() {
    let manifest = Path::new(MANIFEST_DIR);
    let cargo_toml = fs::read_to_string(manifest.join("Cargo.toml")).expect("read Cargo.toml");
    validate_cargo_toml(&cargo_toml).unwrap_or_else(|e| panic!("Cargo.toml check failed: {}", e));

    let lib_rs = fs::read_to_string(manifest.join(r"src\lib.rs")).expect("read src/lib.rs");
    let lib_lines: Vec<&str> = lib_rs.lines().map(str::trim).collect();
    let has_gated_ui = lib_lines
        .iter()
        .any(|l| l.contains("cfg(feature = \"gui\")") && l.contains("pub mod ui;"))
        || lib_lines
            .windows(2)
            .any(|w| w[0].contains("cfg(feature = \"gui\")") && w[1].starts_with("pub mod ui;"));
    assert!(
        has_gated_ui,
        "src/lib.rs must gate `pub mod ui;` behind `#[cfg(feature = \"gui\")]`"
    );
}

#[test]
fn t1_f9_cargo_lock_eliminates_duplicate_winreg_and_windows_sys() {
    let lock_text =
        fs::read_to_string(Path::new(MANIFEST_DIR).join("Cargo.lock")).expect("read Cargo.lock");
    validate_cargo_lock(&lock_text)
        .unwrap_or_else(|e| panic!("Cargo.lock check failed: {}", e));
}

#[test]
fn t1_f9_mcp_rs_has_no_catch_unwind() {
    let mcp_rs =
        fs::read_to_string(Path::new(MANIFEST_DIR).join(r"src\mcp.rs")).expect("read src/mcp.rs");
    let hits = scan_ident_tokens(&mcp_rs, "catch_unwind");
    assert!(
        hits.is_empty(),
        "src/mcp.rs must not use catch_unwind (incompatible with panic=\"abort\"); found lines {:?}",
        hits
    );
}

#[test]
fn t1_f9_packaging_compressed_embed_cli_and_portable_exclusion() {
    let manifest = Path::new(MANIFEST_DIR);
    let package_ps1 = fs::read_to_string(manifest.join("package.ps1")).expect("read package.ps1");
    let build_rs = fs::read_to_string(manifest.join("build.rs")).expect("read build.rs");
    let win_rs = fs::read_to_string(manifest.join(r"src\win.rs")).expect("read src/win.rs");
    let install_rs =
        fs::read_to_string(manifest.join(r"src\install.rs")).expect("read src/install.rs");
    let workflow = fs::read_to_string(manifest.join(r".github\workflows\check.yml"))
        .expect("read .github/workflows/check.yml");

    // 1. Setup builds with `embed-cli`, portable stage builds WITHOUT `embed-cli`.
    assert!(
        package_ps1.contains("embed-cli"),
        "package.ps1 must build Setup with `embed-cli`"
    );
    let builds_without_embed = package_ps1.contains("--no-default-features")
        || package_ps1
            .lines()
            .filter(|l| l.contains("AtherMemGuard") && l.contains("cargo"))
            .count()
            >= 2;
    assert!(
        builds_without_embed,
        "package.ps1 must build AtherMemGuard separately without `embed-cli` for portable zip"
    );

    // 2. LZNT1/XPRESS compression used for embedded CLI.
    let combined_compress = format!("{}\n{}\n{}\n{}", win_rs, install_rs, build_rs, package_ps1);
    assert!(
        combined_compress.contains("RtlCompressBuffer"),
        "expected RtlCompressBuffer across win.rs/install.rs/build.rs/package.ps1"
    );
    assert!(
        combined_compress.contains("RtlDecompressBuffer"),
        "expected RtlDecompressBuffer across win.rs/install.rs/build.rs/package.ps1"
    );

    // 3. CI workflow checks -D warnings, --locked build/test, and embed-cli build.
    assert!(workflow.contains("-D warnings"), "check.yml missing -D warnings");
    assert!(workflow.contains("--locked"), "check.yml missing --locked");
    assert!(workflow.contains("embed-cli"), "check.yml missing embed-cli");
}

#[test]
fn t1_f9_cli_feature_isolation_and_manifest_invariants() {
    let manifest = Path::new(MANIFEST_DIR);
    let build_rs = fs::read_to_string(manifest.join("build.rs")).expect("read build.rs");
    assert!(build_rs.contains(r#"level="asInvoker""#));
    assert!(build_rs.contains("PerMonitorV2"));
    assert!(build_rs.contains("longPathAware"));
    assert!(build_rs.contains("AtherMemGuard.portable"));

    let memguard_bin_src =
        fs::read_to_string(manifest.join(r"src\bin\memguard.rs")).expect("read memguard.rs");
    assert!(
        scan_ident_tokens(&memguard_bin_src, "ui").is_empty(),
        "src/bin/memguard.rs must not reference `ui`"
    );

    let disk_clean_src =
        fs::read_to_string(manifest.join(r"src\disk_clean.rs")).expect("read disk_clean.rs");
    assert!(scan_ident_tokens(&disk_clean_src, "unsafe").is_empty());

    let env = TestDir::new("f9_cli_smoke");
    let ver = env.run_output(&["--version"]);
    assert!(ver.status.success());
    assert!(String::from_utf8_lossy(&ver.stdout).contains("memguard"));
    let help = env.run_output(&["--help"]);
    assert!(help.status.success());
    let help_s = String::from_utf8_lossy(&help.stdout);
    for sub in ["status", "wait", "doctor", "disk", "mcp"] {
        assert!(help_s.contains(sub), "--help missing `{}`", sub);
    }
}

#[test]
fn t2_f9_scanner_ignores_comments_and_nested_blocks() {
    let clean_comments = concat!(
        "// unsafe in line comment\n",
        "/// unsafe in doc comment\n",
        "//! unsafe in inner doc comment\n",
        "/* unsafe in block comment */\n",
        "/* outer /* nested unsafe */ still comment unsafe */\n",
        "/* /*/ unsafe */\n",
    );
    assert!(
        scan_ident_tokens(clean_comments, "unsafe").is_empty(),
        "comments produced false positives"
    );

    let mixed = "/* a */ unsafe { /* b */ }\n";
    assert_eq!(scan_ident_tokens(mixed, "unsafe"), vec![1]);
}

#[test]
fn t2_f9_scanner_ignores_strings_raw_strings_and_byte_strings() {
    let literals_only = concat!(
        "let s = \"unsafe\";\n",
        "let e = \"\\\"unsafe\\\"\";\n",
        "let r1 = r\"unsafe\";\n",
        "let r2 = r#\" \"unsafe\" \"#;\n",
        "let r3 = r##\" \"# unsafe \"# \"##;\n",
        "let b1 = b\"unsafe\";\n",
        "let br = br##\"b\"# unsafe\"##;\n",
        "let c1 = c\"unsafe\";\n",
        "let cr = cr#\"unsafe\"#;\n",
    );
    assert!(
        scan_ident_tokens(literals_only, "unsafe").is_empty(),
        "string literals produced false positives"
    );

    let escapes_and_chars = concat!(
        "let s1 = \"\\\\\"; unsafe {}\n",
        "let s2 = r\"\\\"; unsafe {}\n",
        "let q = '\"'; unsafe {}\n",
        "let a = '\\''; let b = b'\\\\'; unsafe {}\n",
    );
    assert_eq!(scan_ident_tokens(escapes_and_chars, "unsafe"), vec![1, 2, 3, 4]);
}

#[test]
fn t2_f9_scanner_handles_lifetimes_and_word_boundaries() {
    let lifetimes_and_boundaries = concat!(
        "fn f<'a, 'b>(x: &'a str, y: &'b str) -> &'a str { unsafe { x } }\n",
        "'outer: loop { unsafe { break 'outer; } }\n",
        "}unsafe{}\n",
        "let raw_lt: &'r#unsafe str = \"\"; let unsafe_code = 1; let is_unsafe = 2; let r#unsafe = 3; #[allow(unsafe_code)] fn g() {}\n",
    );
    assert_eq!(
        scan_ident_tokens(lifetimes_and_boundaries, "unsafe"),
        vec![1, 2, 3]
    );
}

#[test]
fn t2_f9_allowlist_path_normalization_and_rejection() {
    for ok in [
        "src/win.rs",
        r"src\win.rs",
        "src/ui/canvas.rs",
        r"src\ui\flyout\mod.rs",
        "src/disk_win.rs",
    ] {
        assert!(is_allowlisted_unsafe_path(ok), "should allow {}", ok);
    }
    for bad in [
        "src/drivers.rs",
        "src/disk_clean.rs",
        "src/win_extra.rs",
        "src/ui_fake/mod.rs",
        "src/my_ui/mod.rs",
        "src/bin/memguard.rs",
        "src/bin/tray.rs",
        "src/mcp.rs",
    ] {
        assert!(!is_allowlisted_unsafe_path(bad), "should reject {}", bad);
    }
}

#[test]
fn t2_f9_cargo_toml_parser_rejects_violations_and_malformed_inputs() {
    let valid = r#"
[features]
default = ["gui"]
gui = []
ui-snapshots = ["gui"]
embed-cli = []

[[bin]]
name = "AtherMemGuard"
required-features = ["gui"]

[dependencies]
serde = "1"

[profile.release]
panic = "abort"
"#;
    assert!(validate_cargo_toml(valid).is_ok(), "{:?}", validate_cargo_toml(valid));

    let bad_cases = [
        "",
        "[features]\ndefault=[\"gui\"]\ngui=[]\nui-snapshots=[\"gui\"]\nembed-cli=[]\n[[bin]]\nname=\"AtherMemGuard\"\nrequired-features=[\"gui\"]\n",
        &valid.replace("panic = \"abort\"", "panic = \"unwind\""),
        &valid.replace("panic = \"abort\"", "# panic = \"abort\""),
        &valid.replace("ui-snapshots = [\"gui\"]\n", ""),
        &valid.replace("serde = \"1\"", "winreg = \"0.52\""),
        &format!("{}\n[dependencies.winreg]\nversion = \"0.52\"\n", valid),
        &format!("{}\n[dependencies]\nwinreg.version = \"0.52\"\n", valid),
        &format!("{}\n[target.'cfg(windows)'.dependencies]\nwinreg = \"0.52\"\n", valid),
        &format!("{}\n[target.'cfg(windows)'.dev-dependencies]\nwinreg = \"0.52\"\n", valid),
        &format!("{}\n[target.'cfg(windows)'.build-dependencies.winreg]\nversion = \"0.52\"\n", valid),
        "[unclosed",
    ];
    for (idx, bad) in bad_cases.iter().enumerate() {
        assert!(
            validate_cargo_toml(bad).is_err(),
            "case {} unexpectedly succeeded",
            idx
        );
    }
}

#[test]
fn t2_f9_cargo_lock_parser_rejects_forbidden_versions_and_malformed_inputs() {
    let valid = "[[package]]\nname = \"windows-sys\"\nversion = \"0.59.0\"\n";
    assert!(validate_cargo_lock(valid).is_ok());

    let bad_cases = [
        "",
        "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n",
        &format!("{}\n[[package]]\nname = \"winreg\"\nversion = \"0.52.0\"\n", valid),
        &format!("{}\n[[package]]\nname = \"windows-sys\"\nversion = \"0.48.0\"\n", valid),
        &format!("{}\n[[package]]\nname = \"windows-sys\"\nversion = \"0.61.2\"\n", valid),
        "[[package]]\nname = \"broken\"\n",
    ];
    for (idx, bad) in bad_cases.iter().enumerate() {
        assert!(
            validate_cargo_lock(bad).is_err(),
            "lock case {} unexpectedly succeeded",
            idx
        );
    }
}

// ============================================================================
// Tier 3: Pairwise Cross-Feature Combinations (10 Tests: `t3_*`)
// ============================================================================

#[test]
fn t3_team_and_user_config_shape_status() {
    let env = TestDir::new("t3_team_user");
    let pid = std::process::id();
    let user_stem = format!("AmgE2EUser_{}", pid);
    let _user_child = spawn_ping_exe(&env.dir, &user_stem);

    let team_src = env.dir.join("team_source.toml");
    fs::write(
        &team_src,
        format!(
            "interval_sec = 5\n[general]\npreset = \"gentle\"\n[[group]]\nname = \"TeamApp\"\nprocesses = [\"AmgE2ETeam_{}\"]\ntrim_above_pct = 5.0\n",
            pid
        ),
    )
    .expect("write team_source.toml");

    let team_out = env.run_output(&["config", "team", &team_src.display().to_string()]);
    assert!(
        team_out.status.success(),
        "config team failed: {}",
        String::from_utf8_lossy(&team_out.stderr)
    );

    env.write_settings(&format!(
        "[[group]]\nname = \"UserApp\"\nprocesses = [\"{}.exe\"]\npriority = \"low\"\n",
        user_stem
    ));
    let set_out = env.run_output(&["config", "set", "general.preset", "aggressive"]);
    assert!(
        set_out.status.success(),
        "config set failed: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );

    let show_out = env.run_output(&["config", "show"]);
    assert!(show_out.status.success());
    let show_text = String::from_utf8_lossy(&show_out.stdout);
    assert!(show_text.contains("preset = \"aggressive\""), "{}", show_text);
    assert!(show_text.contains("TeamApp"), "{}", show_text);
    assert!(show_text.contains("UserApp"), "{}", show_text);

    let st = env.status_json();
    let groups = st["groups"].as_array().expect("groups");
    assert!(groups.iter().any(|g| g["name"] == "TeamApp"));
    let user_g = groups
        .iter()
        .find(|g| g["name"] == "UserApp")
        .expect("UserApp in status");
    assert!(user_g["processes"].as_u64().unwrap_or(0) >= 1, "{}", user_g);
}

#[test]
fn t3_config_reload_updates_daemon_status() {
    let env = TestDir::new("t3_reload_daemon");
    env.write_settings("interval_sec = 30\n");

    let _daemon = DaemonGuard::start(&env);
    let st0 = env.status_json();
    assert_eq!(st0["daemon"], true);
    assert_eq!(st0["paused"], false);

    let stem = format!("AmgE2EReload_{}", std::process::id());
    let _child = spawn_ping_exe(&env.dir, &stem);

    let mut cfg_text = fs::read_to_string(env.dir.join("settings.toml")).unwrap_or_default();
    cfg_text.push_str(&format!(
        "\n[[group]]\nname = \"ReloadGrp\"\nprocesses = [\"{}\"]\npriority = \"verylow\"\n",
        stem
    ));
    env.write_settings(&cfg_text);

    let reload_out = env.run_output(&["reload"]);
    assert!(
        reload_out.status.success(),
        "memguard reload failed (exit {:?}): {}",
        reload_out.status.code(),
        String::from_utf8_lossy(&reload_out.stderr)
    );

    let ok = poll_until(Duration::from_secs(6), || {
        let st = env.status_json();
        st["daemon"] == true
            && st["groups"]
                .as_array()
                .and_then(|arr| arr.iter().find(|g| g["name"] == "ReloadGrp"))
                .is_some_and(|g| {
                    g["processes"].as_u64().unwrap_or(0) >= 1
                        && g["policy"]["priority"] == "verylow"
                })
    });
    assert!(ok, "status --json did not reflect ReloadGrp within 6s");
}

#[test]
fn t3_disk_checkup_plan_clean_bin_undo_lifecycle() {
    let env = TestDir::new("t3_disk_bin_undo");
    env.write_settings("[disk]\nmin_age_hours = 0\n");

    let proj = env.dir.join("Proj");
    fs::create_dir_all(proj.join(r"Saved\Logs")).expect("create Saved\\Logs");
    fs::write(proj.join("Game.uproject"), br#"{"FileVersion": 3}"#).expect("write uproject");
    fs::write(proj.join(r"Saved\Logs\test.log"), vec![7u8; 4096]).expect("write test.log");

    let bin_before: Value = serde_json::from_slice(
        &env.run_output(&["disk", "bin", "--json"]).stdout,
    )
    .expect("bin before");
    let before_ids: BTreeSet<String> = bin_before["runs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r["run_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let chk_out = env.run_output(&[
        "disk",
        "checkup",
        "--path",
        &proj.display().to_string(),
        "--json",
    ]);
    assert!(chk_out.status.success());
    let chk: Value = serde_json::from_slice(&chk_out.stdout).expect("parse checkup");
    let cid = chk["scan"]["checkup_id"]
        .as_str()
        .expect("checkup_id")
        .to_string();
    let logs = chk["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["key"] == "ue:logs")
        .expect("ue:logs item");
    assert_eq!(logs["class"], "disposable");
    let d_logs = logs["id"].as_str().expect("d_logs id").to_string();

    let plan_out = env.run_output(&["disk", "plan", &d_logs, "--json"]);
    assert!(plan_out.status.success());
    let plan: Value = serde_json::from_slice(&plan_out.stdout).expect("parse plan");
    assert!(plan["plan_id"].as_str().is_some());

    let mut bin_guard = BinCleanupGuard::with_baseline(&env, before_ids.clone());
    let clean_out = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        &format!("{}:bin", d_logs),
        "--yes",
        "--checkup",
        &cid,
        "--json",
    ]);
    assert!(
        clean_out.status.success(),
        "clean :bin failed: {}",
        String::from_utf8_lossy(&clean_out.stderr)
    );
    let clean: Value = serde_json::from_slice(&clean_out.stdout).expect("parse clean");
    assert_eq!(clean["outcomes"][0]["result"], "done");
    let undo_id = clean["outcomes"][0]["undo_id"]
        .as_str()
        .expect("undo_id")
        .to_string();
    let run_id = clean["run_id"].as_str().expect("run_id").to_string();
    assert!(clean["bin_note"].is_string());
    assert!(!proj.join(r"Saved\Logs").exists());

    let bin_after: Value = serde_json::from_slice(
        &env.run_output(&["disk", "bin", "--json"]).stdout,
    )
    .expect("bin after");
    let new_run = bin_after["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| {
            r["run_id"]
                .as_str()
                .is_some_and(|id| !before_ids.contains(id) && id == run_id)
        })
        .expect("new bin run");
    let vol_root = new_run["volume_root"]
        .as_str()
        .expect("volume_root")
        .to_string();
    bin_guard.track(vol_root, run_id.clone(), undo_id.clone());
    assert!(new_run["holds"].as_u64().unwrap_or(0) > 0);

    let undo_out = env.run_output(&["disk", "undo", &undo_id, "--json"]);
    assert!(undo_out.status.success());
    let undo: Value = serde_json::from_slice(&undo_out.stdout).expect("parse undo");
    assert_eq!(undo["outcomes"][0]["result"], "undone");
    let restored = fs::read(proj.join(r"Saved\Logs\test.log")).expect("restored log");
    assert_eq!(restored.len(), 4096);

    let bin_final: Value = serde_json::from_slice(
        &env.run_output(&["disk", "bin", "--json"]).stdout,
    )
    .expect("bin final");
    assert!(
        !bin_final["runs"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .any(|r| r["run_id"] == run_id),
        "undone run_id should no longer appear in disk bin"
    );
}

#[test]
fn t3_git_tracked_vs_untracked_controls_clean() {
    let env = TestDir::new("t3_git_clean");
    env.write_settings("[disk]\nmin_age_hours = 0\n");

    let proj = env.dir.join("GitGame");
    fs::create_dir_all(proj.join("Intermediate")).expect("create Intermediate");
    fs::create_dir_all(proj.join("DerivedDataCache")).expect("create DDC");
    fs::write(proj.join("GitGame.uproject"), br#"{"FileVersion": 3}"#).expect("uproject");
    fs::write(proj.join(r"Intermediate\tracked.obj"), vec![1u8; 4096]).expect("tracked.obj");
    fs::write(proj.join(r"DerivedDataCache\untracked.bin"), vec![2u8; 4096])
        .expect("untracked.bin");

    run_git(&proj, &["init"]);
    run_git(&proj, &["config", "user.email", "t@example.com"]);
    run_git(&proj, &["config", "user.name", "Test"]);
    run_git(&proj, &["add", "Intermediate/tracked.obj"]);
    run_git(&proj, &["commit", "-m", "init"]);

    let chk_out = env.run_output(&[
        "disk",
        "checkup",
        "--path",
        &proj.display().to_string(),
        "--json",
    ]);
    assert!(chk_out.status.success());
    let chk: Value = serde_json::from_slice(&chk_out.stdout).expect("checkup json");
    let cid = chk["scan"]["checkup_id"].as_str().expect("cid");
    let items = chk["items"].as_array().expect("items");
    let d_int = items
        .iter()
        .find(|i| i["key"] == "ue:intermediate")
        .and_then(|i| i["id"].as_str())
        .expect("d_int");
    let d_ddc = items
        .iter()
        .find(|i| i["key"] == "ue:ddc")
        .and_then(|i| i["id"].as_str())
        .expect("d_ddc");

    let int_clean = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        &format!("{}:now", d_int),
        "--yes",
        "--checkup",
        cid,
        "--json",
    ]);
    assert_eq!(int_clean.status.code(), Some(1));
    let int_json: Value = serde_json::from_slice(&int_clean.stdout).expect("int clean json");
    assert_eq!(int_json["outcomes"][0]["result"], "refused");
    let msg = int_json["outcomes"][0]["message"].as_str().unwrap_or("");
    assert!(msg.contains("Git tracks"), "expected `Git tracks`, got: {}", msg);
    assert!(proj.join(r"Intermediate\tracked.obj").is_file());

    let ddc_clean = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        &format!("{}:now", d_ddc),
        "--yes",
        "--checkup",
        cid,
        "--json",
    ]);
    assert!(ddc_clean.status.success());
    let ddc_json: Value = serde_json::from_slice(&ddc_clean.stdout).expect("ddc clean json");
    assert_eq!(ddc_json["outcomes"][0]["result"], "done");
    assert!(!proj.join(r"DerivedDataCache\untracked.bin").exists());
}

#[test]
fn t3_run_caps_and_priority_forwards_exit_code() {
    let env = TestDir::new("t3_run_caps");

    let ok = env.run_output(&[
        "run",
        "--cap-gb",
        "0.5",
        "--per-process-cap-gb",
        "0.25",
        "--priority",
        "verylow",
        "--cpu",
        "eco",
        "--",
        "cmd",
        "/C",
        "exit",
        "0",
    ]);
    assert_eq!(ok.status.code(), Some(0));

    let code7 = env.run_output(&[
        "run",
        "--cap-gb",
        "0.5",
        "--priority",
        "low",
        "--cpu",
        "below",
        "--",
        "cmd",
        "/C",
        "exit",
        "7",
    ]);
    assert_eq!(code7.status.code(), Some(7));

    let missing = env.run_output(&[
        "run",
        "--cap-gb",
        "0.5",
        "--",
        "__nonexistent_e2e_exe_xyz__",
    ]);
    assert_eq!(missing.status.code(), Some(125));
}

#[test]
fn t3_doctor_self_tree_and_dry_run_fix_undo() {
    let env = TestDir::new("t3_doctor_flow");
    let pid = std::process::id();
    let win_stem = format!("AmgE2EDocWin_{}", pid);
    let wrap_stem = format!("AmgE2EWrap_{}", pid);

    let gui_exe = build_dummy_gui_exe(&env.dir, &win_stem);
    let wrap_exe = copy_sys_exe(&env.dir, r"System32\cmd.exe", &wrap_stem);
    let _taskkill = TaskkillByName(format!("{}.exe", win_stem));

    env.write_settings(&format!(
        "[doctor]\nignore_below_mb = 0\nclose = [\"{}\", \"{}\"]\n",
        win_stem, wrap_stem
    ));

    // (a) Self-tree check via wrapper cmd script.
    let doc_cmd = env.dir.join("self_doc.cmd");
    fs::write(&doc_cmd, format!("@\"{}\" doctor --json\r\n", MEMGUARD_BIN)).expect("write cmd");
    let mut self_cmd = Command::new(&wrap_exe);
    self_cmd
        .args(["/C", &doc_cmd.display().to_string()])
        .env("ATHER_MEMGUARD_DIR", &env.dir)
        .creation_flags(CREATE_NO_WINDOW);
    let self_out = env.run_cmd(self_cmd);
    assert!(self_out.status.success());
    let self_report: Value =
        serde_json::from_slice(&self_out.stdout).expect("parse self_tree doctor --json");
    let wrap_id = format!("app:{}", wrap_stem.to_ascii_lowercase());
    let wrap_finding = self_report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|f| f["id"] == wrap_id)
        .expect("wrapper finding in self_tree");
    assert_eq!(wrap_finding["actions"], json!([]));
    assert!(
        wrap_finding["detail"]
            .as_str()
            .unwrap_or("")
            .contains("started from this app"),
        "{}",
        wrap_finding
    );

    let fix_cmd = env.dir.join("self_fix.cmd");
    fs::write(
        &fix_cmd,
        format!("@\"{}\" doctor --fix {} --json\r\n", MEMGUARD_BIN, wrap_id),
    )
    .expect("write fix cmd");
    let mut self_fix_cmd = Command::new(&wrap_exe);
    self_fix_cmd
        .args(["/C", &fix_cmd.display().to_string()])
        .env("ATHER_MEMGUARD_DIR", &env.dir)
        .creation_flags(CREATE_NO_WINDOW);
    let self_fix_out = env.run_cmd(self_fix_cmd);
    assert_eq!(self_fix_out.status.code(), Some(1));

    // (b) Sibling windowed process: dry-run -> close -> undo.
    let child = Command::new(&gui_exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn windowed dummy");
    let mut dummy = KillChild(child);

    let doc_id = format!("app:{}", win_stem.to_ascii_lowercase());
    let appeared = poll_until(Duration::from_secs(25), || {
        let out = env.run_output(&["doctor", "--json"]);
        if !out.status.success() {
            return false;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            return false;
        };
        v["findings"]
            .as_array()
            .and_then(|arr| arr.iter().find(|f| f["id"] == doc_id))
            .is_some_and(|f| {
                f["has_window"] == true
                    && f["actions"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|x| x["kind"] == "close"))
            })
    });
    assert!(appeared, "windowed dummy did not appear in doctor --json");

    let dry_out = env.run_output(&[
        "doctor",
        "--fix",
        &format!("{}:close", doc_id),
        "--dry-run",
        "--json",
    ]);
    assert!(dry_out.status.success());
    let dry_json: Value = serde_json::from_slice(&dry_out.stdout).expect("dry-run json");
    assert_eq!(dry_json["outcomes"][0]["result"], "would_do");
    assert!(dummy.is_alive(), "dry-run must not close dummy");

    let fix_out = env.run_output(&["doctor", "--fix", &format!("{}:close", doc_id), "--json"]);
    assert!(fix_out.status.success());
    let fix_json: Value = serde_json::from_slice(&fix_out.stdout).expect("fix json");
    assert_eq!(fix_json["outcomes"][0]["result"], "done");
    let undo_id = fix_json["outcomes"][0]["undo_id"]
        .as_str()
        .expect("undo_id")
        .to_string();
    assert!(dummy.exited_within(Duration::from_secs(5)));

    let undo_out = env.run_output(&["doctor", "--undo", &undo_id, "--json"]);
    assert!(undo_out.status.success());
    let undo_json: Value = serde_json::from_slice(&undo_out.stdout).expect("undo json");
    assert_eq!(undo_json["outcomes"][0]["result"], "undone");

    let relaunched = poll_until(Duration::from_secs(15), || {
        let out = env.run_output(&["doctor", "--json"]);
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            return false;
        };
        v["findings"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|f| f["id"] == doc_id))
    });
    assert!(relaunched, "relaunched dummy did not reappear in doctor --json");
}

#[test]
fn t3_wait_disk_gate_with_prime_and_path() {
    let env = TestDir::new("t3_wait_disk");
    let path_s = env.dir.display().to_string();

    let blocked = env.run_output(&[
        "wait",
        "--disk-path",
        &path_s,
        "--disk-gb",
        "1000000",
        "--prime",
        "--ram-gb",
        "0.01",
        "--commit-gb",
        "0.01",
        "--timeout",
        "2",
        "--json",
    ]);
    assert_eq!(blocked.status.code(), Some(2));
    let b_json: Value = serde_json::from_slice(&blocked.stdout).expect("blocked wait json");
    assert_eq!(b_json["ready"], false);
    assert_eq!(b_json["waited_sec"], 0);
    assert_eq!(b_json["blocked_by"], json!(["disk"]));
    let checks = b_json["disk_checks"].as_array().expect("disk_checks");
    assert!(checks.iter().any(|c| c["ok"] == false));

    let passed = env.run_output(&[
        "wait",
        "--disk-path",
        &path_s,
        "--disk-gb",
        "0.01",
        "--prime",
        "--ram-gb",
        "0.01",
        "--commit-gb",
        "0.01",
        "--timeout",
        "5",
        "--json",
    ]);
    assert_eq!(passed.status.code(), Some(0));
    let p_json: Value = serde_json::from_slice(&passed.stdout).expect("passed wait json");
    assert_eq!(p_json["ready"], true);
    assert_eq!(p_json["blocked_by"], json!([]));
    assert!(
        p_json["disk_checks"]
            .as_array()
            .expect("disk_checks")
            .iter()
            .all(|c| c["ok"] == true)
    );
}

#[test]
fn t3_mcp_config_and_status_reflect_settings() {
    let env = TestDir::new("t3_mcp_cfg_status");
    let stem = format!("AmgE2EMcp_{}", std::process::id());
    let _child = spawn_ping_exe(&env.dir, &stem);

    env.write_settings(&format!(
        "[[group]]\nname = \"McpTestGrp\"\nprocesses = [\"{}\"]\ntrim_above_mb = 512\n",
        stem
    ));
    let set_out = env.run_output(&["config", "set", "general.preset", "aggressive"]);
    assert!(set_out.status.success());

    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1);

    let (_n, cfg_r) = mcp
        .call_tool(2, "memguard_config", json!({}), None, Duration::from_secs(5))
        .expect("memguard_config");
    assert!(
        cfg_r.get("error").is_none() && cfg_r["result"]["isError"] == false,
        "memguard_config failed: {}",
        cfg_r
    );
    let cfg_text = cfg_r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(cfg_text.contains("aggressive") && cfg_text.contains("McpTestGrp"), "{}", cfg_text);

    let (_n, st_r) = mcp
        .call_tool(3, "memguard_status", json!({}), None, Duration::from_secs(5))
        .expect("memguard_status");
    assert_eq!(st_r["result"]["isError"], false);
    let grp = st_r["result"]["structuredContent"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"] == "McpTestGrp")
        .expect("McpTestGrp in status");
    assert!(grp["processes"].as_u64().unwrap_or(0) >= 1, "{}", grp);
    assert_eq!(grp["policy"]["trim_above_mb"], 256);
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t3_daemon_history_cli_and_mcp_summary() {
    let env = TestDir::new("t3_history_summary");
    env.write_settings("interval_sec = 2\n[history]\nenabled = true\n");

    let mut daemon = DaemonGuard::start(&env);
    let polled = poll_until(Duration::from_secs(6), || {
        let csv_rows = fs::read_dir(env.dir.join("history"))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("csv"))
            .filter_map(|p| fs::read_to_string(p).ok())
            .map(|t| t.lines().count().saturating_sub(1))
            .sum::<usize>();
        csv_rows >= 2 || daemon.tick_count() >= 2
    });
    assert!(
        polled,
        "expected >= 2 daemon history ticks/rows within 6s (ticks={})",
        daemon.tick_count()
    );
    let stop_out = daemon.stop();
    assert!(stop_out.status.success());

    let csvs: Vec<PathBuf> = fs::read_dir(env.dir.join("history"))
        .expect("read history dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("csv"))
        .collect();
    assert!(!csvs.is_empty(), "expected history CSV");
    let csv_text = fs::read_to_string(&csvs[0]).expect("read csv");
    assert!(csv_text.lines().count() >= 3, "expected >= 2 data rows:\n{}", csv_text);

    let cli_hist = env.run_output(&["history", "--hours", "1", "--json"]);
    assert!(cli_hist.status.success());
    let cli_samples: Vec<Value> = serde_json::from_slice(&cli_hist.stdout).expect("cli samples");
    assert!(cli_samples.len() >= 2, "expected >= 2 CLI samples");

    let mut mcp = McpClient::spawn(&env);
    mcp.initialize(1);

    let (_n, hist_r) = mcp
        .call_tool(2, "memguard_history", json!({ "hours": 1 }), None, Duration::from_secs(5))
        .expect("memguard_history");
    assert_eq!(hist_r["result"]["isError"], false);
    assert_eq!(
        hist_r["result"]["structuredContent"]["total_samples"].as_u64(),
        Some(cli_samples.len() as u64)
    );

    let (_n, sum_r) = mcp
        .call_tool(3, "memguard_session_summary", json!({}), None, Duration::from_secs(5))
        .expect("memguard_session_summary");
    assert!(
        sum_r.get("error").is_none() && sum_r["result"]["isError"] == false,
        "memguard_session_summary failed: {}",
        sum_r
    );
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t3_disk_min_age_refusal_vs_zero_override() {
    let env = TestDir::new("t3_min_age");
    let proj = env.dir.join("FreshProj");
    fs::create_dir_all(proj.join("Intermediate")).expect("create Intermediate");
    fs::write(proj.join("FreshProj.uproject"), br#"{"FileVersion": 3}"#).expect("uproject");
    fs::write(proj.join(r"Intermediate\fresh.obj"), vec![1u8; 4096]).expect("fresh.obj");

    let chk_out = env.run_output(&[
        "disk",
        "checkup",
        "--path",
        &proj.display().to_string(),
        "--json",
    ]);
    assert!(chk_out.status.success());
    let chk: Value = serde_json::from_slice(&chk_out.stdout).expect("checkup");
    let cid = chk["scan"]["checkup_id"].as_str().expect("cid");
    let d_int = chk["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["key"] == "ue:intermediate")
        .and_then(|i| i["id"].as_str())
        .expect("d_int");

    let refused = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        &format!("{}:now", d_int),
        "--yes",
        "--checkup",
        cid,
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    let ref_json: Value = serde_json::from_slice(&refused.stdout).expect("refused json");
    assert_eq!(ref_json["outcomes"][0]["result"], "refused");
    assert!(
        ref_json["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("last 2 hours")
    );
    assert!(proj.join(r"Intermediate\fresh.obj").is_file());

    let set_out = env.run_output(&["config", "set", "disk.min_age_hours", "0"]);
    assert!(set_out.status.success());

    let cleaned = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        &format!("{}:now", d_int),
        "--yes",
        "--checkup",
        cid,
        "--json",
    ]);
    assert!(cleaned.status.success());
    let cl_json: Value = serde_json::from_slice(&cleaned.stdout).expect("cleaned json");
    assert_eq!(cl_json["outcomes"][0]["result"], "done");
    assert!(!proj.join(r"Intermediate\fresh.obj").exists());
}

// ============================================================================
// Tier 4: Real-World Developer Workflows (6 Scenarios: `t4_*`)
// ============================================================================

#[test]
fn t4_ue_git_workspace_checkup_plan_clean_bin_undo() {
    let env = TestDir::new("t4_ue_git");
    env.write_settings("[disk]\nmin_age_hours = 0\n");

    let proj = env.dir.join("MyGame");
    for d in [
        r"Content",
        r"Config",
        r"Source",
        r"Plugins\CoolPlugin\Intermediate",
        r"Intermediate",
        r"DerivedDataCache",
        r"Saved\Autosaves",
        r"Saved\Logs",
        r"Saved\Cooked\Windows",
    ] {
        fs::create_dir_all(proj.join(d)).expect("create project subdir");
    }
    fs::write(
        proj.join("MyGame.uproject"),
        br#"{"FileVersion": 3, "EngineAssociation": "5.4"}"#,
    )
    .expect("uproject");
    fs::write(proj.join(r"Content\Hero.uasset"), b"hero").expect("uasset");
    fs::write(proj.join(r"Config\DefaultEngine.ini"), b"[Core]\n").expect("ini");
    fs::write(proj.join(r"Source\MyGame.cpp"), b"int main() { return 0; }\n").expect("cpp");
    fs::write(proj.join(r"Plugins\CoolPlugin\CoolPlugin.uplugin"), b"{}").expect("uplugin");
    fs::write(
        proj.join(r"Plugins\CoolPlugin\Intermediate\p.obj"),
        vec![1u8; 8192],
    )
    .expect("plugin obj");
    fs::write(proj.join(r"Intermediate\Build.obj"), vec![2u8; 16384]).expect("Build.obj");
    fs::write(proj.join(r"DerivedDataCache\Pak.ddc"), vec![3u8; 8192]).expect("Pak.ddc");
    fs::write(proj.join(r"Saved\Autosaves\Auto.umap"), b"autosave").expect("Auto.umap");
    let log_bytes = vec![4u8; 4096];
    fs::write(proj.join(r"Saved\Logs\MyGame.log"), &log_bytes).expect("MyGame.log");
    fs::write(proj.join(r"Saved\Cooked\Windows\Cook.pak"), vec![5u8; 4096]).expect("Cook.pak");

    run_git(&proj, &["init"]);
    run_git(&proj, &["config", "user.email", "t@example.com"]);
    run_git(&proj, &["config", "user.name", "Test"]);
    run_git(
        &proj,
        &[
            "add",
            "MyGame.uproject",
            "Content/Hero.uasset",
            "Config/DefaultEngine.ini",
            "Source/MyGame.cpp",
        ],
    );
    run_git(&proj, &["commit", "-m", "initial game"]);

    let chk_out = env.run_output(&[
        "disk",
        "checkup",
        "--path",
        &proj.display().to_string(),
        "--json",
    ]);
    assert!(chk_out.status.success());
    let chk: Value = serde_json::from_slice(&chk_out.stdout).expect("checkup");
    let cid = chk["scan"]["checkup_id"].as_str().expect("cid").to_string();
    let items = chk["items"].as_array().expect("items");

    let find_id = |key: &str| -> String {
        items
            .iter()
            .find(|i| i["key"] == key)
            .and_then(|i| i["id"].as_str())
            .unwrap_or_else(|| panic!("missing item key {}", key))
            .to_string()
    };
    let d_int = find_id("ue:intermediate");
    let d_plug = find_id("ue:plugin_intermediate");
    let d_ddc = find_id("ue:ddc");
    let d_logs = find_id("ue:logs");
    let _d_cooked = find_id("ue:cooked");

    for it in items {
        for p in it["paths"].as_array().into_iter().flatten() {
            let s = p.as_str().unwrap_or("");
            assert!(!s.ends_with(r"\Content") && !s.ends_with(r"\Config") && !s.ends_with(r"\Source") && !s.contains("Autosaves") && !s.contains(r"\.git"),
                "protected path exposed in checkup: {}", s);
        }
    }

    let plan_out = env.run_output(&[
        "disk",
        "plan",
        &d_int,
        &d_plug,
        &d_ddc,
        &d_logs,
        "--json",
    ]);
    assert!(plan_out.status.success());
    let plan: Value = serde_json::from_slice(&plan_out.stdout).expect("plan");
    assert!(plan["plan_id"].as_str().is_some());
    assert!(
        plan["plans"][0]["frees_together_gb"]
            .as_f64()
            .is_some_and(|v| v >= 0.0)
    );

    let bin_before: Value = serde_json::from_slice(
        &env.run_output(&["disk", "bin", "--json"]).stdout,
    )
    .expect("bin before");
    let before_ids: BTreeSet<String> = bin_before["runs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r["run_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut bin_guard = BinCleanupGuard::with_baseline(&env, before_ids);
    let clean_out = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        &format!("{}:now", d_int),
        &format!("{}:now", d_plug),
        &format!("{}:now", d_ddc),
        &format!("{}:bin", d_logs),
        "--yes",
        "--checkup",
        &cid,
        "--json",
    ]);
    assert!(
        clean_out.status.success(),
        "clean failed: {}",
        String::from_utf8_lossy(&clean_out.stderr)
    );
    let clean: Value = serde_json::from_slice(&clean_out.stdout).expect("clean json");
    let outcomes = clean["outcomes"].as_array().expect("outcomes");
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes.iter().all(|o| o["result"] == "done"), "{}", clean);
    let logs_undo = outcomes
        .iter()
        .find(|o| o["action"] == "bin")
        .and_then(|o| o["undo_id"].as_str())
        .expect("logs undo_id")
        .to_string();
    let run_id = clean["run_id"].as_str().expect("run_id").to_string();

    assert!(!proj.join("Intermediate").exists());
    assert!(!proj.join("DerivedDataCache").exists());
    assert!(!proj.join(r"Saved\Logs").exists());
    assert!(proj.join(r"Content\Hero.uasset").is_file());
    assert!(proj.join(r"Config\DefaultEngine.ini").is_file());
    assert!(proj.join(r"Source\MyGame.cpp").is_file());
    assert!(proj.join(r"Saved\Autosaves\Auto.umap").is_file());

    let bin_json: Value = serde_json::from_slice(
        &env.run_output(&["disk", "bin", "--json"]).stdout,
    )
    .expect("disk bin");
    let run_entry = bin_json["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| r["run_id"] == run_id)
        .expect("test run in bin");
    assert!(run_entry["holds"].as_u64().unwrap_or(0) > 0);
    bin_guard.track(
        run_entry["volume_root"]
            .as_str()
            .expect("vol")
            .to_string(),
        run_id.clone(),
        logs_undo.clone(),
    );

    let undo_out = env.run_output(&["disk", "undo", &logs_undo, "--json"]);
    assert!(undo_out.status.success());
    let undo_json: Value = serde_json::from_slice(&undo_out.stdout).expect("undo json");
    assert_eq!(undo_json["outcomes"][0]["result"], "undone");
    assert_eq!(
        fs::read(proj.join(r"Saved\Logs\MyGame.log")).expect("restored log"),
        log_bytes
    );

    let bin_after: Value = serde_json::from_slice(
        &env.run_output(&["disk", "bin", "--json"]).stdout,
    )
    .expect("bin after");
    assert!(
        !bin_after["runs"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .any(|r| r["run_id"] == run_id)
    );
}

#[test]
fn t4_ai_agent_mcp_session_workflow() {
    let env = TestDir::new("t4_mcp_agent").with_env_key("t4_mcp_agent");
    env.write_settings("[disk]\nmin_age_hours = 0\n");

    let proj = env.dir.join("AgentGame");
    fs::create_dir_all(proj.join("Intermediate")).expect("Intermediate");
    fs::create_dir_all(proj.join(r"Saved\Logs")).expect("Logs");
    fs::write(proj.join("AgentGame.uproject"), br#"{"FileVersion": 3}"#).expect("uproject");
    fs::write(proj.join(r"Intermediate\a.obj"), vec![1u8; 8192]).expect("a.obj");
    fs::write(proj.join(r"Saved\Logs\a.log"), vec![2u8; 4096]).expect("a.log");

    let mut mcp = McpClient::spawn(&env);
    let init_r = mcp.initialize(1);
    assert_eq!(init_r["result"]["serverInfo"]["name"], "ather-memguard");
    assert!(init_r["result"]["capabilities"]["tools"].is_object());

    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
    let (_n, ping_r) = mcp
        .recv_until_id(&json!(2), Duration::from_secs(5))
        .expect("ping");
    assert_eq!(ping_r["result"], json!({}));

    mcp.send_json(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {} }));
    let (_n, list_r) = mcp
        .recv_until_id(&json!(3), Duration::from_secs(5))
        .expect("tools/list");
    let tools = list_r["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 16, "expected 16 tools, got {}", tools.len());

    let (_n, cfg_r) = mcp
        .call_tool(4, "memguard_config", json!({}), None, Duration::from_secs(5))
        .expect("config");
    assert_eq!(cfg_r["result"]["isError"], false, "{}", cfg_r);

    let (_n, drv_r) = mcp
        .call_tool(5, "memguard_drivers", json!({}), None, Duration::from_secs(5))
        .expect("drivers");
    assert_eq!(drv_r["result"]["isError"], false, "{}", drv_r);
    assert!(drv_r["result"]["structuredContent"]["tags_available"].is_boolean());

    let (_n, dsk_r) = mcp
        .call_tool(6, "memguard_disk", json!({}), None, Duration::from_secs(5))
        .expect("disk");
    assert_eq!(dsk_r["result"]["isError"], false, "{}", dsk_r);

    let (_n, sum_r) = mcp
        .call_tool(7, "memguard_session_summary", json!({}), None, Duration::from_secs(5))
        .expect("session_summary");
    assert_eq!(sum_r["result"]["isError"], false, "{}", sum_r);

    // Send memguard_wait (15s) and cancel immediately; must abort within 5s.
    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "memguard_wait",
            "arguments": { "ram_gb": 999999, "timeout_sec": 15 }
        }
    }));
    mcp.send_json(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 10 }
    }));
    let (_n, cancel_r) = mcp
        .recv_until_id(&json!(10), Duration::from_secs(5))
        .expect("cancelled wait must return within 5s");
    assert!(
        cancel_r.get("error").is_some()
            || cancel_r["result"]["isError"] == true
            || cancel_r["result"]["structuredContent"]["ready"] == false
    );

    let (_n, chk_r) = mcp
        .call_tool(
            11,
            "memguard_disk_checkup",
            json!({ "path": proj.display().to_string(), "refresh": true }),
            Some(json!({ "progressToken": "p1" })),
            Duration::from_secs(15),
        )
        .expect("disk_checkup");
    assert_eq!(chk_r["result"]["isError"], false);
    let items = chk_r["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items");
    let d_int = items
        .iter()
        .find(|i| i["key"] == "ue:intermediate")
        .and_then(|i| i["id"].as_str())
        .expect("d_int")
        .to_string();
    let d_logs = items
        .iter()
        .find(|i| i["key"] == "ue:logs")
        .and_then(|i| i["id"].as_str())
        .expect("d_logs")
        .to_string();

    let (_n, plan_r) = mcp
        .call_tool(
            12,
            "memguard_disk_plan",
            json!({ "ids": [&d_int, &d_logs] }),
            None,
            Duration::from_secs(15),
        )
        .expect("disk_plan");
    let plan_id = plan_r["result"]["structuredContent"]["plan_id"]
        .as_str()
        .expect("plan_id")
        .to_string();

    let (_n, cl_logs) = mcp
        .call_tool(
            13,
            "memguard_disk_clean",
            json!({ "ids": [&d_logs], "plan_id": &plan_id }),
            None,
            Duration::from_secs(20),
        )
        .expect("clean logs");
    assert_eq!(
        cl_logs["result"]["structuredContent"]["outcomes"][0]["result"],
        "refused"
    );

    let (_n, cl_int) = mcp
        .call_tool(
            14,
            "memguard_disk_clean",
            json!({ "ids": [&d_int], "plan_id": &plan_id }),
            None,
            Duration::from_secs(20),
        )
        .expect("clean intermediate");
    assert_eq!(
        cl_int["result"]["structuredContent"]["outcomes"][0]["result"],
        "done"
    );
    assert!(!proj.join(r"Intermediate\a.obj").exists());
    assert_eq!(mcp.close_and_wait(), 0);
}

#[test]
fn t4_team_config_daemon_pause_reload_resume_csv() {
    let env = TestDir::new("t4_team_daemon");
    let pid = std::process::id();
    let team_stem = format!("AmgE2ETeamD_{}", pid);
    let local_stem = format!("AmgE2ELocalD_{}", pid);

    let team_src = env.dir.join("team.src.toml");
    fs::write(
        &team_src,
        format!(
            "interval_sec = 2\n[general]\npreset = \"gentle\"\n[[group]]\nname = \"TeamGroup\"\nprocesses = [\"{}\"]\npriority = \"low\"\n",
            team_stem
        ),
    )
    .expect("write team src");
    let team_out = env.run_output(&["config", "team", &team_src.display().to_string()]);
    assert!(team_out.status.success());

    env.write_settings("[disk]\nmin_age_hours = 0\n");
    let _team_child = spawn_ping_exe(&env.dir, &team_stem);

    let mut daemon = DaemonGuard::start(&env);
    let st0 = env.status_json();
    assert_eq!(st0["daemon"], true);
    assert_eq!(st0["paused"], false);
    let tg0 = st0["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"] == "TeamGroup")
        .expect("TeamGroup");
    assert!(tg0["processes"].as_u64().unwrap_or(0) >= 1);

    let pause_out = env.run_output(&["pause", "--minutes", "5"]);
    assert!(
        pause_out.status.success(),
        "pause failed (exit {:?}): {}",
        pause_out.status.code(),
        String::from_utf8_lossy(&pause_out.stderr)
    );
    assert!(poll_until(Duration::from_secs(4), || env.status_json()["paused"] == true));

    let _local_child = spawn_ping_exe(&env.dir, &local_stem);
    env.write_settings(&format!(
        "[disk]\nmin_age_hours = 0\n[[group]]\nname = \"LocalGroup\"\nprocesses = [\"{}\"]\npriority = \"verylow\"\n",
        local_stem
    ));
    let reload_out = env.run_output(&["reload"]);
    assert!(reload_out.status.success());

    let resume_out = env.run_output(&["resume"]);
    assert!(resume_out.status.success());
    std::thread::sleep(Duration::from_millis(2400));

    let st1 = env.status_json();
    assert_eq!(st1["paused"], false);
    let groups1 = st1["groups"].as_array().expect("groups");
    assert!(
        groups1
            .iter()
            .find(|g| g["name"] == "TeamGroup")
            .is_some_and(|g| g["processes"].as_u64().unwrap_or(0) >= 1)
    );
    assert!(
        groups1
            .iter()
            .find(|g| g["name"] == "LocalGroup")
            .is_some_and(|g| g["processes"].as_u64().unwrap_or(0) >= 1)
    );

    let stop_out = daemon.stop();
    assert!(stop_out.status.success());

    let csvs: Vec<PathBuf> = fs::read_dir(env.dir.join("history"))
        .expect("history dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("csv"))
        .collect();
    assert!(!csvs.is_empty());

    let hist_out = env.run_output(&["history", "--hours", "1", "--json"]);
    assert!(hist_out.status.success());
    let samples: Vec<Value> = serde_json::from_slice(&hist_out.stdout).expect("history json");
    assert!(samples.len() >= 2, "expected >= 2 samples, got {}", samples.len());
    assert!(
        samples.iter().any(|s| {
            s["groups"]
                .as_array()
                .is_some_and(|gs| gs.iter().any(|g| g["name"] == "TeamGroup"))
        }),
        "expected TeamGroup in history samples"
    );
}

#[test]
fn t4_multi_process_workload_run_and_doctor_guard() {
    let env = TestDir::new("t4_workload_doctor");
    let pid = std::process::id();
    let bg_stem = format!("AmgE2EBgApp_{}", pid);
    let doc_stem = format!("AmgE2EDocApp_{}", pid);

    let gui_exe = build_dummy_gui_exe(&env.dir, &doc_stem);
    let _taskkill = TaskkillByName(format!("{}.exe", doc_stem));

    env.write_settings(&format!(
        "[[group]]\nname = \"TestBg\"\nprocesses = [\"{}\"]\npriority = \"verylow\"\n[doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        bg_stem, doc_stem
    ));

    let _bg_child = spawn_ping_exe(&env.dir, &bg_stem);
    let doc_child = Command::new(&gui_exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn windowed app");
    let mut doc_guard = KillChild(doc_child);

    let run_out = env.run_output(&[
        "run",
        "--cap-gb",
        "0.5",
        "--per-process-cap-gb",
        "0.25",
        "--priority",
        "verylow",
        "--cpu",
        "eco",
        "--",
        "cmd",
        "/C",
        "exit",
        "0",
    ]);
    assert_eq!(run_out.status.code(), Some(0));

    let st = env.status_json();
    let test_bg = st["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"] == "TestBg")
        .expect("TestBg in status");
    assert!(test_bg["processes"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(test_bg["policy"]["priority"], "verylow");

    let doc_id = format!("app:{}", doc_stem.to_ascii_lowercase());
    let seen = poll_until(Duration::from_secs(25), || {
        let out = env.run_output(&["doctor", "--json"]);
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            return false;
        };
        v["findings"]
            .as_array()
            .and_then(|arr| arr.iter().find(|f| f["id"] == doc_id))
            .is_some_and(|f| f["has_window"] == true && f["verdict"] == "close")
    });
    assert!(seen, "expected windowed {} in doctor --json", doc_id);

    let dry_out = env.run_output(&[
        "doctor",
        "--fix",
        &format!("{}:close", doc_id),
        "--dry-run",
        "--json",
    ]);
    assert!(dry_out.status.success());
    let dry_json: Value = serde_json::from_slice(&dry_out.stdout).expect("dry-run json");
    assert_eq!(dry_json["outcomes"][0]["result"], "would_do");
    assert!(doc_guard.is_alive());

    let fix_out = env.run_output(&["doctor", "--fix", &format!("{}:close", doc_id), "--json"]);
    assert!(fix_out.status.success());
    let fix_json: Value = serde_json::from_slice(&fix_out.stdout).expect("fix json");
    assert_eq!(fix_json["outcomes"][0]["result"], "done");
    let undo_id = fix_json["outcomes"][0]["undo_id"]
        .as_str()
        .expect("undo_id")
        .to_string();
    assert!(doc_guard.exited_within(Duration::from_secs(5)));

    let undo_out = env.run_output(&["doctor", "--undo", &undo_id, "--json"]);
    assert!(undo_out.status.success());
    let undo_json: Value = serde_json::from_slice(&undo_out.stdout).expect("undo json");
    assert_eq!(undo_json["outcomes"][0]["result"], "undone");

    let back = poll_until(Duration::from_secs(15), || {
        let out = env.run_output(&["doctor", "--json"]);
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            return false;
        };
        v["findings"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|f| f["id"] == doc_id))
    });
    assert!(back, "relaunched {} should reappear in doctor --json", doc_id);
}

#[test]
fn t4_cache_relocate_worktree_lfs_expanded_catalog() {
    let env = TestDir::new("t4_relocate_wt").with_env_key("t4_relocate_wt");
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);

    // Part A: Cache relocate in isolated HKCU test subkey + SystemDrive temp cache.
    let local_app_data = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| env.dir.display().to_string());
    let sys_cache_root = PathBuf::from(&local_app_data)
        .join("Temp")
        .join(format!("amg_e2e_vcpkg_{}_{}", pid, seq));
    let _sys_cache_guard = TempFolderGuard(sys_cache_root.clone());
    fs::create_dir_all(&sys_cache_root).expect("create sys_cache_root");
    fs::write(sys_cache_root.join("pkg.bin"), vec![1u8; 1024]).expect("write cache sample");

    let disk_out = env.run_output(&["disk", "--json"]);
    assert!(disk_out.status.success());
    let disk_json: Value = serde_json::from_slice(&disk_out.stdout).expect("disk --json");
    let alt_drive = disk_json["volumes"]
        .as_array()
        .and_then(|vols| {
            vols.iter().find(|v| {
                let roles = v["roles"].as_array();
                let is_sys = roles.is_some_and(|r| r.iter().any(|x| x == "system"));
                let is_local_backing = matches!(v["backing"].as_str(), Some("partition" | "vhd"));
                let is_normal = v["level"] == "normal";
                let has_room = v["free_gb"].as_f64().unwrap_or(0.0) > 2.0;
                !is_sys && is_local_backing && is_normal && has_room
            })
        })
        .and_then(|v| v["paths"].as_array().and_then(|p| p.first()).and_then(Value::as_str))
        .map(str::to_string);

    if let Some(drive_root) = alt_drive {
        let dest = PathBuf::from(&drive_root).join(format!("amg_e2e_dest_{}_{}", pid, seq));
        let _dest_guard = TempFolderGuard(dest.clone());

        let mut plan_cmd = env.cmd(&["disk", "relocate", "--to", &dest.display().to_string(), "--json"]);
        plan_cmd.env("VCPKG_DEFAULT_BINARY_CACHE", &sys_cache_root);
        let plan_out = env.run_cmd(plan_cmd);
        assert!(plan_out.status.success());
        let plan: Value = serde_json::from_slice(&plan_out.stdout).expect("plan json");
        assert!(plan["refused"].is_null(), "unexpected relocate refusal: {}", plan);

        let mut apply_cmd = env.cmd(&[
            "disk",
            "relocate",
            "--to",
            &dest.display().to_string(),
            "--apply",
            "--yes",
            "--json",
        ]);
        apply_cmd.env("VCPKG_DEFAULT_BINARY_CACHE", &sys_cache_root);
        let apply_out = env.run_cmd(apply_cmd);
        assert!(
            apply_out.status.success(),
            "relocate apply failed: {}",
            String::from_utf8_lossy(&apply_out.stderr)
        );
        let applied: Value = serde_json::from_slice(&apply_out.stdout).expect("apply json");
        assert_eq!(applied["applied"], true);

        std::thread::sleep(Duration::from_secs(2));
        let new_vcpkg = dest.join(r"vcpkg\archives");
        fs::create_dir_all(&new_vcpkg).expect("create relocated vcpkg");
        fs::write(new_vcpkg.join("m.bin"), b"marker").expect("write marker");

        let mut undo_cmd = env.cmd(&["disk", "relocate", "--undo", "--json"]);
        undo_cmd.env("VCPKG_DEFAULT_BINARY_CACHE", &sys_cache_root);
        let undo_out = env.run_cmd(undo_cmd);
        assert!(undo_out.status.success());
        let undone: Value = serde_json::from_slice(&undo_out.stdout).expect("undo json");
        assert_eq!(undone["undone"], true);
    } else {
        let mut plan_cmd = env.cmd(&["disk", "relocate", "--json"]);
        plan_cmd.env("VCPKG_DEFAULT_BINARY_CACHE", &sys_cache_root);
        let plan_out = env.run_cmd(plan_cmd);
        assert!(plan_out.status.success());
        let plan: Value = serde_json::from_slice(&plan_out.stdout).expect("plan json");
        assert!(!plan["refused"].is_null(), "expected refusal on single-drive host");

        let mut apply_cmd = env.cmd(&["disk", "relocate", "--apply", "--yes", "--json"]);
        apply_cmd.env("VCPKG_DEFAULT_BINARY_CACHE", &sys_cache_root);
        let apply_out = env.run_cmd(apply_cmd);
        assert_eq!(apply_out.status.code(), Some(1));
    }

    // Part B: Git worktree (with Git 2.48+ relative `gitdir:`) + LFS + expanded catalog.
    let main_proj = env.dir.join("MainProj");
    fs::create_dir_all(&main_proj).expect("create MainProj");
    fs::write(main_proj.join("MainProj.uproject"), br#"{"FileVersion": 3}"#).expect("uproject");
    run_git(&main_proj, &["init"]);
    run_git(&main_proj, &["config", "user.email", "t@example.com"]);
    run_git(&main_proj, &["config", "user.name", "Test"]);
    run_git(&main_proj, &["add", "MainProj.uproject"]);
    run_git(&main_proj, &["commit", "-m", "init"]);

    fs::create_dir_all(main_proj.join(r".git\lfs\objects\aa\bb")).expect("create lfs");
    fs::write(
        main_proj.join(r".git\lfs\objects\aa\bb\blob"),
        vec![9u8; 1024],
    )
    .expect("write lfs blob");

    run_git(
        &main_proj,
        &[
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            r"..\WtProj",
            "-b",
            "lane-test",
        ],
    );
    let wt_proj = env.dir.join("WtProj");
    fs::write(wt_proj.join("MainProj.uproject"), br#"{"FileVersion": 3}"#).expect("wt uproject");

    let chk_out = env.run_output(&[
        "disk",
        "checkup",
        "--path",
        &main_proj.display().to_string(),
        "--refresh",
        "--json",
    ]);
    assert!(chk_out.status.success());
    let chk: Value = serde_json::from_slice(&chk_out.stdout).expect("checkup json");
    let cid = chk["scan"]["checkup_id"].as_str().expect("cid");
    let items = chk["items"].as_array().expect("items");

    let lfs = items
        .iter()
        .find(|i| i["key"] == "git:lfs")
        .expect("git:lfs item");
    assert_eq!(lfs["class"], "review");

    let lane = items
        .iter()
        .find(|i| i["key"] == "lane")
        .expect("relative-gitdir worktree `lane` item (M2 F4.1d)");
    assert_eq!(lane["class"], "review");
    assert_eq!(lane["actions"], json!([]));
    assert!(
        lane["paths"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(|p| p.as_str().is_some_and(|s| s.contains("WtProj")))),
        "lane should reference WtProj: {}",
        lane
    );

    let lane_id = lane["id"].as_str().expect("lane id");
    let refuse_lane = env.run_output(&[
        "disk",
        "clean",
        "--fix",
        lane_id,
        "--yes",
        "--checkup",
        cid,
        "--json",
    ]);
    assert_eq!(refuse_lane.status.code(), Some(1));

    let catalog = fs::read_to_string(Path::new(MANIFEST_DIR).join(r"src\disk_catalog.toml"))
        .expect("read disk_catalog.toml");
    assert!(
        catalog.contains("cargo") || catalog.contains("pnpm"),
        "expected expanded M2 catalog entries (cargo/pnpm) in src/disk_catalog.toml"
    );
}

#[test]
fn t4_release_and_static_architecture_audit() {
    let manifest = Path::new(MANIFEST_DIR);

    // 1. Live CLI smoke in isolated TestDir.
    let env = TestDir::new("t4_static_audit");
    for args in [
        &["--version"][..],
        &["status", "--json"][..],
        &["ram", "--json"][..],
        &["drivers", "--json"][..],
        &["disk", "--json"][..],
    ] {
        let out = env.run_output(args);
        assert!(
            out.status.success(),
            "memguard {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        if args.contains(&"--json") {
            let _: Value = serde_json::from_slice(&out.stdout)
                .unwrap_or_else(|e| panic!("invalid JSON from {:?}: {}", args, e));
        }
    }

    // 2. Unsafe module allowlist across src/**/*.rs.
    let mut rs_files = Vec::new();
    collect_rs_files(&manifest.join("src"), &mut rs_files);
    let violations: Vec<String> = rs_files
        .iter()
        .filter_map(|p| {
            let rel = p
                .strip_prefix(manifest)
                .ok()?
                .display()
                .to_string()
                .replace('\\', "/");
            let src = fs::read_to_string(p).ok()?;
            let hits = scan_ident_tokens(&src, "unsafe");
            (!hits.is_empty() && !is_allowlisted_unsafe_path(&rel))
                .then(|| format!("{}: {:?}", rel, hits))
        })
        .collect();
    assert!(
        violations.is_empty(),
        "unsafe allowlist violations: {:?}",
        violations
    );

    // 3. Cargo.toml, Cargo.lock, src/mcp.rs, package.ps1, build.rs invariants.
    let cargo_toml = fs::read_to_string(manifest.join("Cargo.toml")).expect("Cargo.toml");
    validate_cargo_toml(&cargo_toml).expect("Cargo.toml invariants");

    let cargo_lock = fs::read_to_string(manifest.join("Cargo.lock")).expect("Cargo.lock");
    validate_cargo_lock(&cargo_lock).expect("Cargo.lock invariants");

    let mcp_rs = fs::read_to_string(manifest.join(r"src\mcp.rs")).expect("src/mcp.rs");
    assert!(scan_ident_tokens(&mcp_rs, "catch_unwind").is_empty());

    let package_ps1 = fs::read_to_string(manifest.join("package.ps1")).expect("package.ps1");
    assert!(package_ps1.contains("embed-cli"));
    assert!(
        package_ps1.contains("--no-default-features")
            || package_ps1
                .lines()
                .filter(|l| l.contains("AtherMemGuard") && l.contains("cargo"))
                .count()
                >= 2
    );

    let build_rs = fs::read_to_string(manifest.join("build.rs")).expect("build.rs");
    assert!(build_rs.contains(r#"level="asInvoker""#));

    if std::env::var("MEMGUARD_CHECK_DIST").as_deref() == Ok("1") {
        let pkg_doc = parse_simple_toml(&cargo_toml).expect("parse Cargo.toml for package version");
        let pkg_ver = pkg_doc
            .tables
            .get("package")
            .and_then(|t| t.get("version"))
            .and_then(|v| unquote_toml_str(v))
            .expect("package.version in Cargo.toml");
        let setup_exe = manifest
            .join("dist")
            .join(format!("AtherMemGuard-Setup-{}.exe", pkg_ver));
        let meta = fs::metadata(&setup_exe).expect("dist setup exe metadata");
        assert!(
            meta.len() < 4 * 1024 * 1024,
            "Setup exe exceeds 4 MB target: {} bytes",
            meta.len()
        );
        let latest_json: Value = serde_json::from_str(
            &fs::read_to_string(manifest.join(r"dist\latest.json")).expect("read dist/latest.json"),
        )
        .expect("parse dist/latest.json");
        assert_eq!(latest_json["windows"]["version"], pkg_ver);
    }
}

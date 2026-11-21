//! Opaque-box E2E tests for Feature Areas F1–F4 (Tiers 1 & 2):
//! - F1: CLI Core & Diagnostics (`status`, `advise`, `prime`, `wait`, `run`, `history`, `ram`, `drivers`, tray unknown flag)
//! - F2: Layered Config (`config`, `BUILTIN < team.toml < settings.toml`, clamping, `.exe` stripping, `[disk]`/`[doctor]`, Bug #18 / `team.toml` manifest_url config layering; `updater.rs` Bug #13 is in `tests/update_flow.rs`)
//! - F3: Memory Engine, Foreground Grace, Pressure Split & UE 5.4+ (daemon, `run`, `prime`, UE 5.4+, Bugs #2/#4/#14)
//! - F4: Doctor & Process Safety (`doctor`, `self_tree`, `close`/`end`/`undo`, `drivers`, Bugs #16/#17)
//!
//! Strictly opaque-box: uses ONLY `std`, `serde_json`, and `env!("CARGO_BIN_EXE_memguard")`.
//! Every test runs in a unique `ATHER_MEMGUARD_DIR` temp directory, disables all 4 built-in groups
//! when running `prime` / `wait --prime` / `daemon`, and cleans up spawned processes via RAII Drop.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static SEQ: AtomicU64 = AtomicU64::new(1);

/// `paths::mutex()` (`Local\AtherMemGuard.Instance.Test`) and `paths::exit_event()`
/// (`Local\AtherMemGuard.Exit.Test`) are shared across all `ATHER_MEMGUARD_DIR` values on baseline.
/// Serialize all tests that start/stop a daemon or assert `daemon == false`.
static DAEMON_MUTEX: Mutex<()> = Mutex::new(());

fn lock_daemon() -> MutexGuard<'static, ()> {
    DAEMON_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("amg_e2e_cli_{}_{}_{}", std::process::id(), seq, tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create isolated temp dir");
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(&p, content).expect("write temp file");
        p
    }

    fn read_to_string(&self, rel: &str) -> String {
        fs::read_to_string(self.join(rel)).expect("read temp file")
    }

    fn read_bytes(&self, rel: &str) -> Vec<u8> {
        fs::read(self.join(rel)).expect("read temp bytes")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..40 {
            if fs::remove_dir_all(&self.path).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct KillGuard(Option<Child>);

impl KillGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn id(&self) -> u32 {
        self.0.as_ref().expect("child present").id()
    }

    fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.0.as_mut().expect("child present").try_wait().expect("try_wait")
    }

    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.try_wait().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        false
    }
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Fallback cleanup by unique copied image name for processes relaunched by `doctor --undo`.
struct ProcessNameCleanupGuard {
    exe_name: String,
}

impl ProcessNameCleanupGuard {
    fn new(exe_name: impl Into<String>) -> Self {
        Self { exe_name: exe_name.into() }
    }
}

impl Drop for ProcessNameCleanupGuard {
    fn drop(&mut self) {
        if let Ok(mut child) = Command::new("taskkill")
            .args(["/F", "/IM", &self.exe_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if child.try_wait().ok().flatten().is_some() {
                    std::thread::sleep(Duration::from_millis(100));
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct DaemonGuard {
    dir: PathBuf,
    child: Option<Child>,
}

impl DaemonGuard {
    fn spawn_with_stdout(dir: &Path, stdout_path: &Path) -> Self {
        let _ = fs::remove_file(dir.join("status.json"));
        let out_file = fs::File::create(stdout_path).expect("create daemon stdout capture file");
        let child = Command::new(memguard_exe())
            .arg("daemon")
            .env("ATHER_MEMGUARD_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn memguard daemon");
        Self { dir: dir.to_path_buf(), child: Some(child) }
    }

    fn check_alive(&mut self) {
        if let Some(c) = self.child.as_mut() {
            if let Some(st) = c.try_wait().expect("poll daemon child") {
                panic!("memguard daemon exited prematurely with {:?}", st);
            }
        }
    }

    fn start(dir: &Path) -> Self {
        let _ = fs::remove_file(dir.join("status.json"));
        let child = Command::new(memguard_exe())
            .arg("daemon")
            .env("ATHER_MEMGUARD_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn memguard daemon");

        let mut guard = Self { dir: dir.to_path_buf(), child: Some(child) };
        let start = Instant::now();
        loop {
            guard.check_alive();
            if dir.join("status.json").is_file() {
                let out = run_memguard(dir, &["status", "--json"]);
                if out.status.success() {
                    let v = parse_json(&out);
                    if v["daemon"].as_bool() == Some(true) {
                        return guard;
                    }
                }
            }
            assert!(start.elapsed() < Duration::from_secs(12), "timed out waiting for memguard daemon to become ready");
            std::thread::sleep(Duration::from_millis(120));
        }
    }

    fn stop(&mut self) -> Output {
        let out = run_memguard(&self.dir, &["stop"]);
        if let Some(mut child) = self.child.take() {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if child.try_wait().ok().flatten().is_some() {
                    return out;
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        out
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.stop();
        }
    }
}

fn memguard_exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_memguard"))
}

fn unique_tag(prefix: &str) -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}_{}", prefix, std::process::id(), seq)
}

struct IoFileCleanup(PathBuf, PathBuf);

impl Drop for IoFileCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(&self.1);
    }
}

/// Run a child command with file-redirected stdout/stderr and a bounded `try_wait()` timeout.
/// Avoids `Command::output()` pipe deadlocks on Windows when a spawned grandchild (such as
/// `memguard doctor --undo` relaunching a detached GUI app) inherits handles.
fn run_command_bounded(mut cmd: Command, timeout: Duration, label: &str) -> Output {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir();
    let out_path = base.join(format!("amg_e2e_out_{}_{}.tmp", std::process::id(), seq));
    let err_path = base.join(format!("amg_e2e_err_{}_{}.tmp", std::process::id(), seq));
    let _cleanup = IoFileCleanup(out_path.clone(), err_path.clone());

    let out_file = fs::File::create(&out_path).expect("create stdout capture file");
    let err_file = fs::File::create(&err_path).expect("create stderr capture file");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));

    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {}: {}", label, e));
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap_or_else(|e| panic!("try_wait {}: {}", label, e)) {
            break st;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let out_so_far = fs::read(&out_path).unwrap_or_default();
            let err_so_far = fs::read(&err_path).unwrap_or_default();
            panic!(
                "{} timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                label,
                timeout,
                String::from_utf8_lossy(&out_so_far),
                String::from_utf8_lossy(&err_so_far)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = fs::read(&out_path).unwrap_or_default();
    let stderr = fs::read(&err_path).unwrap_or_default();
    Output { status, stdout, stderr }
}

fn run_memguard(dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(memguard_exe());
    cmd.args(args)
        .env("ATHER_MEMGUARD_DIR", dir)
        .creation_flags(CREATE_NO_WINDOW);
    run_command_bounded(cmd, Duration::from_secs(45), &format!("memguard {:?}", args))
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn parse_json(out: &Output) -> Value {
    let text = stdout_str(out);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("invalid JSON ({}):\n{}\nstderr:\n{}", e, text, stderr_str(out)))
}

fn copy_sys_exe(dir: &Path, rel_src: &str, name_without_ext: &str) -> PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
    let src = PathBuf::from(windir).join(rel_src);
    let dst = dir.join(format!("{}.exe", name_without_ext));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).expect("create exe parent dir");
    }
    fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {} -> {}: {}", src.display(), dst.display(), e));
    dst
}

fn spawn_ping_dummy(dir: &Path, name: &str) -> KillGuard {
    let exe = copy_sys_exe(dir, r"System32\PING.EXE", name);
    let child = Command::new(&exe)
        .args(["-t", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("spawn ping dummy");
    KillGuard::new(child)
}

fn daemon_tick_lines(stdout_path: &Path) -> Vec<String> {
    fs::read_to_string(stdout_path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains("pressure "))
        .map(str::to_string)
        .collect()
}

fn spawn_windowed_gui_dummy(dir: &Path, exe_name: &str, window_title: &str) -> KillGuard {
    fs::create_dir_all(dir).expect("create gui dummy dir");
    let exe = dir.join(format!("{}.exe", exe_name));
    let mut csharp_title = String::with_capacity(window_title.len());
    for ch in window_title.chars() {
        match ch {
            '\\' => csharp_title.push_str("\\\\"),
            '"' => csharp_title.push_str("\\\""),
            '\r' => csharp_title.push_str("\\r"),
            '\n' => csharp_title.push_str("\\n"),
            '\t' => csharp_title.push_str("\\t"),
            '\0' => csharp_title.push_str("\\0"),
            '\'' => csharp_title.push_str("''"),
            _ => csharp_title.push(ch),
        }
    }
    let ps = format!(
        "$code = 'using System; using System.Windows.Forms; public class Program {{ [STAThread] public static void Main() {{ var f = new Form(); f.Text = \"{}\"; f.WindowState = FormWindowState.Minimized; Application.Run(f); }} }}'; \
         Add-Type -TypeDefinition $code -OutputAssembly '{}' -OutputType WindowsApplication -ReferencedAssemblies System.Windows.Forms",
        csharp_title,
        exe.display().to_string().replace('\'', "''")
    );
    let mut build_cmd = Command::new("powershell.exe");
    build_cmd
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW);
    let build = run_command_bounded(build_cmd, Duration::from_secs(45), "powershell Add-Type");
    assert!(
        build.status.success(),
        "Add-Type failed: {}\n{}",
        stdout_str(&build),
        stderr_str(&build)
    );
    let child = Command::new(&exe)
        .args(["--e2e-arg", "hello"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn windowed gui dummy");
    KillGuard::new(child)
}

fn query_priority_class(pid: u32) -> String {
    let script = format!("(Get-Process -Id {}).PriorityClass", pid);
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW);
    let out = run_command_bounded(cmd, Duration::from_secs(30), "powershell PriorityClass");
    assert!(out.status.success(), "powershell PriorityClass query failed: {}", stderr_str(&out));
    stdout_str(&out).trim().to_string()
}

fn disabled_builtins_toml() -> &'static str {
    r#"
[general]
check_ram_speed = false

[disk]
enabled = false
check_dev_drive = false

[[group]]
name = "Browsers"
enabled = false

[[group]]
name = "Chat"
enabled = false

[[group]]
name = "Agents"
enabled = false

[[group]]
name = "Background"
enabled = false
"#
}

fn spawn_cap_loop_dummy(dir: &Path, name: &str) -> (KillGuard, ChildStdin, BufReader<ChildStdout>) {
    let exe = copy_sys_exe(dir, r"System32\WindowsPowerShell\v1.0\powershell.exe", name);
    let script = r#"
[Console]::Out.WriteLine('READY'); [Console]::Out.Flush()
while ($null -ne ($line = [Console]::In.ReadLine())) {
    if ($line -eq 'alloc') {
        try {
            $b = New-Object byte[] (160MB)
            $b[0] = 1
            $b = $null
            [GC]::Collect()
            [Console]::Out.WriteLine('OK')
        } catch {
            [Console]::Out.WriteLine('OOM')
        }
        [Console]::Out.Flush()
    } elseif ($line -eq 'quit') {
        break
    }
}
"#;
    let mut child = Command::new(&exe)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .expect("spawn cap loop dummy");

    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read READY");
    assert_eq!(line.trim(), "READY", "cap loop dummy failed to initialize");
    (KillGuard::new(child), stdin, reader)
}

fn probe_alloc(stdin: &mut ChildStdin, reader: &mut BufReader<ChildStdout>) -> String {
    writeln!(stdin, "alloc").expect("write alloc");
    stdin.flush().expect("flush alloc");
    let mut resp = String::new();
    reader.read_line(&mut resp).expect("read alloc response");
    resp.trim().to_string()
}

// ============================================================================
// F1: CLI Core & Diagnostics (7 Tier 1 + 7 Tier 2)
// ============================================================================

#[test]
fn t1_f1_status_text_and_json() {
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t1_f1_status");

    let text_out = run_memguard(dir.path(), &["status"]);
    assert_eq!(text_out.status.code(), Some(0), "stderr: {}", stderr_str(&text_out));
    let text = stdout_str(&text_out);
    assert!(text.contains("Ather MemGuard"), "missing header:\n{}", text);
    assert!(text.contains("background app: NOT RUNNING"), "missing stopped state:\n{}", text);
    assert!(text.contains("Policy now"), "missing policy section:\n{}", text);

    let json_out = run_memguard(dir.path(), &["status", "--json"]);
    assert_eq!(json_out.status.code(), Some(0), "stderr: {}", stderr_str(&json_out));
    let v = parse_json(&json_out);

    assert!(!v["time"].as_str().unwrap_or("").is_empty());
    assert_eq!(v["daemon"].as_bool(), Some(false));
    assert_eq!(v["paused"].as_bool(), Some(false));
    assert!(v["mode"].is_string());
    assert!(v["activities"]["editor"].is_boolean());
    assert!(v["activities"]["build"].is_boolean());
    assert!(v["activities"]["cook"].is_boolean());
    let pressure = v["pressure"].as_str().expect("pressure string");
    assert!(matches!(pressure, "normal" | "elevated" | "critical"), "unexpected pressure: {}", pressure);

    assert!(v["system"]["ram_total_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["system"]["ram_free_gb"].as_f64().is_some());
    assert!(v["system"]["ram_free_pct"].as_f64().is_some());
    assert!(v["system"]["commit_limit_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["system"]["commit_used_gb"].as_f64().is_some());
    assert!(v["system"]["commit_free_gb"].as_f64().is_some());
    assert!(v["system"]["commit_free_pct"].as_f64().is_some());
    assert!(v["system"]["file_cache_gb"].as_f64().is_some());
    assert!(v["system"]["nonpaged_pool_gb"].as_f64().is_some());

    assert!(v["ue"]["processes"].is_array());
    assert!(v["ue"]["workers"].is_u64());
    assert!(v["ue"]["workers_committed_gb"].is_f64());
    assert!(v["ue"]["workers_paused"].is_u64());
    assert!(v["ue"]["total_committed_gb"].is_f64());
    assert!(v["ue"]["total_ram_gb"].is_f64());

    let groups = v["groups"].as_array().expect("groups array");
    assert_eq!(groups.len(), 4, "expected 4 built-in groups: {:?}", groups);
    for g in groups {
        assert!(g["name"].is_string());
        assert!(g["processes"].is_u64());
        assert!(g["ram_mb"].is_u64());
        assert!(g["committed_mb"].is_u64());
        assert!(g["policy"]["priority"].is_string());
        assert!(g["policy"]["trim_above_mb"].is_u64());
        assert!(g["policy"]["cpu"].is_string());
        assert!(g["policy"]["suspend"].is_boolean());
        assert!(g["hard_cap_mb"].is_u64());
        assert!(g["hard_cap_per_process_mb"].is_u64());
        assert!(g["suspended"].is_boolean());
    }

    assert!(v["ram"]["in_use_gb"].is_f64());
    assert!(v["ram"]["unreal_gb"].is_f64());
    assert!(v["ram"]["groups"].is_array());
    assert!(v["ram"]["other_apps_gb"].is_f64());
    assert!(v["ram"]["other_apps_count"].is_u64());
    assert!(v["ram"]["file_cache_gb"].is_f64());
    assert!(v["ram"]["windows_gb"].is_f64());
    assert!(v["ram"]["compressed_gb"].is_f64());
    assert!(v["ram"]["unattributed_gb"].is_f64());
    assert!(v["advice"].is_array());
    assert!(v["disk"]["volumes"].is_array());
}

#[test]
fn t1_f1_advise_text_and_json() {
    let dir = TempDir::new("t1_f1_advise");

    let text_out = run_memguard(dir.path(), &["advise"]);
    assert_eq!(text_out.status.code(), Some(0));
    let text = stdout_str(&text_out);
    assert!(text.contains("Mode "), "missing Mode:\n{}", text);
    assert!(text.contains("Suggested compile parallelism:"), "missing parallelism:\n{}", text);

    let json_out = run_memguard(dir.path(), &["advise", "--json"]);
    assert_eq!(json_out.status.code(), Some(0));
    let v = parse_json(&json_out);

    assert!(v["mode"].is_string());
    let pressure = v["pressure"].as_str().expect("pressure");
    assert!(matches!(pressure, "normal" | "elevated" | "critical"));
    let cores = v["logical_cores"].as_u64().expect("logical_cores");
    assert!(cores >= 1);
    assert!(v["ram_budget_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(v["commit_free_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    let suggested = v["suggested_max_parallel_actions"].as_u64().expect("suggested");
    if cores >= 2 {
        assert!(suggested >= 2 && suggested <= cores, "suggested {} out of 2..={}", suggested, cores);
    } else {
        assert!(suggested >= 1 && suggested <= cores);
    }
    assert!(v["notes"].is_array());
}

#[test]
fn t1_f1_prime_for_workload_ready() {
    let dir = TempDir::new("t1_f1_prime");
    let toml = format!(
        "{}\n[prime.editor]\nram_gb = 0.01\ncommit_gb = 0.01\ndisk_gb = 0.0\n",
        disabled_builtins_toml()
    );
    dir.write("settings.toml", &toml);

    let json_out = run_memguard(dir.path(), &["prime", "--for", "editor", "--json"]);
    assert_eq!(json_out.status.code(), Some(0), "stderr: {}", stderr_str(&json_out));
    let v = parse_json(&json_out);
    assert_eq!(v["trimmed_processes"].as_u64(), Some(0));
    assert!(v["before"]["ram_total_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["after"]["ram_total_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["orphans"].is_array());
    assert!(v["advice"].is_array());
    assert_eq!(v["readiness"]["target"].as_str(), Some("editor"));
    assert_eq!(v["readiness"]["ready"].as_bool(), Some(true));

    let text_out = run_memguard(dir.path(), &["prime"]);
    assert_eq!(text_out.status.code(), Some(0));
    let text = stdout_str(&text_out);
    assert!(text.contains("Trimmed 0 processes"), "{}", text);
    assert!(text.contains("RAM free:"), "{}", text);
}

#[test]
fn t1_f1_wait_valid_target_succeeds() {
    let dir = TempDir::new("t1_f1_wait_ok");
    let dir_str = dir.path().to_str().expect("utf8 path");
    let out = run_memguard(
        dir.path(),
        &[
            "wait",
            "--ram-gb",
            "0.01",
            "--commit-gb",
            "0.01",
            "--disk-path",
            dir_str,
            "--disk-gb",
            "0.001",
            "--timeout",
            "5",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_str(&out));
    let v = parse_json(&out);
    assert_eq!(v["ready"].as_bool(), Some(true));
    assert_eq!(v["waited_sec"].as_u64(), Some(0));
    assert_eq!(v["blocked_by"].as_array().map(|a| a.len()), Some(0));
    let checks = v["disk_checks"].as_array().expect("disk_checks");
    assert!(!checks.is_empty());
    assert!(checks.iter().all(|c| c["ok"].as_bool() == Some(true)));
}

#[test]
fn t1_f1_run_exit_code_and_policy_flags() {
    let dir = TempDir::new("t1_f1_run_flags");

    let out0 = run_memguard(dir.path(), &["run", "--", "cmd.exe", "/c", "exit", "0"]);
    assert_eq!(out0.status.code(), Some(0));

    let out42 = run_memguard(
        dir.path(),
        &[
            "run",
            "--priority",
            "verylow",
            "--cpu",
            "eco",
            "--cap-gb",
            "1",
            "--per-process-cap-gb",
            "0.5",
            "--",
            "cmd.exe",
            "/c",
            "echo run_ok & exit 42",
        ],
    );
    assert_eq!(out42.status.code(), Some(42), "stderr: {}", stderr_str(&out42));
    assert!(stdout_str(&out42).contains("run_ok"));

    let out7 = run_memguard(dir.path(), &["run", "--priority", "low", "--cpu", "below", "--", "cmd.exe", "/c", "exit", "7"]);
    assert_eq!(out7.status.code(), Some(7));

    let out3 = run_memguard(dir.path(), &["run", "--cpu", "idle", "--", "cmd.exe", "/c", "exit", "3"]);
    assert_eq!(out3.status.code(), Some(3));
}

#[test]
fn t1_f1_history_seeded_csv_table_and_json() {
    let dir = TempDir::new("t1_f1_history");
    let st_out = run_memguard(dir.path(), &["status", "--json"]);
    assert_eq!(st_out.status.code(), Some(0));
    let st = parse_json(&st_out);
    let now_str = st["time"].as_str().expect("status time").to_string();
    let date = &now_str[..10];

    let csv = format!(
        "time,mode,pressure,ram_free_gb,commit_free_gb,ue_committed_gb,editor_committed_gb,workers,Browsers\n\
         {},idle,normal,32.50,40.00,0.00,0.00,0,1.25\n",
        now_str
    );
    dir.write(&format!("history\\{}.csv", date), &csv);

    let json_out = run_memguard(dir.path(), &["history", "--hours", "2", "--json"]);
    assert_eq!(json_out.status.code(), Some(0));
    let arr = parse_json(&json_out).as_array().expect("history array").clone();
    assert_eq!(arr.len(), 1, "expected 1 seeded sample: {:?}", arr);
    let s = &arr[0];
    assert!(s["unix"].as_i64().unwrap_or(0) > 0);
    assert_eq!(s["time"].as_str(), Some(now_str.as_str()));
    assert_eq!(s["mode"].as_str(), Some("idle"));
    assert_eq!(s["pressure"].as_str(), Some("normal"));
    assert_eq!(s["ram_free_gb"].as_f64(), Some(32.5));
    assert_eq!(s["commit_free_gb"].as_f64(), Some(40.0));
    assert_eq!(s["workers"].as_u64(), Some(0));
    assert_eq!(s["groups"][0]["name"].as_str(), Some("Browsers"));
    assert_eq!(s["groups"][0]["ram_gb"].as_f64(), Some(1.25));

    let text_out = run_memguard(dir.path(), &["history", "--hours", "2"]);
    assert_eq!(text_out.status.code(), Some(0));
    let text = stdout_str(&text_out);
    assert!(text.contains("RAM free"), "{}", text);
    assert!(text.contains("1 samples"), "{}", text);
}

#[test]
fn t1_f1_ram_and_drivers_json_and_html() {
    let dir = TempDir::new("t1_f1_ram_drivers");
    let html_path = dir.join("ram.html");
    let html_str = html_path.to_str().expect("utf8 html path");

    let ram_out = run_memguard(dir.path(), &["ram", "--json", "--html", html_str]);
    assert_eq!(ram_out.status.code(), Some(0), "stderr: {}", stderr_str(&ram_out));
    let ram_v = parse_json(&ram_out);
    let verdict = ram_v["verdict"].as_str().expect("ram verdict");
    assert!(
        matches!(verdict, "below_rated" | "at_rated" | "no_profile" | "mixed_kit" | "unknown" | "not_applicable"),
        "unexpected verdict: {}",
        verdict
    );
    assert!(ram_v["headline"].is_string());
    assert!(ram_v["summary"].is_string());
    assert!(ram_v["sticks"].is_array());
    assert!(ram_v["slots_total"].is_u64());
    assert!(ram_v["board"].is_object());
    assert!(ram_v["board"]["maker"].is_string());
    assert!(ram_v["cpu"].is_object());
    assert!(ram_v["cpu"]["name"].is_string());
    assert!(ram_v["findings"].is_array());
    assert!(ram_v["steps"].is_array());
    assert!(ram_v["fingerprint"].is_string());

    let html = fs::read_to_string(&html_path).expect("read ram.html");
    assert!(html.starts_with("<!doctype html>"), "unexpected html start: {}", &html[..html.len().min(64)]);

    let drv_out = run_memguard(dir.path(), &["drivers", "--json"]);
    assert_eq!(drv_out.status.code(), Some(0));
    let drv_v = parse_json(&drv_out);
    assert!(drv_v["nonpaged_pool_gb"].as_f64().is_some());
    assert!(drv_v["paged_pool_gb"].as_f64().is_some());
    assert!(drv_v["system_code_gb"].as_f64().is_some());
    assert!(drv_v["drivers_code_gb"].as_f64().is_some());
    assert!(drv_v["windows_processes_gb"].as_f64().is_some());
    assert!(drv_v["total_gb"].as_f64().is_some());
    assert!(drv_v["processes"].is_array());
    assert!(drv_v["tags_available"].is_boolean());
    assert!(drv_v["tags"].is_array());
    assert!(!drv_v["verdict"].as_array().expect("verdict array").is_empty());
}

#[test]
fn t2_f1_wait_zero_targets_exits_1() {
    let dir = TempDir::new("t2_f1_wait_zero");
    let out = run_memguard(dir.path(), &["wait", "--ram-gb", "0", "--commit-gb", "0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_str(&out).contains("Say what to wait for"));
}

#[test]
fn t2_f1_wait_impossible_disk_short_circuits_exits_2() {
    let dir = TempDir::new("t2_f1_wait_disk");
    // Disable the 4 built-in groups while keeping [disk] enabled so the disk gate runs.
    dir.write(
        "settings.toml",
        r#"
[[group]]
name = "Browsers"
enabled = false
[[group]]
name = "Chat"
enabled = false
[[group]]
name = "Agents"
enabled = false
[[group]]
name = "Background"
enabled = false
"#,
    );
    let dir_str = dir.path().to_str().expect("utf8 path");

    let start = Instant::now();
    let out = run_memguard(
        dir.path(),
        &[
            "wait",
            "--disk-path",
            dir_str,
            "--disk-gb",
            "1000000000",
            "--prime",
            "--timeout",
            "30",
            "--json",
        ],
    );
    assert!(start.elapsed() < Duration::from_secs(5), "disk gate did not short-circuit");
    assert_eq!(out.status.code(), Some(2));
    let v = parse_json(&out);
    assert_eq!(v["ready"].as_bool(), Some(false));
    assert_eq!(v["waited_sec"].as_u64(), Some(0));
    assert_eq!(v["blocked_by"], serde_json::json!(["disk"]));
    assert_eq!(v["need_disk_gb"].as_f64(), Some(1_000_000_000.0));
    let checks = v["disk_checks"].as_array().expect("disk_checks");
    assert!(checks.iter().any(|c| c["ok"].as_bool() == Some(false)));
}

#[test]
fn t2_f1_wait_impossible_memory_timeout_0_exits_2() {
    let dir = TempDir::new("t2_f1_wait_mem0");
    let out = run_memguard(dir.path(), &["wait", "--ram-gb", "999999", "--timeout", "0", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let v = parse_json(&out);
    assert_eq!(v["ready"].as_bool(), Some(false));
    assert_eq!(v["waited_sec"].as_u64(), Some(0));
    assert_eq!(v["blocked_by"], serde_json::json!(["memory"]));
}

#[test]
fn t2_f1_prime_impossible_target_exits_2() {
    let dir = TempDir::new("t2_f1_prime_fail");
    let toml = format!(
        "{}\n[prime.cook]\nram_gb = 999999\ncommit_gb = 999999\ndisk_gb = 0.0\n",
        disabled_builtins_toml()
    );
    dir.write("settings.toml", &toml);

    let out = run_memguard(dir.path(), &["prime", "--for", "cook", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let v = parse_json(&out);
    assert_eq!(v["readiness"]["target"].as_str(), Some("cook"));
    assert_eq!(v["readiness"]["ready"].as_bool(), Some(false));
}

#[test]
fn t2_f1_run_missing_cmd_125_and_exit_mask() {
    let dir = TempDir::new("t2_f1_run_err");

    let missing = run_memguard(dir.path(), &["run", "--", "__e2e_missing_binary_xyz__.exe"]);
    assert_eq!(missing.status.code(), Some(125));
    assert!(stderr_str(&missing).contains("memguard run:"));

    let masked = run_memguard(dir.path(), &["run", "--", "cmd.exe", "/c", "exit", "257"]);
    assert_eq!(masked.status.code(), Some(1));
}

#[test]
fn t2_f1_history_hours_0_clamp_and_ram_bad_html() {
    let dir = TempDir::new("t2_f1_hist0_ram");

    let hist_text = run_memguard(dir.path(), &["history", "--hours", "0"]);
    assert_eq!(hist_text.status.code(), Some(0));
    assert!(stdout_str(&hist_text).contains("last 1 h"), "{}", stdout_str(&hist_text));

    let hist_json = run_memguard(dir.path(), &["history", "--hours", "0", "--json"]);
    assert_eq!(hist_json.status.code(), Some(0));
    assert_eq!(parse_json(&hist_json), serde_json::json!([]));

    let bad_html = dir.join("no_such_dir\\out.html");
    let bad_html_str = bad_html.to_str().expect("utf8 path");
    let ram_out = run_memguard(dir.path(), &["ram", "--html", bad_html_str, "--json"]);
    assert_eq!(ram_out.status.code(), Some(0));
    assert!(stderr_str(&ram_out).contains("Couldn't write"), "{}", stderr_str(&ram_out));
    let v = parse_json(&ram_out);
    assert!(v["verdict"].is_string());
}

#[test]
fn t2_f1_tray_exe_unknown_flag_exits_2() {
    let tray = memguard_exe().with_file_name("AtherMemGuard.exe");
    if !tray.is_file() {
        return;
    }
    let dir = TempDir::new("t2_f1_tray_unk");
    let mut cmd = Command::new(&tray);
    cmd.arg("--e2e-unknown-flag")
        .env("ATHER_MEMGUARD_DIR", dir.path())
        .creation_flags(CREATE_NO_WINDOW);
    let out = run_command_bounded(cmd, Duration::from_secs(30), "AtherMemGuard.exe --e2e-unknown-flag");
    assert_eq!(out.status.code(), Some(2));
    let log = dir.read_to_string("memguard.log");
    assert!(log.contains("didn't understand"), "unexpected log:\n{}", log);
}

// ============================================================================
// F2: Layered Config & Updater (6 Tier 1 + 6 Tier 2)
// ============================================================================

#[test]
fn t1_f2_config_path_check_and_reset() {
    let dir = TempDir::new("t1_f2_cfg_basic");
    let expected_path = dir.join("settings.toml");

    let p_out = run_memguard(dir.path(), &["config"]);
    assert_eq!(p_out.status.code(), Some(0));
    assert_eq!(stdout_str(&p_out).trim(), expected_path.display().to_string());

    let c_out = run_memguard(dir.path(), &["config", "--check"]);
    assert_eq!(c_out.status.code(), Some(0), "stderr: {}", stderr_str(&c_out));
    assert!(stdout_str(&c_out).contains("is valid: 4 groups."), "{}", stdout_str(&c_out));

    let s_out = run_memguard(dir.path(), &["config", "set", "general.preset", "gentle"]);
    assert_eq!(s_out.status.code(), Some(0));

    let r_out = run_memguard(dir.path(), &["config", "--reset"]);
    assert_eq!(r_out.status.code(), Some(0));
    assert!(dir.read_to_string("settings.toml.bak").contains("gentle"));

    let show_out = run_memguard(dir.path(), &["config", "show"]);
    assert_eq!(show_out.status.code(), Some(0));
    assert!(stdout_str(&show_out).contains("preset = \"balanced\""));
}

#[test]
fn t1_f2_config_layering_team_user_reset() {
    let dir = TempDir::new("t1_f2_layering");
    let team_src = dir.write(
        "team_src.toml",
        r#"
[general]
notifications = false

[updates]
manifest_url = "https://team.example.com/latest.json"

[[group]]
name = "Browsers"
trim_above_mb = 3072

[[group]]
name = "TeamTools"
processes = ["obs64"]
"#,
    );
    let team_src_str = team_src.to_str().expect("utf8");
    let t_out = run_memguard(dir.path(), &["config", "team", team_src_str]);
    assert_eq!(t_out.status.code(), Some(0), "stderr: {}", stderr_str(&t_out));

    assert_eq!(run_memguard(dir.path(), &["config", "set", "general.preset", "aggressive"]).status.code(), Some(0));
    assert_eq!(run_memguard(dir.path(), &["config", "set", "group.Browsers.trim_above_pct", "3.5"]).status.code(), Some(0));

    let show1 = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show1.contains("trim_cooldown_sec = 120"), "{}", show1);
    assert!(show1.contains("notifications = false"), "{}", show1);
    assert!(show1.contains("manifest_url = \"https://team.example.com/latest.json\""), "{}", show1);
    assert!(show1.contains("name = \"TeamTools\""), "{}", show1);
    assert!(show1.contains("preset = \"aggressive\""), "{}", show1);
    assert!(show1.contains("trim_above_pct = 3.5"), "{}", show1);
    assert!(!show1.contains("trim_above_mb = 3072"), "user trim_above_pct should replace team trim_above_mb:\n{}", show1);

    let reset_pct = run_memguard(dir.path(), &["config", "reset", "group.Browsers.trim_above_pct"]);
    assert_eq!(reset_pct.status.code(), Some(0));
    assert!(stdout_str(&reset_pct).contains("team.toml"), "{}", stdout_str(&reset_pct));

    let show2 = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show2.contains("trim_above_mb = 3072"), "team trim_above_mb should return:\n{}", show2);

    let reset_preset = run_memguard(dir.path(), &["config", "reset", "general.preset"]);
    assert_eq!(reset_preset.status.code(), Some(0));
    assert!(stdout_str(&reset_preset).contains("built-in default"), "{}", stdout_str(&reset_preset));
}

#[test]
fn t1_f2_config_export_and_import_bak() {
    let dir = TempDir::new("t1_f2_exp_imp");
    assert_eq!(run_memguard(dir.path(), &["config", "set", "general.preset", "gentle"]).status.code(), Some(0));
    assert_eq!(run_memguard(dir.path(), &["config", "set", "disk.scan_depth", "5"]).status.code(), Some(0));

    let exp = dir.join("exp.toml");
    let exp_str = exp.to_str().expect("utf8");
    let e_out = run_memguard(dir.path(), &["config", "export", exp_str]);
    assert_eq!(e_out.status.code(), Some(0), "stderr: {}", stderr_str(&e_out));

    assert_eq!(run_memguard(dir.path(), &["config", "--reset"]).status.code(), Some(0));
    let i_out = run_memguard(dir.path(), &["config", "import", exp_str]);
    assert_eq!(i_out.status.code(), Some(0), "stderr: {}", stderr_str(&i_out));
    assert!(dir.join("settings.toml.bak").is_file());

    let show = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show.contains("preset = \"gentle\""), "{}", show);
    assert!(show.contains("scan_depth = 5"), "{}", show);
}

#[test]
fn t1_f2_config_set_preserves_comments_and_disk_doctor_fields() {
    let dir = TempDir::new("t1_f2_comments");
    dir.write("settings.toml", "# E2E-COMMENT\n[general]\npreset = \"balanced\"\n");

    for args in [
        ["config", "set", "interval_sec", "15"],
        ["config", "set", "disk.hold_hours", "48"],
        ["config", "set", "disk.elevated_free_gb", "200"],
        ["config", "set", "doctor.ignore_below_mb", "250"],
        ["config", "set", "doctor.nudge_above_gb", "6.5"],
    ] {
        let out = run_memguard(dir.path(), &args);
        assert_eq!(out.status.code(), Some(0), "args={:?} stderr={}", args, stderr_str(&out));
    }

    let raw = dir.read_to_string("settings.toml");
    assert!(raw.contains("# E2E-COMMENT"), "comment lost:\n{}", raw);

    let show = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show.contains("interval_sec = 15"), "{}", show);
    assert!(show.contains("hold_hours = 48"), "{}", show);
    assert!(show.contains("elevated_free_gb = 200"), "{}", show);
    assert!(show.contains("ignore_below_mb = 250"), "{}", show);
    assert!(show.contains("nudge_above_gb = 6.5"), "{}", show);

    assert_eq!(run_memguard(dir.path(), &["config", "--check"]).status.code(), Some(0));
}

#[test]
fn t1_f2_config_exe_suffix_stripping_via_status() {
    let dir = TempDir::new("t1_f2_exestrip");
    let tag = unique_tag("E2EM1ExeStrip");
    let _dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(200));

    let toml = format!(
        "[[group]]\nname = \"E2EExeStrip\"\nprocesses = [\"{}.EXE\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let out = run_memguard(dir.path(), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = parse_json(&out);
    let grp = v["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"].as_str() == Some("E2EExeStrip"))
        .expect("E2EExeStrip group");
    assert_eq!(grp["processes"].as_u64(), Some(1));
}

#[test]
fn t1_f2_config_trim_above_mb_vs_pct_and_interval_clamp() {
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t1_f2_mb_pct");
    dir.write(
        "settings.toml",
        r#"
[[group]]
name = "E2EM1MbVsPct"
processes = ["__e2e_none__"]
trim_above_mb = 2048
trim_above_pct = 50.0
"#,
    );

    let chk = run_memguard(dir.path(), &["config", "--check"]);
    assert_eq!(chk.status.code(), Some(0));
    assert!(
        stdout_str(&chk).contains("sets both trim_above_mb and trim_above_pct; trim_above_mb is used"),
        "{}",
        stdout_str(&chk)
    );

    let st = parse_json(&run_memguard(dir.path(), &["status", "--json"]));
    let grp = st["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"].as_str() == Some("E2EM1MbVsPct"))
        .expect("E2EM1MbVsPct");
    assert_eq!(grp["policy"]["trim_above_mb"].as_u64(), Some(2048));

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let out_low = std::env::temp_dir().join(format!("amg_e2e_out_{}_{}_lo.tmp", std::process::id(), seq));
    let out_high = std::env::temp_dir().join(format!("amg_e2e_out_{}_{}_hi.tmp", std::process::id(), seq));
    let _io_cleanup = IoFileCleanup(out_low.clone(), out_high.clone());

    // Lower bound: interval_sec = 1 passes config --check and clamps up to 2s at runtime.
    dir.write("settings.toml", &format!("interval_sec = 1\n{}", disabled_builtins_toml()));
    assert_eq!(run_memguard(dir.path(), &["config", "--check"]).status.code(), Some(0));

    {
        let mut daemon = DaemonGuard::spawn_with_stdout(dir.path(), &out_low);
        let wait_t1 = Instant::now();
        while daemon_tick_lines(&out_low).is_empty() {
            daemon.check_alive();
            assert!(wait_t1.elapsed() < Duration::from_secs(12), "timed out waiting for tick 1 (interval_sec=1)");
            std::thread::sleep(Duration::from_millis(20));
        }
        let t1_done = Instant::now();
        while daemon_tick_lines(&out_low).len() < 2 {
            daemon.check_alive();
            assert!(t1_done.elapsed() < Duration::from_secs(12), "timed out waiting for tick 2 (interval_sec=1)");
            std::thread::sleep(Duration::from_millis(20));
        }
        let elapsed = t1_done.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1800),
            "expected interval_sec=1 to clamp to >=2s (measured {:?} between tick 1 and tick 2)",
            elapsed
        );
        let stop_out = daemon.stop();
        assert_eq!(stop_out.status.code(), Some(0), "stderr: {}", stderr_str(&stop_out));
    }

    // Upper bound: interval_sec = 9999 passes config --check and is not clamped down to 2s;
    // clamping to the 600s ceiling is covered in config.rs unit tests (avoiding a 600s sleep in E2E).
    dir.write("settings.toml", &format!("interval_sec = 9999\n{}", disabled_builtins_toml()));
    assert_eq!(run_memguard(dir.path(), &["config", "--check"]).status.code(), Some(0));

    {
        let mut daemon = DaemonGuard::spawn_with_stdout(dir.path(), &out_high);
        let wait_t1 = Instant::now();
        while daemon_tick_lines(&out_high).is_empty() {
            daemon.check_alive();
            assert!(wait_t1.elapsed() < Duration::from_secs(12), "timed out waiting for tick 1 (interval_sec=9999)");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(2500));
        let lines = daemon_tick_lines(&out_high);
        assert_eq!(
            lines.len(),
            1,
            "expected no 2nd tick within 2.5s when interval_sec=9999, got {} ticks:\n{}",
            lines.len(),
            lines.join("\n")
        );
        let stop_out = daemon.stop();
        assert_eq!(stop_out.status.code(), Some(0), "stderr: {}", stderr_str(&stop_out));
    }
}

#[test]
fn t2_f2_deny_unknown_fields_preserves_settings_on_invalid_edit() {
    let dir = TempDir::new("t2_f2_deny");
    dir.write("settings.toml", "# sentinel\n[general]\npreset = \"balanced\"\n");
    let before = dir.read_bytes("settings.toml");

    let bad_toml = dir.write("bad_import.toml", "intervalsec = 5\n");
    let bad_toml_str = bad_toml.to_str().expect("utf8");

    let invalid_cmds: Vec<Vec<&str>> = vec![
        vec!["config", "set", "general.unknown_e2e", "true"],
        vec!["config", "set", "general.preset", "turbo"],
        vec!["config", "set", "history.keep_days", "0"],
        vec!["config", "set", "disk.unknown_e2e", "1"],
        vec!["config", "set", "disk.scan_depth", "99"],
        vec!["config", "set", "group.Browsers.trim_above_pct", "150"],
        vec!["config", "set", "updates.manifest_url", "ftp://bad.example/x.json"],
        vec!["config", "set", "group.Browsers.name", "Renamed"],
        vec!["config", "import", bad_toml_str],
    ];

    for cmd in invalid_cmds {
        let out = run_memguard(dir.path(), &cmd);
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {:?}", cmd);
        assert!(!stderr_str(&out).trim().is_empty(), "expected stderr for {:?}", cmd);
        assert_eq!(dir.read_bytes("settings.toml"), before, "settings.toml mutated by {:?}", cmd);
        assert!(!dir.join("settings.toml.tmp").exists(), "leftover tmp after {:?}", cmd);
    }
}

#[test]
fn t2_f2_invalid_team_toml_refused_and_skipped() {
    let dir = TempDir::new("t2_f2_bad_team");
    let bad = dir.write("bad_team.toml", "[general]\npreset = 7\n");
    let bad_str = bad.to_str().expect("utf8");

    let out = run_memguard(dir.path(), &["config", "team", bad_str]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!dir.join("team.toml").exists());

    dir.write("team.toml", "[general]\npreset = 7\n");
    let chk = run_memguard(dir.path(), &["config", "--check"]);
    assert_eq!(chk.status.code(), Some(0));
    assert!(stdout_str(&chk).contains("team settings ignored"), "{}", stdout_str(&chk));

    let show = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show.contains("preset = \"balanced\""), "{}", show);
}

#[test]
fn t2_f2_config_invalid_paths_and_noop_reset() {
    let dir = TempDir::new("t2_f2_paths");
    dir.write("settings.toml", "[general]\npreset = \"balanced\"\n");
    let before = dir.read_bytes("settings.toml");

    let noop = run_memguard(dir.path(), &["config", "reset", "general.notifications"]);
    assert_eq!(noop.status.code(), Some(0));
    assert_eq!(dir.read_bytes("settings.toml"), before);

    assert_eq!(run_memguard(dir.path(), &["config", "set", "general..preset", "gentle"]).status.code(), Some(1));
    assert_eq!(run_memguard(dir.path(), &["config", "set", "group.Browsers", "trim_above_mb"]).status.code(), Some(1));
    let missing = dir.join("nonexistent.toml");
    assert_eq!(
        run_memguard(dir.path(), &["config", "import", missing.to_str().expect("utf8")]).status.code(),
        Some(1)
    );
}

#[test]
fn t2_f2_config_cli_flag_conflicts_exit_2() {
    let dir = TempDir::new("t2_f2_conflicts");
    assert_eq!(run_memguard(dir.path(), &["config", "--check", "show"]).status.code(), Some(2));
    assert_eq!(run_memguard(dir.path(), &["config", "--reset", "show"]).status.code(), Some(2));
}

/// Verifies `BUILTIN_TOML` `trim_cooldown_sec = 120` (Bug #18 effective default) and
/// `team.toml` `[updates].manifest_url` config layering/override/reset via `memguard config`.
/// Note: `updater.rs` runtime `team.toml` resolution and custom HTTPS host enforcement (Bug #13)
/// have no `memguard.exe` CLI entry point and are covered in `tests/update_flow.rs`.
#[test]
fn t2_f2_config_cooldown_default_and_team_manifest_layering() {
    let dir = TempDir::new("t2_f2_cd_team");
    let show = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show.contains("trim_cooldown_sec = 120"), "expected 120s default:\n{}", show);

    let team = dir.write("team_src.toml", "[updates]\nmanifest_url = \"https://team.internal/latest.json\"\n");
    assert_eq!(run_memguard(dir.path(), &["config", "team", team.to_str().expect("utf8")]).status.code(), Some(0));

    let show_team = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show_team.contains("manifest_url = \"https://team.internal/latest.json\""), "{}", show_team);

    assert_eq!(
        run_memguard(dir.path(), &["config", "set", "updates.manifest_url", "https://user.internal/latest.json"])
            .status
            .code(),
        Some(0)
    );
    let show_user = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show_user.contains("manifest_url = \"https://user.internal/latest.json\""), "{}", show_user);

    let reset = run_memguard(dir.path(), &["config", "reset", "updates.manifest_url"]);
    assert_eq!(reset.status.code(), Some(0));
    assert!(stdout_str(&reset).contains("team.toml"), "{}", stdout_str(&reset));
    let show_back = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show_back.contains("manifest_url = \"https://team.internal/latest.json\""), "{}", show_back);
}

#[test]
fn t2_f2_config_doctor_typed_deny_unknown_fields() {
    // M1 target: typed DoctorSettings with deny_unknown_fields and BUILTIN_TOML defaults.
    let dir = TempDir::new("t2_f2_doc_typed");
    dir.write("settings.toml", "[general]\npreset = \"balanced\"\n");
    let before = dir.read_bytes("settings.toml");

    let bad_key = run_memguard(dir.path(), &["config", "set", "doctor.unknown_e2e_field", "true"]);
    assert_eq!(bad_key.status.code(), Some(1), "unknown [doctor] key must be rejected");
    assert_eq!(dir.read_bytes("settings.toml"), before);

    let bad_type = run_memguard(dir.path(), &["config", "set", "doctor.ignore_below_mb", "bad_string"]);
    assert_eq!(bad_type.status.code(), Some(1), "invalid [doctor] field type must be rejected");
    assert_eq!(dir.read_bytes("settings.toml"), before);

    let show = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show.contains("[doctor]"), "expected [doctor] defaults in config show:\n{}", show);
    assert!(show.contains("ignore_below_mb = 150"), "expected ignore_below_mb = 150:\n{}", show);
}

// ============================================================================
// F3: Memory Engine, Foreground Grace, Pressure Split & UE 5.4+ (7 T1 + 7 T2)
// ============================================================================

#[test]
fn t1_f3_daemon_status_history_and_clean_stop() {
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t1_f3_daemon");
    let tag = unique_tag("E2EM1Ping");
    let dummy = spawn_ping_dummy(dir.path(), &tag);
    let pid = dummy.id();
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "interval_sec = 2\n{}\n[[group]]\nname = \"E2EM1Group\"\nprocesses = [\"{}\"]\npriority = \"low\"\ncpu = \"below\"\n",
        disabled_builtins_toml(),
        tag
    );
    dir.write("settings.toml", &toml);

    let mut daemon = DaemonGuard::start(dir.path());
    let start = Instant::now();
    let mut matched = false;
    while start.elapsed() < Duration::from_secs(10) {
        let st = parse_json(&run_memguard(dir.path(), &["status", "--json"]));
        assert_eq!(st["daemon"].as_bool(), Some(true));
        assert_eq!(st["paused"].as_bool(), Some(false));
        if let Some(grp) = st["groups"].as_array().and_then(|a| a.iter().find(|g| g["name"].as_str() == Some("E2EM1Group"))) {
            if grp["processes"].as_u64() == Some(1)
                && grp["ram_mb"].as_u64().unwrap_or(0) >= 1
                && grp["committed_mb"].as_u64().unwrap_or(0) >= 1
                && query_priority_class(pid) == "BelowNormal"
            {
                matched = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(matched, "daemon did not apply policy to E2EM1Group in time");

    let stop_out = daemon.stop();
    assert_eq!(stop_out.status.code(), Some(0), "stderr: {}", stderr_str(&stop_out));

    let after = parse_json(&run_memguard(dir.path(), &["status", "--json"]));
    assert_eq!(after["daemon"].as_bool(), Some(false));
    assert_eq!(query_priority_class(pid), "Normal", "priority class was not restored on stop");

    let date = &after["time"].as_str().expect("time")[..10];
    let csv_path = dir.join(&format!("history\\{}.csv", date));
    assert!(csv_path.is_file(), "expected history CSV at {}", csv_path.display());
    let csv_text = fs::read_to_string(&csv_path).expect("read history csv");
    assert!(csv_text.lines().count() >= 2, "expected header + >=1 row:\n{}", csv_text);

    let hist = parse_json(&run_memguard(dir.path(), &["history", "--hours", "1", "--json"]));
    assert!(!hist.as_array().expect("history array").is_empty());
}

#[test]
fn t1_f3_prime_trims_target_group() {
    let dir = TempDir::new("t1_f3_prime");
    let tag = unique_tag("E2EM1Ping");
    let _dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "{}\n[[group]]\nname = \"E2EM1PrimeTarget\"\nprocesses = [\"{}\"]\npriority = \"low\"\n",
        disabled_builtins_toml(),
        tag
    );
    dir.write("settings.toml", &toml);

    let out = run_memguard(dir.path(), &["prime", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_str(&out));
    let v = parse_json(&out);
    assert_eq!(v["trimmed_processes"].as_u64(), Some(1));
    assert!(v["before"]["ram_total_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["after"]["ram_total_gb"].as_f64().unwrap_or(0.0) > 0.0);

    let log = dir.read_to_string("memguard.log");
    assert!(log.contains("prime: trimmed 1 processes"), "{}", log);
}

#[test]
fn t1_f3_run_job_cap_and_cpu_priority() {
    let dir = TempDir::new("t1_f3_run_caps");

    let below = run_memguard(
        dir.path(),
        &[
            "run",
            "--cpu",
            "below",
            "--",
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.Diagnostics.Process]::GetCurrentProcess().PriorityClass",
        ],
    );
    assert_eq!(below.status.code(), Some(0));
    assert_eq!(stdout_str(&below).trim(), "BelowNormal");

    let idle = run_memguard(
        dir.path(),
        &[
            "run",
            "--cpu",
            "idle",
            "--",
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.Diagnostics.Process]::GetCurrentProcess().PriorityClass",
        ],
    );
    assert_eq!(idle.status.code(), Some(0));
    assert_eq!(stdout_str(&idle).trim(), "Idle");

    let low = run_memguard(dir.path(), &["run", "--priority", "low", "--", "cmd.exe", "/c", "exit", "0"]);
    assert_eq!(low.status.code(), Some(0));

    let capped = run_memguard(
        dir.path(),
        &[
            "run",
            "--cap-gb",
            "0.12",
            "--",
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "try { $null = New-Object byte[] (256MB); exit 0 } catch [System.OutOfMemoryException] { exit 42 }",
        ],
    );
    assert_ne!(capped.status.code(), Some(0), "expected OOM failure under 0.12 GB job cap");
}

#[test]
fn t1_f3_run_cpu_eco_idle_priority() {
    // M1 target (F2.4.2): Cpu::Eco sets IDLE_PRIORITY_CLASS + EcoQoS.
    let dir = TempDir::new("t1_f3_cpu_eco");
    let out = run_memguard(
        dir.path(),
        &[
            "run",
            "--cpu",
            "eco",
            "--",
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.Diagnostics.Process]::GetCurrentProcess().PriorityClass",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_str(&out).trim(), "Idle", "M1 Cpu::Eco must set Idle priority class");
}

#[test]
fn t1_f3_overlapping_groups_first_wins() {
    // M1 target (Bug #4): overlapping second group must not re-trim processes claimed by first group.
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t1_f3_overlap");
    let tag = unique_tag("E2EM1Overlap");
    let dummy = spawn_ping_dummy(dir.path(), &tag);
    let pid = dummy.id();
    std::thread::sleep(Duration::from_millis(300));

    let toml = format!(
        "interval_sec = 2\n{}\n\
         [[group]]\nname = \"E2EM1First\"\nprocesses = [\"{}\"]\ncpu = \"below\"\n\
         [[group]]\nname = \"E2EM1Second\"\nprocesses = [\"{}\"]\ncpu = \"idle\"\ntrim_above_mb = 1\n",
        disabled_builtins_toml(),
        tag,
        tag
    );
    dir.write("settings.toml", &toml);

    let mut daemon = DaemonGuard::start(dir.path());
    let start = Instant::now();
    let mut applied = false;
    while start.elapsed() < Duration::from_secs(10) {
        if query_priority_class(pid) == "BelowNormal" {
            applied = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(applied, "first group did not apply BelowNormal priority");

    // Allow another tick to confirm second group neither overrides priority nor double-trims.
    std::thread::sleep(Duration::from_millis(2400));
    assert_eq!(query_priority_class(pid), "BelowNormal", "second group must not override first group");

    let st = parse_json(&run_memguard(dir.path(), &["status", "--json"]));
    let ram_groups = st["ram"]["groups"].as_array().expect("ram.groups");
    if let Some(second_ram) = ram_groups.iter().find(|g| g["name"].as_str() == Some("E2EM1Second")) {
        assert_eq!(second_ram["ram_gb"].as_f64(), Some(0.0));
    }

    let _ = daemon.stop();
    let log = dir.read_to_string("memguard.log");
    let double_trimmed = log
        .lines()
        .any(|line| line.contains("[E2EM1Second]") && line.contains("trimmed 1"));
    assert!(!double_trimmed, "Bug #4: second group double-trimmed claimed process:\n{}", log);
}

#[test]
fn t1_f3_ue54_workload_detection() {
    // M1 target (F2.5): UbaHost, LiveCodingConsole, UnrealHeaderTool protected & build-active;
    // UnrealEditor-Cmd -run=DerivedDataCache activates UE mode as commandlet.
    // Safety: outside any .uproject directory, <500ms dummy process lifetime.
    let dir = TempDir::new("t1_f3_ue54");
    let ue_dir = dir.join("ue_dummy");
    fs::create_dir_all(&ue_dir).expect("create ue_dummy dir");

    let toml = format!(
        "{}\n[[group]]\nname = \"E2EM1UeGroup\"\nprocesses = [\"UbaHost\", \"LiveCodingConsole\", \"UnrealHeaderTool\"]\n",
        disabled_builtins_toml()
    );
    dir.write("settings.toml", &toml);

    // Pre-copy binaries before spawning so live process lifetime stays <500ms.
    let uba_exe = copy_sys_exe(&ue_dir, r"System32\PING.EXE", "UbaHost");
    let lc_exe = copy_sys_exe(&ue_dir, r"System32\PING.EXE", "LiveCodingConsole");
    let uht_exe = copy_sys_exe(&ue_dir, r"System32\PING.EXE", "UnrealHeaderTool");
    let cmdlet_exe = copy_sys_exe(&ue_dir, r"System32\cmd.exe", "UnrealEditor-Cmd");

    let st_phase_a = {
        let _g1 = KillGuard::new(
            Command::new(&uba_exe)
                .args(["-t", "127.0.0.1"])
                .current_dir(&ue_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("spawn UbaHost"),
        );
        let _g2 = KillGuard::new(
            Command::new(&lc_exe)
                .args(["-t", "127.0.0.1"])
                .current_dir(&ue_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("spawn LiveCodingConsole"),
        );
        let _g3 = KillGuard::new(
            Command::new(&uht_exe)
                .args(["-t", "127.0.0.1"])
                .current_dir(&ue_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("spawn UnrealHeaderTool"),
        );
        parse_json(&run_memguard(dir.path(), &["status", "--json"]))
    };

    let ue_grp = st_phase_a["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"].as_str() == Some("E2EM1UeGroup"))
        .expect("E2EM1UeGroup");
    assert_eq!(ue_grp["processes"].as_u64(), Some(0), "UE 5.4+ tools must be protected");
    assert_eq!(st_phase_a["activities"]["build"].as_bool(), Some(true), "UE 5.4+ build tools must set activities.build");
    assert!(st_phase_a["ue"]["workers"].as_u64().unwrap_or(0) >= 3);

    let st_phase_b = {
        let _g4 = KillGuard::new(
            Command::new(&cmdlet_exe)
                .args(["/k", "rem -run=DerivedDataCache"])
                .current_dir(&ue_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("spawn UnrealEditor-Cmd"),
        );
        parse_json(&run_memguard(dir.path(), &["status", "--json"]))
    };

    let ue_procs = st_phase_b["ue"]["processes"].as_array().expect("ue.processes");
    assert!(
        ue_procs
            .iter()
            .any(|p| p["kind"].as_str() == Some("commandlet")
                && p["name"].as_str().unwrap_or("").eq_ignore_ascii_case("UnrealEditor-Cmd")),
        "expected UnrealEditor-Cmd commandlet in {:?}",
        ue_procs
    );
    assert_ne!(
        st_phase_b["mode"].as_str(),
        Some("idle"),
        "DerivedDataCache commandlet must activate UE mode"
    );
}

#[test]
fn t1_f3_bug2_job_caps_cleared_on_stop_and_reload() {
    // M1 target (Bug #2): Job Object hard caps must be cleared on reload and on stop.
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t1_f3_bug2");
    let tag1 = unique_tag("E2EM1CapA");
    let (_guard1, mut stdin1, mut reader1) = spawn_cap_loop_dummy(dir.path(), &tag1);

    let base_capped = format!(
        "interval_sec = 2\n{}\n[[group]]\nname = \"E2EM1CapGroup\"\nprocesses = [\"{}\"]\nhard_cap_per_process_mb = 128\n",
        disabled_builtins_toml(),
        tag1
    );
    dir.write("settings.toml", &base_capped);

    let mut daemon = DaemonGuard::start(dir.path());
    let start = Instant::now();
    let mut hit_oom = false;
    while start.elapsed() < Duration::from_secs(12) {
        if probe_alloc(&mut stdin1, &mut reader1) == "OOM" {
            hit_oom = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(hit_oom, "daemon did not enforce 128 MB job cap on target");

    // Phase A: config reload removes the hard cap.
    // Wait on the daemon's own `memguard.log` ("settings loaded:") rather than `status --json`,
    // because `status --json` reads `settings.toml` directly before the daemon's next tick.
    let reloads_before = fs::read_to_string(dir.join("memguard.log"))
        .unwrap_or_default()
        .matches("settings loaded:")
        .count();
    let uncapped = format!(
        "interval_sec = 2\n{}\n[[group]]\nname = \"E2EM1CapGroup\"\nprocesses = [\"{}\"]\nhard_cap_per_process_mb = 0\n",
        disabled_builtins_toml(),
        tag1
    );
    dir.write("settings.toml", &uncapped);
    let reload_start = Instant::now();
    let mut reloaded = false;
    while reload_start.elapsed() < Duration::from_secs(12) {
        let reloads_now = fs::read_to_string(dir.join("memguard.log"))
            .unwrap_or_default()
            .matches("settings loaded:")
            .count();
        if reloads_now > reloads_before {
            reloaded = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(reloaded, "daemon did not reload uncapped config");
    assert_eq!(probe_alloc(&mut stdin1, &mut reader1), "OK", "Bug #2: hard cap remained after config reload");

    // Phase B: re-enable cap on a fresh target and verify memguard stop clears it.
    let tag2 = unique_tag("E2EM1CapB");
    let (_guard2, mut stdin2, mut reader2) = spawn_cap_loop_dummy(dir.path(), &tag2);
    let recapped = format!(
        "interval_sec = 2\n{}\n[[group]]\nname = \"E2EM1CapStop\"\nprocesses = [\"{}\"]\nhard_cap_per_process_mb = 128\n",
        disabled_builtins_toml(),
        tag2
    );
    dir.write("settings.toml", &recapped);
    let cap2_start = Instant::now();
    let mut hit_oom2 = false;
    while cap2_start.elapsed() < Duration::from_secs(12) {
        if probe_alloc(&mut stdin2, &mut reader2) == "OOM" {
            hit_oom2 = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(hit_oom2, "daemon did not enforce 128 MB job cap on second target");

    let stop_out = daemon.stop();
    assert_eq!(stop_out.status.code(), Some(0));
    assert_eq!(probe_alloc(&mut stdin2, &mut reader2), "OK", "Bug #2: hard cap remained after memguard stop");
}

#[test]
fn t2_f3_daemon_second_instance_rejected() {
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t2_f3_dup_daemon");
    dir.write("settings.toml", &format!("interval_sec = 2\n{}", disabled_builtins_toml()));

    let mut daemon = DaemonGuard::start(dir.path());
    let second = run_memguard(dir.path(), &["daemon"]);
    assert_eq!(second.status.code(), Some(0));
    assert!(stderr_str(&second).contains("already running"), "stderr: {}", stderr_str(&second));

    let st = parse_json(&run_memguard(dir.path(), &["status", "--json"]));
    assert_eq!(st["daemon"].as_bool(), Some(true));
    assert_eq!(daemon.stop().status.code(), Some(0));
}

#[test]
fn t2_f3_prime_disabled_group_and_empty_targets() {
    let dir = TempDir::new("t2_f3_prime_dis");
    let tag = unique_tag("E2EM1DisPing");
    let _dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(200));

    let toml = format!(
        "{}\n\
         [[group]]\nname = \"E2EM1Disabled\"\nenabled = false\nprocesses = [\"{}\"]\n\
         [[group]]\nname = \"E2EM1Empty\"\nprocesses = [\"__no_such_e2em1_proc__\"]\n",
        disabled_builtins_toml(),
        tag
    );
    dir.write("settings.toml", &toml);

    let prime_out = run_memguard(dir.path(), &["prime", "--json"]);
    assert_eq!(prime_out.status.code(), Some(0));
    assert_eq!(parse_json(&prime_out)["trimmed_processes"].as_u64(), Some(0));

    let st = parse_json(&run_memguard(dir.path(), &["status", "--json"]));
    let groups = st["groups"].as_array().expect("groups");
    assert!(groups.iter().all(|g| g["name"].as_str() != Some("E2EM1Disabled")));
    let empty = groups.iter().find(|g| g["name"].as_str() == Some("E2EM1Empty")).expect("E2EM1Empty");
    assert_eq!(empty["processes"].as_u64(), Some(0));
}

#[test]
fn t2_f3_run_tiny_cap_and_bad_command_boundary() {
    let dir = TempDir::new("t2_f3_run_tiny");

    let missing = run_memguard(dir.path(), &["run", "--", "__e2e_nonexistent_cmd_xyz__.exe"]);
    assert_eq!(missing.status.code(), Some(125));
    assert!(stderr_str(&missing).contains("memguard run:"));

    let tiny = run_memguard(dir.path(), &["run", "--per-process-cap-gb", "0.0001", "--", "cmd.exe", "/c", "echo", "ok"]);
    assert_ne!(tiny.status.code(), Some(0), "tiny 100KB commit cap must prevent cmd.exe initialization");
}

#[test]
fn t2_f3_pressure_bounds_and_unknown_key_rejected() {
    let dir = TempDir::new("t2_f3_pressure_base");
    dir.write("settings.toml", "[general]\npreset = \"balanced\"\n");
    let before = dir.read_bytes("settings.toml");

    for args in [
        ["config", "set", "--", "pressure.elevated_ram_free_pct", "101.0"],
        ["config", "set", "--", "pressure.elevated_ram_free_pct", "-1.0"],
        ["config", "set", "--", "pressure.critical_commit_free_pct", "150.0"],
        ["config", "set", "--", "pressure.unknown_e2e_key", "10"],
    ] {
        let out = run_memguard(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {:?}", args);
        assert_eq!(dir.read_bytes("settings.toml"), before);
    }
}

#[test]
fn t2_f3_foreground_grace_and_pressure_hysteresis_m1() {
    // M1 target: foreground_grace_sec and [pressure] hysteresis_pct / hysteresis_gb.
    let dir = TempDir::new("t2_f3_fg_hyst_m1");
    let show = stdout_str(&run_memguard(dir.path(), &["config", "show"]));
    assert!(show.contains("foreground_grace_sec"), "missing foreground_grace_sec:\n{}", show);
    assert!(show.contains("hysteresis_pct"), "missing hysteresis_pct:\n{}", show);
    assert!(show.contains("hysteresis_gb"), "missing hysteresis_gb:\n{}", show);

    assert_eq!(run_memguard(dir.path(), &["config", "set", "foreground_grace_sec", "30"]).status.code(), Some(0));
    assert_eq!(run_memguard(dir.path(), &["config", "set", "pressure.hysteresis_pct", "2.5"]).status.code(), Some(0));
    assert_eq!(run_memguard(dir.path(), &["config", "set", "pressure.hysteresis_gb", "1.5"]).status.code(), Some(0));

    let before = dir.read_bytes("settings.toml");
    for bad in [
        ["config", "set", "--", "foreground_grace_sec", "9999"],
        ["config", "set", "--", "pressure.hysteresis_pct", "-1.0"],
        ["config", "set", "--", "pressure.hysteresis_pct", "101.0"],
    ] {
        let out = run_memguard(dir.path(), &bad);
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {:?}", bad);
        assert_eq!(dir.read_bytes("settings.toml"), before);
    }
}

#[test]
fn t2_f3_bug14_concurrent_log_writes_and_rotation() {
    let dir = TempDir::new("t2_f3_bug14_log");
    dir.write("settings.toml", disabled_builtins_toml());

    // Pre-seed memguard.log > 2 MB to force rotation during concurrent prime invocations.
    let seed_line = "2026-01-01 00:00:00  seed\n";
    let mut seed = String::with_capacity(2_200_000);
    while seed.len() < 2_200_000 {
        seed.push_str(seed_line);
    }
    dir.write("memguard.log", &seed);

    let mut handles = Vec::new();
    for _ in 0..6 {
        let p = dir.path().to_path_buf();
        handles.push(std::thread::spawn(move || run_memguard(&p, &["prime", "--json"])));
    }
    for h in handles {
        let out = h.join().expect("join thread");
        assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_str(&out));
    }

    let is_log_line = |line: &str| {
        let b = line.as_bytes();
        b.len() >= 21
            && b[0..4].iter().all(|c| c.is_ascii_digit())
            && b[4] == b'-'
            && b[5..7].iter().all(|c| c.is_ascii_digit())
            && b[7] == b'-'
            && b[8..10].iter().all(|c| c.is_ascii_digit())
            && b[10] == b' '
            && b[11..13].iter().all(|c| c.is_ascii_digit())
            && b[13] == b':'
            && b[14..16].iter().all(|c| c.is_ascii_digit())
            && b[16] == b':'
            && b[17..19].iter().all(|c| c.is_ascii_digit())
            && &b[19..21] == b"  "
    };

    for rel in ["memguard.log", "memguard.log.old"] {
        let p = dir.join(rel);
        if p.is_file() {
            let text = fs::read_to_string(&p).expect("valid UTF-8 log");
            for line in text.lines() {
                if !line.trim().is_empty() {
                    assert!(is_log_line(line), "corrupted/interleaved log line in {}: {:?}", rel, line);
                }
            }
        }
    }
}

#[test]
fn t2_f3_group_name_dot_and_duplicate_rejected() {
    let dir = TempDir::new("t2_f3_grp_bad");

    dir.write("settings.toml", "[[group]]\nname = \"Bad.Name\"\nprocesses = [\"x\"]\n");
    let dot_out = run_memguard(dir.path(), &["config", "--check"]);
    assert_eq!(dot_out.status.code(), Some(1));
    assert!(stderr_str(&dot_out).contains("dot"), "{}", stderr_str(&dot_out));

    dir.write(
        "settings.toml",
        "[[group]]\nname = \"DupGroup\"\nprocesses = [\"x\"]\n[[group]]\nname = \"dupgroup\"\nprocesses = [\"y\"]\n",
    );
    let dup_out = run_memguard(dir.path(), &["config", "--check"]);
    assert_eq!(dup_out.status.code(), Some(1));
    assert!(stderr_str(&dup_out).contains("defined twice"), "{}", stderr_str(&dup_out));
}

// ============================================================================
// F4: Doctor & Process Safety (6 Tier 1 + 7 Tier 2)
// ============================================================================

#[test]
fn t1_f4_doctor_json_schema() {
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t1_f4_schema");

    let out = run_memguard(dir.path(), &["doctor", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_str(&out));
    let v = parse_json(&out);

    assert!(!v["time"].as_str().unwrap_or("").is_empty());
    assert!(v["unreal_active"].is_boolean());
    assert!(v["reclaimable_ram_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(v["reclaimable_commit_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(v["system"]["ram_total_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["system"]["commit_limit_gb"].as_f64().unwrap_or(0.0) > 0.0);

    let findings = v["findings"].as_array().expect("findings");
    let sys_mg = findings
        .iter()
        .find(|f| f["id"].as_str() == Some("sys:memguard"))
        .expect("sys:memguard finding");
    assert_eq!(sys_mg["kind"].as_str(), Some("system"));
    assert_eq!(sys_mg["verdict"].as_str(), Some("review"));
    assert_eq!(sys_mg["actions"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(sys_mg["recommended"].as_bool(), Some(false));

    for f in findings {
        assert!(!f["id"].as_str().unwrap_or("").is_empty());
        assert!(!f["title"].as_str().unwrap_or("").is_empty());
        assert!(!f["detail"].as_str().unwrap_or("").is_empty());
        assert_ne!(f["verdict"].as_str(), Some("keep"));
    }
}

#[test]
fn t1_f4_doctor_dry_run_makes_no_changes() {
    let dir = TempDir::new("t1_f4_dry_run");
    let tag = unique_tag("E2EM1DocPing");
    let mut dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let id = format!("app:{}", tag.to_ascii_lowercase());
    let pick = format!("{}:end", id);
    let out = run_memguard(dir.path(), &["doctor", "--fix", &pick, "--force", "--dry-run", "--json"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr_str(&out));
    let v = parse_json(&out);
    assert_eq!(v["dry_run"].as_bool(), Some(true));
    let outcomes = v["outcomes"].as_array().expect("outcomes");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["finding_id"].as_str(), Some(id.as_str()));
    assert_eq!(outcomes[0]["action"].as_str(), Some("end"));
    assert_eq!(outcomes[0]["result"].as_str(), Some("would_do"));

    assert!(dummy.try_wait().is_none(), "dry-run must not terminate process");
    assert!(!dir.join("doctor-undo.json").exists(), "dry-run must not write doctor-undo.json");
}

#[test]
fn t1_f4_doctor_close_windowed_and_undo_relaunch() {
    let dir = TempDir::new("t1_f4_win_undo");
    let tag = unique_tag("E2EM1DocWin");
    let exe_dir = dir.join("dir with space");
    fs::create_dir_all(&exe_dir).expect("create dir with space");
    let _name_cleanup = ProcessNameCleanupGuard::new(format!("{}.exe", tag));
    let mut dummy = spawn_windowed_gui_dummy(&exe_dir, &tag, &tag);

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let id = format!("app:{}", tag.to_ascii_lowercase());
    let start = Instant::now();
    let mut has_win = false;
    while start.elapsed() < Duration::from_secs(30) {
        let rep = parse_json(&run_memguard(dir.path(), &["doctor", "--json"]));
        if let Some(f) = rep["findings"].as_array().and_then(|a| a.iter().find(|f| f["id"].as_str() == Some(id.as_str()))) {
            if f["has_window"].as_bool() == Some(true) && f["verdict"].as_str() == Some("close") {
                has_win = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(has_win, "windowed gui dummy never surfaced with has_window == true");

    let close_pick = format!("{}:close", id);
    let close_out = run_memguard(dir.path(), &["doctor", "--fix", &close_pick, "--json"]);
    assert_eq!(close_out.status.code(), Some(0), "stderr: {}", stderr_str(&close_out));
    let cv = parse_json(&close_out);
    assert_eq!(cv["dry_run"].as_bool(), Some(false));
    assert_eq!(cv["outcomes"][0]["result"].as_str(), Some("done"));
    assert_eq!(cv["outcomes"][0]["message"].as_str(), Some("Closed"));
    let undo_id = cv["outcomes"][0]["undo_id"].as_str().expect("undo_id").to_string();
    assert!(!undo_id.is_empty());
    assert!(dummy.wait_exit(Duration::from_secs(5)), "windowed process did not close");
    assert!(dir.join("doctor-undo.json").is_file());

    let undo_out = run_memguard(dir.path(), &["doctor", "--undo", &undo_id, "--json"]);
    assert_eq!(undo_out.status.code(), Some(0), "stderr: {}", stderr_str(&undo_out));
    let uv = parse_json(&undo_out);
    assert_eq!(uv["outcomes"][0]["result"].as_str(), Some("undone"));
    assert_eq!(uv["outcomes"][0]["message"].as_str(), Some("Started again"));

    let relaunch_start = Instant::now();
    let mut relaunched = false;
    let mut last_rep = Value::Null;
    while relaunch_start.elapsed() < Duration::from_secs(20) {
        let rep = parse_json(&run_memguard(dir.path(), &["doctor", "--json"]));
        if let Some(f) = rep["findings"].as_array().and_then(|a| a.iter().find(|f| f["id"].as_str() == Some(id.as_str()))) {
            let new_pid = f["pids"].as_array().and_then(|p| p.first()).and_then(|p| p.as_u64());
            if new_pid.is_some() && new_pid != Some(dummy.id() as u64) {
                relaunched = true;
                break;
            }
        }
        last_rep = rep;
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        relaunched,
        "relaunched process did not appear in doctor --json; findings:\n{}\nundo log:\n{}",
        last_rep["findings"],
        dir.read_to_string("doctor-undo.json")
    );

    let end_pick = format!("{}:end", id);
    let end_out = run_memguard(dir.path(), &["doctor", "--fix", &end_pick, "--force", "--json"]);
    assert_eq!(end_out.status.code(), Some(0));
    assert_eq!(parse_json(&end_out)["outcomes"][0]["result"].as_str(), Some("done"));
}

#[test]
fn t1_f4_doctor_end_with_force_terminates_windowless() {
    let dir = TempDir::new("t1_f4_end_force");
    let tag = unique_tag("E2EM1DocPing");
    let mut dummy = spawn_ping_dummy(dir.path(), &tag);
    let pid = dummy.id();
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let id = format!("app:{}", tag.to_ascii_lowercase());
    let rep = parse_json(&run_memguard(dir.path(), &["doctor", "--json"]));
    let f = rep["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|f| f["id"].as_str() == Some(id.as_str()))
        .expect("finding present");
    assert_eq!(f["verdict"].as_str(), Some("close"));
    assert_eq!(f["has_window"].as_bool(), Some(false));
    assert_eq!(f["pids"], serde_json::json!([pid]));
    let actions = f["actions"].as_array().expect("actions");
    assert!(actions.iter().any(|a| a["kind"].as_str() == Some("close") && a["needs_force"].as_bool() == Some(false)));
    assert!(actions.iter().any(|a| a["kind"].as_str() == Some("end") && a["needs_force"].as_bool() == Some(true)));

    let pick = format!("{}:end", id);
    let out = run_memguard(dir.path(), &["doctor", "--fix", &pick, "--force", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let ov = parse_json(&out);
    assert_eq!(ov["outcomes"][0]["result"].as_str(), Some("done"));
    assert_eq!(ov["outcomes"][0]["message"].as_str(), Some("Ended"));
    assert!(ov["outcomes"][0]["undo_id"].is_null());
    assert!(dummy.wait_exit(Duration::from_secs(5)));
}

#[test]
fn t1_f4_doctor_review_version_info_bug17() {
    let dir = TempDir::new("t1_f4_verinfo");
    let tag = unique_tag("E2EM1DocRev");
    let _dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nreview_above_mb = 0\n"
    );
    dir.write("settings.toml", &toml);

    let id = format!("app:{}", tag.to_ascii_lowercase());
    let out = run_memguard(dir.path(), &["doctor", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let rep = parse_json(&out);
    let f = rep["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .find(|f| f["id"].as_str() == Some(id.as_str()))
        .expect("review finding present");

    assert_eq!(f["verdict"].as_str(), Some("review"));
    assert_eq!(f["recommended"].as_bool(), Some(false));
    assert_eq!(f["company"].as_str(), Some("Microsoft Corporation"));
    assert!(f["detail"].as_str().unwrap_or("").contains("Microsoft Corporation"));
    let actions = f["actions"].as_array().expect("actions");
    assert!(actions.iter().any(|a| a["kind"].as_str() == Some("close")));
    assert!(actions.iter().any(|a| a["kind"].as_str() == Some("hold_back")));
    assert!(actions.iter().any(|a| a["kind"].as_str() == Some("leave_it")));
}

#[test]
fn t1_f4_drivers_json_schema() {
    let dir = TempDir::new("t1_f4_drivers");
    let out = run_memguard(dir.path(), &["drivers", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = parse_json(&out);

    assert!(v["nonpaged_pool_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["total_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(v["windows_processes_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(v["paged_pool_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(v["system_code_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(v["drivers_code_gb"].as_f64().unwrap_or(-1.0) >= 0.0);
    assert!(!v["processes"].as_array().expect("processes").is_empty());
    let verdict = v["verdict"].as_array().expect("verdict");
    assert!(!verdict.is_empty());

    let tags_avail = v["tags_available"].as_bool().expect("tags_available");
    let tags = v["tags"].as_array().expect("tags");
    if !tags_avail {
        assert!(tags.is_empty());
    } else {
        for t in tags {
            assert_eq!(t["tag"].as_str().map(|s| s.len()), Some(4));
            for d in t["drivers"].as_array().expect("tag drivers") {
                let file = d["file"].as_str().expect("driver file");
                assert!(file.to_ascii_lowercase().ends_with(".sys"), "unexpected driver file: {}", file);
            }
        }
    }
}

#[test]
fn t2_f4_doctor_dry_run_or_force_without_fix_rejected() {
    let dir = TempDir::new("t2_f4_flags");
    for args in [vec!["doctor", "--dry-run"], vec!["doctor", "--force"], vec!["doctor", "--dry-run", "--json"]] {
        let out = run_memguard(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "args={:?}", args);
        assert!(stderr_str(&out).contains("--dry-run and --force go with --fix."), "{}", stderr_str(&out));
    }
}

#[test]
fn t2_f4_doctor_end_without_force_refused() {
    let dir = TempDir::new("t2_f4_no_force");
    let tag = unique_tag("E2EM1DocPing");
    let mut dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let pick = format!("app:{}:end", tag.to_ascii_lowercase());
    let out = run_memguard(dir.path(), &["doctor", "--fix", &pick, "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v = parse_json(&out);
    assert_eq!(v["outcomes"][0]["result"].as_str(), Some("refused"));
    assert!(v["outcomes"][0]["message"].as_str().unwrap_or("").contains("--force"));
    assert!(dummy.try_wait().is_none(), "refused end must not kill process");
}

#[test]
fn t2_f4_doctor_close_windowless_reports_still_open() {
    let dir = TempDir::new("t2_f4_still_open");
    let tag = unique_tag("E2EM1DocPing");
    let mut dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let pick = format!("app:{}:close", tag.to_ascii_lowercase());
    let out = run_memguard(dir.path(), &["doctor", "--fix", &pick, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = parse_json(&out);
    assert_eq!(v["outcomes"][0]["result"].as_str(), Some("still_open"));
    assert!(v["outcomes"][0]["message"].as_str().unwrap_or("").contains("no window"));
    assert!(v["outcomes"][0]["undo_id"].is_null());
    assert!(dummy.try_wait().is_none());
}

#[test]
fn t2_f4_doctor_unknown_and_info_only_picks_fail() {
    let _daemon_lock = lock_daemon();
    let dir = TempDir::new("t2_f4_bad_picks");

    let unk = run_memguard(dir.path(), &["doctor", "--fix", "app:nonexistent_e2em1_xyz"]);
    assert_eq!(unk.status.code(), Some(1));
    assert!(stderr_str(&unk).contains("No finding"), "{}", stderr_str(&unk));

    let info = run_memguard(dir.path(), &["doctor", "--fix", "sys:memguard"]);
    assert_eq!(info.status.code(), Some(1));
    assert!(stderr_str(&info).contains("nothing to do"), "{}", stderr_str(&info));
}

#[test]
fn t2_f4_doctor_self_tree_and_memguard_name_protection() {
    let dir = TempDir::new("t2_f4_self_tree");
    let parent_tag = unique_tag("E2EM1DocParent");
    let mg_tag = unique_tag("E2EM1DocMemguard");
    let parent_exe = copy_sys_exe(dir.path(), r"System32\cmd.exe", &parent_tag);
    let mut mg_dummy = spawn_ping_dummy(dir.path(), &mg_tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\", \"{}\"]\n",
        parent_tag, mg_tag
    );
    dir.write("settings.toml", &toml);

    let mg_path = memguard_exe().display().to_string();
    let scan_raw = format!("/C \"\"{}\" doctor --json\"", mg_path);
    let mut scan_cmd = Command::new(&parent_exe);
    scan_cmd
        .raw_arg(&scan_raw)
        .env("ATHER_MEMGUARD_DIR", dir.path())
        .creation_flags(CREATE_NO_WINDOW);
    let scan_out = run_command_bounded(scan_cmd, Duration::from_secs(30), "parent wrapper doctor --json");
    assert_eq!(scan_out.status.code(), Some(0), "stderr: {}", stderr_str(&scan_out));
    let rep = parse_json(&scan_out);
    let findings = rep["findings"].as_array().expect("findings");

    let mg_id = format!("app:{}", mg_tag.to_ascii_lowercase());
    assert!(
        findings.iter().all(|f| f["id"].as_str() != Some(mg_id.as_str())),
        "process with memguard in name must be protected and absent from findings"
    );

    let parent_id = format!("app:{}", parent_tag.to_ascii_lowercase());
    let pf = findings
        .iter()
        .find(|f| f["id"].as_str() == Some(parent_id.as_str()))
        .expect("parent wrapper finding present");
    assert_eq!(pf["actions"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(pf["recommended"].as_bool(), Some(false));
    assert!(pf["detail"].as_str().unwrap_or("").contains("started from this app"));

    let fix_raw = format!("/C \"\"{}\" doctor --fix {}\"", mg_path, parent_id);
    let mut fix_cmd = Command::new(&parent_exe);
    fix_cmd
        .raw_arg(&fix_raw)
        .env("ATHER_MEMGUARD_DIR", dir.path())
        .creation_flags(CREATE_NO_WINDOW);
    let fix_out = run_command_bounded(fix_cmd, Duration::from_secs(30), "parent wrapper doctor --fix");
    assert_eq!(fix_out.status.code(), Some(1));
    assert!(mg_dummy.try_wait().is_none());
}

#[test]
fn t2_f4_doctor_undo_nonexistent_and_double_undo_refused() {
    let dir = TempDir::new("t2_f4_undo_bad");

    let empty_undo = run_memguard(dir.path(), &["doctor", "--undo"]);
    assert_eq!(empty_undo.status.code(), Some(1));

    let missing_id = run_memguard(dir.path(), &["doctor", "--undo", "nope-999", "--json"]);
    assert_eq!(missing_id.status.code(), Some(1));
    let mv = parse_json(&missing_id);
    assert_eq!(mv["outcomes"][0]["result"].as_str(), Some("refused"));
    assert!(mv["outcomes"][0]["message"].as_str().unwrap_or("").contains("Nothing to undo"));

    // Real close -> undo -> double-undo refused.
    let tag = unique_tag("E2EM1DocDblUndo");
    let _name_cleanup = ProcessNameCleanupGuard::new(format!("{}.exe", tag));
    let mut dummy = spawn_windowed_gui_dummy(dir.path(), &tag, &tag);

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nclose = [\"{}\"]\n",
        tag
    );
    dir.write("settings.toml", &toml);

    let id = format!("app:{}", tag.to_ascii_lowercase());
    let start = Instant::now();
    loop {
        let rep = parse_json(&run_memguard(dir.path(), &["doctor", "--json"]));
        if rep["findings"]
            .as_array()
            .is_some_and(|a| a.iter().any(|f| f["id"].as_str() == Some(id.as_str()) && f["has_window"].as_bool() == Some(true)))
        {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "windowed dummy never appeared");
        std::thread::sleep(Duration::from_millis(250));
    }

    let close_out = run_memguard(dir.path(), &["doctor", "--fix", &format!("{}:close", id), "--json"]);
    assert_eq!(close_out.status.code(), Some(0));
    let undo_id = parse_json(&close_out)["outcomes"][0]["undo_id"].as_str().expect("undo_id").to_string();
    assert!(dummy.wait_exit(Duration::from_secs(5)));

    let first_undo = run_memguard(dir.path(), &["doctor", "--undo", &undo_id, "--json"]);
    assert_eq!(first_undo.status.code(), Some(0));
    assert_eq!(parse_json(&first_undo)["outcomes"][0]["result"].as_str(), Some("undone"));

    let second_undo = run_memguard(dir.path(), &["doctor", "--undo", &undo_id, "--json"]);
    assert_eq!(second_undo.status.code(), Some(1));
    let sv = parse_json(&second_undo);
    assert_eq!(sv["outcomes"][0]["result"].as_str(), Some("refused"));
    assert_eq!(sv["outcomes"][0]["message"].as_str(), Some("Already undone"));

    let _ = run_memguard(dir.path(), &["doctor", "--fix", &format!("{}:end", id), "--force", "--json"]);
}

#[test]
fn t2_f4_doctor_keep_overrides_close() {
    let dir = TempDir::new("t2_f4_keep_wins");
    let tag = unique_tag("E2EM1DocKeep");
    let _dummy = spawn_ping_dummy(dir.path(), &tag);
    std::thread::sleep(Duration::from_millis(250));

    let toml = format!(
        "[general]\ncheck_ram_speed = false\n[disk]\nenabled = false\ncheck_dev_drive = false\n\
         [doctor]\nignore_below_mb = 0\nkeep = [\"{}\"]\nclose = [\"{}\"]\n",
        tag, tag
    );
    dir.write("settings.toml", &toml);

    let id = format!("app:{}", tag.to_ascii_lowercase());
    let rep = parse_json(&run_memguard(dir.path(), &["doctor", "--json"]));
    assert!(
        rep["findings"].as_array().expect("findings").iter().all(|f| f["id"].as_str() != Some(id.as_str())),
        "Verdict::Keep must suppress finding even when also in close"
    );

    let fix_out = run_memguard(dir.path(), &["doctor", "--fix", &id, "--json"]);
    assert_eq!(fix_out.status.code(), Some(1));
}

//! Tier 5 white-box adversarial E2E tests (R8-isolated, opaque-box CLI/MCP/dist).

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

static SEQ: AtomicU64 = AtomicU64::new(1);

fn memguard_exe() -> &'static str {
    env!("CARGO_BIN_EXE_memguard")
}

struct TempDir(PathBuf);
impl TempDir {
    fn new(prefix: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("amg-t5-{}-{}-{}", prefix, std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir");
        Self(p)
    }
    fn new_in(base: &Path, prefix: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = base.join(format!("amg-t5-{}-{}-{}", prefix, std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir in base");
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}
fn terminate_test_watchers(root: &Path) {
    fn walk_watch_files(p: &Path) {
        let Ok(rd) = fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk_watch_files(&path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("disk-watch.json") {
                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&raw) {
                        for rec in arr {
                            if !rec["measured"].is_null() {
                                continue;
                            }
                            if let Some(pid) = rec["watcher_pid"].as_u64().filter(|&x| x > 0) {
                                let pid_str = pid.to_string();
                                let ps = format!(
                                    "$p = Get-CimInstance Win32_Process -Filter 'ProcessId={}' -ErrorAction SilentlyContinue; \
                                     if ($p -and $p.Name -ieq 'memguard.exe' -and $p.CommandLine -and $p.CommandLine.ToLowerInvariant().Contains('watch-child')) {{ Stop-Process -Id {} -Force -ErrorAction SilentlyContinue }}",
                                    pid_str, pid_str
                                );
                                let _ = Command::new("powershell.exe")
                                    .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
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
        }
    }
    walk_watch_files(root);
}

impl Drop for TempDir {
    fn drop(&mut self) {
        terminate_test_watchers(&self.0);
        for _ in 0..10 {
            if fs::remove_dir_all(&self.0).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}

struct KillGuard(Child);
impl Drop for KillGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct EnvKeyGuard(String);
impl EnvKeyGuard {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        Self(format!(r"Software\AtherMemGuard-test\t5-{}-{}-{}", tag, std::process::id(), n))
    }
    fn key(&self) -> &str {
        &self.0
    }
}
impl Drop for EnvKeyGuard {
    fn drop(&mut self) {
        let full = format!(r"HKCU\{}", self.0);
        let _ = Command::new("reg.exe")
            .args(["delete", &full, "/f"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

fn run_mg(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(memguard_exe())
        .args(args)
        .env("ATHER_MEMGUARD_DIR", dir)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("run memguard")
}

fn run_git(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("run git")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

fn age_tree(root: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    fn walk(p: &Path, when: SystemTime) {
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                walk(&e.path(), when);
            }
        }
        if let Ok(f) = OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(p)
        {
            let _ = f.set_modified(when);
        }
    }
    walk(root, when);
}

fn norm_win(s: &str) -> String {
    let mut out = s.replace('/', "\\").to_ascii_lowercase();
    if let Some(rest) = out.strip_prefix(r"\\?\unc\") {
        out = format!(r"\\{}", rest);
    } else if let Some(rest) = out.strip_prefix(r"\\?\") {
        out = rest.to_string();
    }
    while out.ends_with('\\') && !out.ends_with(":\\") && out.len() > 1 {
        out.pop();
    }
    out
}

fn compute_expect_from_cached_checkup(data_dir: &Path, item_id: &str) -> String {
    let raw = fs::read_to_string(data_dir.join("disk-checkup.json")).expect("read disk-checkup.json");
    let v: Value = serde_json::from_str(&raw).expect("parse disk-checkup.json");
    let idx = item_id
        .trim()
        .strip_prefix('d')
        .and_then(|n| n.parse::<usize>().ok())
        .and_then(|n| n.checked_sub(1))
        .expect("valid item id d<n>");
    let item = &v["items"][idx];
    let key = item["key"].as_str().expect("cached item key");
    let paths: Vec<String> = item["paths"]
        .as_array()
        .expect("cached item paths")
        .iter()
        .map(|p| p.as_str().expect("path str").to_string())
        .collect();
    let mut root_ids: Vec<u128> = v["root_ids"][idx]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|x| {
                    x.as_u64()
                        .map(|n| n as u128)
                        .or_else(|| x.as_str().and_then(|s| s.parse::<u128>().ok()))
                })
                .collect()
        })
        .unwrap_or_default();
    root_ids.sort_unstable();

    let mut h: u64 = 0xcbf29ce484222325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    };
    eat(key.as_bytes());
    for p in &paths {
        eat(norm_win(p).as_bytes());
    }
    for id in root_ids {
        eat(&id.to_le_bytes());
    }
    format!("{:016x}", h)
}

fn find_non_system_dest_root(data_dir: &Path) -> Option<PathBuf> {
    let out = run_mg(data_dir, &["disk", "--json"]);
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let vols = v["volumes"].as_array()?;
    for vol in vols {
        let backing = vol["backing"].as_str().unwrap_or("");
        let dev_drive = vol["dev_drive"].as_bool().unwrap_or(false);
        let level = vol["level"].as_str().unwrap_or("");
        let free_gb = vol["free_gb"].as_f64().unwrap_or(0.0);
        let is_system = vol["roles"]
            .as_array()
            .map(|r| r.iter().any(|x| x.as_str() == Some("system")))
            .unwrap_or(true);
        if (backing == "partition" || dev_drive) && !is_system && level == "normal" && free_gb >= 10.0 {
            if let Some(paths) = vol["paths"].as_array() {
                if let Some(p) = paths
                    .iter()
                    .filter_map(|x| x.as_str())
                    .find(|p| p.len() <= 3 && p.as_bytes().get(1) == Some(&b':'))
                {
                    return Some(PathBuf::from(p));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- Challenger 1 E2E tests

/// G3: Full release artifact parity check when `MEMGUARD_CHECK_DIST=1` is set.
#[test]
fn t5_dist_manifest_sha256sums_and_portable_embed_cli_exclusion() {
    if std::env::var("MEMGUARD_CHECK_DIST").as_deref() != Ok("1") {
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dist = root.join("dist");
    let pkg_ver = env!("CARGO_PKG_VERSION");

    let setup_name = format!("AtherMemGuard-Setup-{}.exe", pkg_ver);
    let zip_name = format!("AtherMemGuard-{}-portable.zip", pkg_ver);
    let setup_bytes = fs::read(dist.join(&setup_name)).expect("read Setup.exe");
    let zip_bytes = fs::read(dist.join(&zip_name)).expect("read portable zip");

    let setup_sha = sha256_hex(&setup_bytes);
    let zip_sha = sha256_hex(&zip_bytes);

    let latest_txt = fs::read_to_string(dist.join("latest.json")).expect("read latest.json");
    let latest: Value = serde_json::from_str(&latest_txt).expect("parse latest.json");
    assert_eq!(latest["windows"]["version"].as_str(), Some(pkg_ver));
    assert_eq!(latest["windows"]["size"].as_u64(), Some(setup_bytes.len() as u64));
    assert_eq!(latest["windows"]["sha256"].as_str(), Some(setup_sha.as_str()));

    #[cfg(feature = "gui")]
    {
        let parsed = athermemguard::updater::parse_manifest(&latest_txt).expect("updater::parse_manifest");
        assert_eq!(parsed.version, pkg_ver);
    }

    let sums = fs::read_to_string(dist.join("SHA256SUMS.txt")).expect("read SHA256SUMS.txt");
    assert!(sums.contains(&format!("{}  {}", setup_sha, setup_name)), "missing setup hash in SHA256SUMS.txt");
    assert!(sums.contains(&format!("{}  {}", zip_sha, zip_name)), "missing zip hash in SHA256SUMS.txt");

    // Setup.exe must contain AMGZ embedded CLI marker; portable AtherMemGuard.exe must NOT.
    assert!(setup_bytes.windows(4).any(|w| w == b"AMGZ"), "Setup.exe missing AMGZ embedded CLI");

    let tmp = TempDir::new("dist_zip");
    let unzip_dir = tmp.path().join("unzip");
    let ps = format!(
        "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
        dist.join(&zip_name).display(),
        unzip_dir.display()
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("expand portable zip");
    assert!(out.status.success(), "Expand-Archive failed: {}", String::from_utf8_lossy(&out.stderr));

    let port_gui = fs::read(unzip_dir.join("AtherMemGuard.exe")).expect("portable AtherMemGuard.exe");
    assert!(
        !port_gui.windows(4).any(|w| w == b"AMGZ"),
        "portable AtherMemGuard.exe must not embed AMGZ CLI"
    );
    assert!(unzip_dir.join("memguard.exe").is_file());
    assert!(unzip_dir.join("AtherMemGuard.portable").is_file());
}

/// G6: Live daemon survives 16-thread concurrent IPC burst (`pause`/`resume`/`reload`).
#[test]
fn t5_daemon_concurrent_16_thread_ipc_burst() {
    let dir = TempDir::new("ipc_burst");
    fs::write(dir.path().join("settings.toml"), "interval_sec = 2\n").unwrap();

    let mut daemon = KillGuard(
        Command::new(memguard_exe())
            .arg("daemon")
            .env("ATHER_MEMGUARD_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn daemon"),
    );

    let t0 = Instant::now();
    let mut ready = false;
    while t0.elapsed() < Duration::from_secs(8) {
        let out = run_mg(dir.path(), &["status", "--json"]);
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            if v["daemon"] == true {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "daemon did not become ready");

    let mut handles = Vec::new();
    for i in 0..16 {
        let d = dir.path().to_path_buf();
        handles.push(std::thread::spawn(move || {
            let cmd = match i % 3 {
                0 => "pause",
                1 => "resume",
                _ => "reload",
            };
            let o = run_mg(&d, &[cmd]);
            assert!(
                o.status.success(),
                "concurrent `{}` failed: {}",
                cmd,
                String::from_utf8_lossy(&o.stderr)
            );
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let stop = run_mg(dir.path(), &["stop"]);
    assert!(stop.status.success(), "stop failed after burst");
    let _ = daemon.0.wait();
}

/// G5 / B2-B5: MCP edge-case regression suite (wait timeout_sec=0, BOM-only line, C:: validation, duplicate id cancel, team.toml fallback).
#[test]
fn t5_mcp_wait_zero_timeout_and_edge_protocol() {
    let dir = TempDir::new("mcp_edge");
    let mut child = KillGuard(
        Command::new(memguard_exe())
            .arg("mcp")
            .env("ATHER_MEMGUARD_DIR", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn mcp"),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut read_msg = || -> Value {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("valid JSON-RPC line")
    };

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t5","version":"1"}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let _ = read_msg();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();

    // 1. timeout_sec = 0 with prime = true must return immediately (waited_sec == 0).
    let t0 = Instant::now();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memguard_wait","arguments":{"workload":"build","timeout_sec":0,"prime":true}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let wait0 = read_msg();
    assert!(t0.elapsed() < Duration::from_secs(5), "timeout_sec=0 blocked");
    assert_eq!(wait0["result"]["isError"], false);
    assert_eq!(wait0["result"]["structuredContent"]["waited_sec"], 0);

    // 2. B3: Standalone BOM-only line must be skipped cleanly without -32700 error.
    write!(stdin, "\u{feff}\n").unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":3,"method":"ping"})).unwrap();
    stdin.flush().unwrap();
    let pong = read_msg();
    assert_eq!(pong["id"], 3, "B3: BOM-only line should be skipped, got: {}", pong);
    assert_eq!(pong["result"], json!({}));

    // 3. B4: memguard_disk_bin {"volume": "C::"} must fail with isError == true.
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memguard_disk_bin","arguments":{"volume":"C::"}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let bad_vol = read_msg();
    assert_eq!(bad_vol["id"], 4);
    assert_eq!(bad_vol["result"]["isError"], true, "B4: C:: must be rejected: {}", bad_vol);

    // 4. B2: Duplicate active id=42 waits + notifications/cancelled -> BOTH reply << 5s!
    let t_dup = Instant::now();
    for _ in 0..2 {
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"memguard_wait","arguments":{"ram_gb":1.0e9,"timeout_sec":5}}})
        )
        .unwrap();
    }
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(80));
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut dup_replies = Vec::new();
    while dup_replies.len() < 2 {
        let msg = read_msg();
        if msg["id"] == 42 {
            dup_replies.push(msg);
        }
    }
    assert!(t_dup.elapsed() < Duration::from_secs(3), "B2: duplicate id wait not cancelled promptly ({:?})", t_dup.elapsed());
    for r in &dup_replies {
        assert_eq!(r["result"]["isError"], true, "B2: expected cancelled reply, got {}", r);
    }

    drop(stdin);
    let _ = child.0.wait();

    // 5. B5: Malformed settings.toml + valid team.toml still applies team overrides and reports team_active: true + warning.
    let dir_b5 = TempDir::new("mcp_b5");
    fs::write(dir_b5.path().join("settings.toml"), "interval_sec = [broken\n").unwrap();
    fs::write(dir_b5.path().join("team.toml"), "interval_sec = 42\n").unwrap();

    let mut child_b5 = KillGuard(
        Command::new(memguard_exe())
            .arg("mcp")
            .env("ATHER_MEMGUARD_DIR", dir_b5.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn mcp b5"),
    );
    let mut stdin_b5 = child_b5.0.stdin.take().unwrap();
    let mut stdout_b5 = BufReader::new(child_b5.0.stdout.take().unwrap());
    writeln!(
        stdin_b5,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t5","version":"1"}}})
    )
    .unwrap();
    writeln!(stdin_b5, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
    writeln!(
        stdin_b5,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memguard_config","arguments":{}}})
    )
    .unwrap();
    stdin_b5.flush().unwrap();
    let mut line = String::new();
    stdout_b5.read_line(&mut line).unwrap(); // initialize
    line.clear();
    stdout_b5.read_line(&mut line).unwrap(); // memguard_config
    let cfg_reply: Value = serde_json::from_str(&line).expect("cfg reply json");
    let sc = &cfg_reply["result"]["structuredContent"];
    assert_eq!(sc["config"]["interval_sec"], 42, "B5: team.toml override must apply when settings.toml is broken: {}", sc);
    assert_eq!(sc["team_active"], true, "B5: team_active must be true: {}", sc);
    assert!(sc["warning"].as_str().is_some_and(|w| !w.is_empty()), "B5: parse warning must be surfaced: {}", sc);

    drop(stdin_b5);
    let _ = child_b5.0.wait();
}

/// G8: Bug #2 E2E for `cpu_rate_pct` and `ws_cap_per_process_mb` lifecycle (set -> reload clear -> stop).
#[test]
fn t5_bug2_cpu_rate_and_ws_cap_lifecycle_on_dummy_child() {
    let dir = TempDir::new("bug2_cpu_ws");
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let tag = format!("T5Bug2Ping_{}", std::process::id());
    let exe = dir.path().join(format!("{}.exe", tag));
    fs::copy(PathBuf::from(windir).join(r"System32\PING.EXE"), &exe).unwrap();

    let _dummy = KillGuard(
        Command::new(&exe)
            .args(["-t", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn dummy ping"),
    );
    std::thread::sleep(Duration::from_millis(250));

    let cfg_capped = format!(
        "interval_sec = 2\n[general]\nauto_prime = false\n\
         [[group]]\nname = \"T5CapGroup\"\nprocesses = [\"{}\"]\ncpu_rate_pct = 25\nws_cap_per_process_mb = 64\n",
        tag
    );
    fs::write(dir.path().join("settings.toml"), &cfg_capped).unwrap();

    let mut daemon = KillGuard(
        Command::new(memguard_exe())
            .arg("daemon")
            .env("ATHER_MEMGUARD_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn daemon"),
    );

    let start = Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(10) {
        let out = run_mg(dir.path(), &["status", "--json"]);
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            if v["daemon"] == true
                && v["groups"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|g| g["name"] == "T5CapGroup" && g["processes"] == 1))
            {
                seen = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(seen, "daemon did not track T5CapGroup");

    // Clear cpu_rate_pct and ws_cap_per_process_mb on reload; verify daemon stays clean and stops cleanly.
    let cfg_cleared = format!(
        "interval_sec = 2\n[general]\nauto_prime = false\n\
         [[group]]\nname = \"T5CapGroup\"\nprocesses = [\"{}\"]\ncpu_rate_pct = 0\nws_cap_per_process_mb = 0\n",
        tag
    );
    fs::write(dir.path().join("settings.toml"), &cfg_cleared).unwrap();
    assert!(run_mg(dir.path(), &["reload"]).status.success());
    std::thread::sleep(Duration::from_millis(600));
    assert!(run_mg(dir.path(), &["stop"]).status.success());
    let _ = daemon.0.wait();
}

// ---------------------------------------------------------------- Challenger 2 E2E tests

#[test]
fn t5_e2e_cli_non_ascii_tracked_intermediate_refused_and_logs_cleaned() {
    let td = TempDir::new("cli_non_ascii");
    let data = td.join("data");
    let game = td.join("Game");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(game.join("Intermediate")).unwrap();
    fs::create_dir_all(game.join(r"Saved\Logs")).unwrap();
    fs::write(game.join("Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();

    let tracked = game.join("Intermediate").join("café_日本語.uasset");
    fs::write(&tracked, b"tracked-utf8-asset").unwrap();
    let old_log = game.join(r"Saved\Logs\old.log");
    fs::write(&old_log, vec![b'L'; 4096]).unwrap();

    assert!(run_git(&game, &["init", "-q"]).status.success());
    assert!(run_git(&game, &["config", "user.email", "t5@example.com"]).status.success());
    assert!(run_git(&game, &["config", "user.name", "T5"]).status.success());
    assert!(run_git(&game, &["add", "Game.uproject", "Intermediate/café_日本語.uasset"]).status.success());
    assert!(run_git(&game, &["commit", "-q", "-m", "init"]).status.success());

    age_tree(&game.join("Intermediate"), 4 * 3600);
    age_tree(&game.join(r"Saved\Logs"), 4 * 3600);

    let g_str = game.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &g_str, "--refresh", "--json"]);
    assert!(chk.status.success(), "checkup failed: {}", String::from_utf8_lossy(&chk.stderr));
    let rep: Value = serde_json::from_slice(&chk.stdout).expect("checkup json");
    let cid = rep["scan"]["checkup_id"].as_str().expect("checkup_id").to_string();
    let items = rep["items"].as_array().expect("items");
    let id_inter = items
        .iter()
        .find(|i| i["key"] == "ue:intermediate")
        .expect("ue:intermediate item")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id_logs = items
        .iter()
        .find(|i| i["key"] == "ue:logs")
        .expect("ue:logs item")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let exp_inter = format!("{}={}", id_inter, compute_expect_from_cached_checkup(&data, &id_inter));
    let exp_logs = format!("{}={}", id_logs, compute_expect_from_cached_checkup(&data, &id_logs));
    let fix_inter = format!("{}:now", id_inter);
    let fix_logs = format!("{}:now", id_logs);

    let clean = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            &fix_inter,
            &fix_logs,
            "--yes",
            "--checkup",
            &cid,
            "--expect",
            &exp_inter,
            "--expect",
            &exp_logs,
            "--json",
        ],
    );
    // Exit code is 1 when any outcome is "refused", while stdout still contains the JSON outcome report.
    let clean_json: Value = serde_json::from_slice(&clean.stdout).unwrap_or_else(|e| {
        panic!(
            "clean json parse failed ({}): stdout={} stderr={}",
            e,
            String::from_utf8_lossy(&clean.stdout),
            String::from_utf8_lossy(&clean.stderr)
        )
    });
    let outcomes = clean_json["outcomes"].as_array().expect("outcomes");
    let out_inter = outcomes.iter().find(|o| o["id"] == id_inter).unwrap();
    let out_logs = outcomes.iter().find(|o| o["id"] == id_logs).unwrap();

    assert_eq!(out_inter["result"], "refused", "tracked non-ASCII Intermediate must be refused: {}", out_inter);
    assert!(tracked.is_file(), "tracked non-ASCII file must remain on disk");
    assert_eq!(out_logs["result"], "done", "Saved\\Logs must be cleaned: {}", out_logs);
    assert!(!old_log.exists(), "old.log should be deleted");
}

#[test]
fn t5_e2e_cli_relocate_forward_slash_and_undo_roundtrip() {
    let td = TempDir::new("cli_reloc_fwd");
    let data = td.join("data");
    let local_app = td.join("localapp");
    let user_prof = td.join("profile");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(local_app.join(r"pip\cache")).unwrap();
    fs::create_dir_all(&user_prof).unwrap();
    fs::write(local_app.join(r"pip\cache\wheel.whl"), b"wheel").unwrap();

    let env_guard = EnvKeyGuard::new("reloc_fwd");
    let run_reloc = |args: &[&str]| -> std::process::Output {
        Command::new(memguard_exe())
            .args(args)
            .env("ATHER_MEMGUARD_DIR", &data)
            .env("ATHER_MEMGUARD_ENV_KEY", env_guard.key())
            .env("LOCALAPPDATA", &local_app)
            .env("USERPROFILE", &user_prof)
            .env_remove("UE-LocalDataCachePath")
            .env_remove("UE-ZenSubprocessDataPath")
            .env_remove("NUGET_PACKAGES")
            .env_remove("npm_config_cache")
            .env_remove("PIP_CACHE_DIR")
            .env_remove("UV_CACHE_DIR")
            .env_remove("PNPM_STORE_DIR")
            .env_remove("YARN_CACHE_FOLDER")
            .env_remove("BUN_INSTALL_CACHE_DIR")
            .env_remove("VCPKG_DEFAULT_BINARY_CACHE")
            .env_remove("SCCACHE_DIR")
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("run relocate")
    };

    if let Some(vol_root) = find_non_system_dest_root(&data) {
        let dest_td = TempDir::new_in(&vol_root, "reloc_fwd_dest");
        let fwd_slash = format!("{}/Caches/", dest_td.path().display().to_string().replace('\\', "/"));

        let apply_out = run_reloc(&["disk", "relocate", "--to", &fwd_slash, "--apply", "--yes", "--json"]);
        assert!(apply_out.status.success(), "relocate apply failed: {}", String::from_utf8_lossy(&apply_out.stderr));
        let rec_txt = fs::read_to_string(data.join("disk-relocate.json")).expect("read disk-relocate.json");
        let rec: Value = serde_json::from_str(&rec_txt).expect("parse disk-relocate.json");
        let entries = rec["entries"].as_array().expect("entries");
        assert!(!entries.is_empty());
        for e in entries {
            let val = e["new_value"].as_str().expect("new_value");
            assert!(!val.contains("/\\") && !val.contains("\\\\"), "Bug T5-2.2 malformed new_value: {}", val);
            assert!(!val.contains('/'), "expected normalized backslashes: {}", val);
        }

        let undo_out = run_reloc(&["disk", "relocate", "--undo", "--yes", "--json"]);
        assert!(undo_out.status.success(), "relocate undo failed: {}", String::from_utf8_lossy(&undo_out.stderr));
        let undo_json: Value = serde_json::from_slice(&undo_out.stdout).expect("undo json");
        assert_eq!(undo_json["undone"], true);
    } else {
        // Single-volume host: verify forward-slash system drive path is cleanly refused without crash.
        let plan_out = run_reloc(&["disk", "relocate", "--to", "C:/amg_t5_fwd_test/", "--json"]);
        assert!(plan_out.status.success());
        let plan: Value = serde_json::from_slice(&plan_out.stdout).expect("plan json");
        assert!(!plan["refused"].is_null());
    }
}

#[test]
fn t5_e2e_cli_bare_repo_worktree_and_git_lfs_tool_only() {
    let td = TempDir::new("cli_bare_lfs");
    let data = td.join("data");
    let ws = td.join("ws");
    let bare = ws.join("bare.git");
    let seed = ws.join("seed");
    let lane = ws.join("lane-feat");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&bare).unwrap();

    let git_plain = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("git")
    };
    if !git_plain(&["init", "--bare", "-q", &bare.display().to_string()]).status.success() {
        return;
    }
    assert!(git_plain(&["clone", "-q", &bare.display().to_string(), &seed.display().to_string()]).status.success());
    assert!(run_git(&seed, &["config", "user.email", "t5@example.com"]).status.success());
    assert!(run_git(&seed, &["config", "user.name", "T5"]).status.success());
    fs::write(seed.join("README.md"), b"hi").unwrap();
    assert!(run_git(&seed, &["add", "README.md"]).status.success());
    assert!(run_git(&seed, &["commit", "-q", "-m", "init"]).status.success());
    assert!(run_git(&seed, &["push", "-q", "origin", "HEAD"]).status.success());

    // Add relative worktree + dirty file, and place LFS objects in bare.git/lfs/objects.
    if !run_git(&bare, &["worktree", "add", "--relative-paths", "../lane-feat", "-b", "feat"]).status.success() {
        return;
    }
    fs::write(lane.join("dirty.txt"), b"untracked").unwrap();
    let lfs_obj = bare.join(r"lfs\objects\aa\bb\obj");
    fs::create_dir_all(lfs_obj.parent().unwrap()).unwrap();
    fs::write(&lfs_obj, vec![1u8; 4096]).unwrap();

    // Checkup on `lane` + `bare` (skip `seed` to keep focus on bare + relative worktree).
    let _ = fs::remove_dir_all(&seed);
    let chk = run_mg(&data, &["disk", "checkup", "--path", &ws.display().to_string(), "--refresh", "--json"]);
    assert!(chk.status.success(), "checkup failed: {}", String::from_utf8_lossy(&chk.stderr));
    let rep: Value = serde_json::from_slice(&chk.stdout).expect("checkup json");
    let items = rep["items"].as_array().expect("items");

    let lane_item = items.iter().find(|i| i["key"] == "lane").expect("expected lane item");
    assert_eq!(lane_item["worktree"]["branch"], "feat", "{}", lane_item);
    assert_eq!(lane_item["worktree"]["dirty"], true, "{}", lane_item);
    assert_eq!(lane_item["actions"], json!([]), "lane must offer empty actions: {}", lane_item);

    let lfs_item = items.iter().find(|i| i["key"] == "git:lfs").expect("expected git:lfs item");
    let actions: Vec<&str> = lfs_item["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["kind"].as_str())
        .collect();
    assert_eq!(actions, vec!["tool"], "git:lfs must offer only tool, got: {}", lfs_item);
    assert_eq!(lfs_item["actions"][0]["command"], "git lfs prune", "{}", lfs_item);
}

#[test]
fn t5_e2e_mcp_disk_clean_agent_refuses_disposable_and_git_items() {
    let td = TempDir::new("mcp_agent_clean");
    let data = td.join("data");
    let game = td.join("Game");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(game.join("Intermediate")).unwrap();
    fs::create_dir_all(game.join(r"Saved\Logs")).unwrap();
    fs::write(game.join("Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();

    let inter_obj = game.join(r"Intermediate\build.obj");
    let old_log = game.join(r"Saved\Logs\old.log");
    fs::write(&inter_obj, vec![1u8; 4096]).unwrap();
    fs::write(&old_log, vec![2u8; 4096]).unwrap();
    age_tree(&game.join("Intermediate"), 4 * 3600);
    age_tree(&game.join(r"Saved\Logs"), 4 * 3600);

    let mut child = KillGuard(
        Command::new(memguard_exe())
            .arg("mcp")
            .env("ATHER_MEMGUARD_DIR", &data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn mcp"),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut read_until_id = |want: i64| -> Value {
        loop {
            let mut line = String::new();
            let n = stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "MCP closed before replying to id {}", want);
            let v: Value = serde_json::from_str(&line).expect("json line");
            if v.get("id").and_then(|x| x.as_i64()) == Some(want) {
                return v;
            }
        }
    };

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t5","version":"1"}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let _ = read_until_id(1);
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memguard_disk_checkup","arguments":{"path":game.display().to_string(),"refresh":true}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let chk = read_until_id(2);
    assert_eq!(chk["result"]["isError"], false, "{}", chk);
    let items = chk["result"]["structuredContent"]["items"].as_array().unwrap();
    let id_inter = items.iter().find(|i| i["key"] == "ue:intermediate").unwrap()["id"].as_str().unwrap().to_string();
    let id_logs = items.iter().find(|i| i["key"] == "ue:logs").unwrap()["id"].as_str().unwrap().to_string();

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memguard_disk_plan","arguments":{"ids":[id_inter, id_logs]}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let plan = read_until_id(3);
    assert_eq!(plan["result"]["isError"], false, "{}", plan);
    let plan_id = plan["result"]["structuredContent"]["plan_id"].as_str().unwrap().to_string();

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memguard_disk_clean","arguments":{"ids":[id_inter, id_logs],"plan_id":plan_id}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let clean = read_until_id(4);
    assert_eq!(clean["result"]["isError"], false, "{}", clean);
    let outcomes = clean["result"]["structuredContent"]["outcomes"].as_array().unwrap();
    let out_inter = outcomes.iter().find(|o| o["id"] == id_inter).unwrap();
    let out_logs = outcomes.iter().find(|o| o["id"] == id_logs).unwrap();

    assert_eq!(out_inter["result"], "done", "regenerable Intermediate must clean via MCP: {}", out_inter);
    assert!(!inter_obj.exists(), "Intermediate build.obj should be deleted");
    assert_eq!(out_logs["result"], "refused", "disposable Saved\\Logs must be refused by MCP agent mode: {}", out_logs);
    assert!(old_log.is_file(), "Saved\\Logs\\old.log must remain untouched");

    drop(stdin);
    let _ = child.0.wait();
}

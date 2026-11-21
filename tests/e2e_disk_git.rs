//! Opaque-box E2E tests for Feature Areas F5 (Disk Checkup, Plan, Clean, Undo & Bin)
//! and F6 (Disk Relocate, Git/LFS/Worktrees, ReFS/VHDX & Expanded Catalog).
//!
//! Strictly opaque-box: uses only `std`, `serde_json`, and `env!("CARGO_BIN_EXE_memguard")`.
//! Every test isolates `ATHER_MEMGUARD_DIR` (and `ATHER_MEMGUARD_ENV_KEY` for relocate)
//! and cleans up all temporary files, `:bin` moves (via `disk undo`), and throwaway
//! registry keys in panic-safe RAII `Drop` guards.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde_json::Value;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

static SEQ: AtomicU64 = AtomicU64::new(1);
static BIN_MUTEX: Mutex<()> = Mutex::new(());

fn memguard_bin() -> &'static str {
    env!("CARGO_BIN_EXE_memguard")
}

fn bin_lock() -> MutexGuard<'static, ()> {
    BIN_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

fn clear_readonly_if_set(p: &Path, m: &fs::Metadata) {
    let mut perms = m.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        let _ = fs::set_permissions(p, perms);
    }
}

fn remove_tree_once(p: &Path) -> std::io::Result<()> {
    let m = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        clear_readonly_if_set(p, &m);
        if m.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return fs::remove_dir(p);
        }
        return fs::remove_file(p);
    }
    if m.is_dir() {
        let mut first_err: Option<std::io::Error> = None;
        if let Ok(rd) = fs::read_dir(p) {
            for entry in rd.flatten() {
                if let Err(e) = remove_tree_once(&entry.path()) {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        clear_readonly_if_set(p, &m);
        if let Err(e) = fs::remove_dir(p) {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    } else {
        clear_readonly_if_set(p, &m);
        fs::remove_file(p)
    }
}

fn remove_tree_safe(p: &Path) {
    for attempt in 0..15 {
        if !p.exists() && fs::symlink_metadata(p).is_err() {
            return;
        }
        if remove_tree_once(p).is_ok() {
            return;
        }
        if attempt < 14 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "amg_e2e_disk_git_{}_{}_{}",
            std::process::id(),
            seq,
            tag
        ));
        fs::create_dir_all(&path).expect("create per-test temp dir");
        Self { path }
    }

    fn new_in(base: &Path, tag: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("amg_e2e_disk_git_{}_{}_{}", std::process::id(), seq, tag));
        fs::create_dir_all(&path).expect("create temp dir on target volume");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }
}

struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("child still present")
    }

    fn wait(mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.take().expect("child still present").wait()
    }

    fn wait_with_output(mut self) -> std::io::Result<Output> {
        self.0.take().expect("child still present").wait_with_output()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut ch) = self.0.take() {
            let _ = ch.kill();
            let _ = ch.wait();
        }
    }
}

fn is_memguard_watch_child(pid: u64) -> bool {
    let ps = format!(
        "$p = Get-CimInstance Win32_Process -Filter 'ProcessId={}' -ErrorAction SilentlyContinue; \
         if ($p -and $p.Name -ieq 'memguard.exe' -and $p.CommandLine -and $p.CommandLine.ToLowerInvariant().Contains('watch-child')) {{ exit 0 }} else {{ exit 1 }}",
        pid
    );
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn terminate_test_watchers(root: &Path) {
    fn walk_watch_files(p: &Path) {
        let Ok(m) = fs::symlink_metadata(p) else { return };
        if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return;
        }
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    walk_watch_files(&e.path());
                }
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some("disk-watch.json") {
            if let Ok(raw) = fs::read_to_string(p) {
                if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&raw) {
                    for rec in arr {
                        if !rec["measured"].is_null() {
                            continue;
                        }
                        if let Some(pid) = rec["watcher_pid"].as_u64().filter(|&x| x > 0) {
                            if is_memguard_watch_child(pid) {
                                let pid_str = pid.to_string();
                                let _ = Command::new("taskkill")
                                    .args(["/F", "/PID", &pid_str, "/FI", "IMAGENAME eq memguard.exe"])
                                    .creation_flags(CREATE_NO_WINDOW)
                                    .output();
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
        terminate_test_watchers(&self.path);
        remove_tree_safe(&self.path);
    }
}

struct EnvKeyGuard {
    subkey: String,
}

impl EnvKeyGuard {
    fn new(tag: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let subkey = format!(
            r"Software\AtherMemGuard-test\e2e_disk_git_{}_{}_{}",
            std::process::id(),
            seq,
            tag
        );
        Self { subkey }
    }

    fn key(&self) -> &str {
        &self.subkey
    }
}

impl Drop for EnvKeyGuard {
    fn drop(&mut self) {
        let hkcu = format!(r"HKCU\{}", self.subkey);
        let _ = Command::new("reg.exe")
            .args(["delete", &hkcu, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
}

struct BinGuard {
    data_dir: PathBuf,
    undo_ids: Vec<String>,
    blocking_paths: Vec<PathBuf>,
}

impl BinGuard {
    fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            undo_ids: Vec::new(),
            blocking_paths: Vec::new(),
        }
    }

    fn register(&mut self, undo_id: &str) {
        self.undo_ids.push(undo_id.to_string());
    }

    fn mark_undone(&mut self, undo_id: &str) {
        self.undo_ids.retain(|u| u != undo_id);
    }

    fn add_blocking_path(&mut self, path: PathBuf) {
        self.blocking_paths.push(path);
    }

    fn clear_blocking_paths(&mut self) {
        self.blocking_paths.clear();
    }
}

impl Drop for BinGuard {
    fn drop(&mut self) {
        for p in &self.blocking_paths {
            remove_tree_safe(p);
        }
        for uid in self.undo_ids.iter().rev() {
            let _ = Command::new(memguard_bin())
                .env("ATHER_MEMGUARD_DIR", &self.data_dir)
                .args(["disk", "undo", uid, "--json"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
}

struct CmdOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| {
            panic!(
                "expected valid JSON stdout (exit={}): {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.code, e, self.stdout, self.stderr
            )
        })
    }
}

fn mg_cmd(data_dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(memguard_bin());
    cmd.env("ATHER_MEMGUARD_DIR", data_dir);
    cmd.args(args);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn run_cmd(mut cmd: Command) -> CmdOutput {
    let out: Output = cmd.output().expect("spawn command");
    CmdOutput {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn run_mg(data_dir: &Path, args: &[&str]) -> CmdOutput {
    run_cmd(mg_cmd(data_dir, args))
}

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, bytes).expect("write file");
}

fn write_uproject(dir: &Path) {
    write_file(
        &dir.join("Game.uproject"),
        br#"{ "FileVersion": 3, "EngineAssociation": "5.4", "Category": "", "Modules": [] }"#,
    );
}

fn write_uplugin(dir: &Path, name: &str) {
    write_file(&dir.join(format!("{}.uplugin", name)), b"{}");
}

fn set_mtime_win(path: &Path, when: SystemTime) {
    let f = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap_or_else(|e| panic!("open {} for mtime: {}", path.display(), e));
    f.set_modified(when)
        .unwrap_or_else(|e| panic!("set_modified {}: {}", path.display(), e));
}

fn age_tree_bottom_up(root: &Path, ago: Duration) {
    let when = SystemTime::now() - ago;
    fn walk(p: &Path, when: SystemTime) {
        if let Ok(m) = fs::symlink_metadata(p) {
            if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return;
            }
            if m.is_dir() {
                if let Ok(rd) = fs::read_dir(p) {
                    for e in rd.flatten() {
                        walk(&e.path(), when);
                    }
                }
            }
            set_mtime_win(p, when);
        }
    }
    walk(root, when);
}

fn make_junction(link: &Path, target: &Path) {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).expect("create junction parent");
    }
    let out = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("run mklink /J");
    assert!(
        out.status.success(),
        "mklink /J {} -> {} failed: stdout={} stderr={}",
        link.display(),
        target.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_git(dir: &Path, args: &[&str]) -> CmdOutput {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    cmd.args(args);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = run_cmd(cmd);
    assert_eq!(
        out.code, 0,
        "git {:?} in {} failed:\nstdout: {}\nstderr: {}",
        args,
        dir.display(),
        out.stdout,
        out.stderr
    );
    out
}

fn init_git_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("create repo dir");
    run_git(dir, &["init"]);
    run_git(dir, &["config", "user.name", "E2E Tester"]);
    run_git(dir, &["config", "user.email", "e2e@example.com"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

fn norm_win(p: &str) -> String {
    let mut s = p.replace('/', "\\").to_lowercase();
    for prefix in [r"\\?\unc\", r"\\?\"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = if prefix.ends_with("unc\\") {
                format!(r"\\{}", rest)
            } else {
                rest.to_string()
            };
        }
    }
    while s.ends_with('\\') && !s.ends_with(":\\") && s.len() > 1 {
        s.pop();
    }
    s
}

fn fnv1a_fingerprint(key: &str, paths: &[String], root_ids: &[u128]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for x in b {
            h ^= *x as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    eat(key.as_bytes());
    for p in paths {
        eat(norm_win(p).as_bytes());
    }
    let mut ids = root_ids.to_vec();
    ids.sort_unstable();
    for id in ids {
        eat(&id.to_le_bytes());
    }
    format!("{:016x}", h)
}

fn compute_expect_from_cached_checkup(data_dir: &Path, item_id: &str) -> String {
    let raw = fs::read_to_string(data_dir.join("disk-checkup.json"))
        .expect("read disk-checkup.json");
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
    let root_ids: Vec<u128> = v["root_ids"][idx]
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
    fnv1a_fingerprint(key, &paths, &root_ids)
}

fn find_item_by_key<'a>(rep: &'a Value, key: &str) -> &'a Value {
    rep["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|it| it["key"].as_str() == Some(key))
        .unwrap_or_else(|| panic!("expected item with key '{}' in {}", key, rep))
}

fn item_has_path_ci(it: &Value, needle: &str) -> bool {
    let needle_norm = norm_win(needle);
    it["paths"]
        .as_array()
        .map(|arr| {
            arr.iter().any(|p| {
                p.as_str()
                    .map(|s| norm_win(s).contains(&needle_norm))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn isolate_relocate_cmd(
    data_dir: &Path,
    env_key: &str,
    local_app: &Path,
    user_profile: &Path,
    args: &[&str],
) -> Command {
    let mut cmd = mg_cmd(data_dir, args);
    cmd.env("ATHER_MEMGUARD_ENV_KEY", env_key);
    cmd.env("LOCALAPPDATA", local_app);
    cmd.env("USERPROFILE", user_profile);
    cmd.env_remove("UE-LocalDataCachePath");
    cmd.env_remove("NUGET_PACKAGES");
    cmd.env_remove("npm_config_cache");
    cmd.env_remove("PIP_CACHE_DIR");
    cmd.env_remove("VCPKG_DEFAULT_BINARY_CACHE");
    cmd
}

fn find_non_system_dest_root(data_dir: &Path) -> Option<PathBuf> {
    let out = run_mg(data_dir, &["disk", "--json"]);
    if out.code != 0 {
        return None;
    }
    let v = out.json();
    let vols = v["volumes"].as_array()?;
    for vol in vols {
        let backing = vol["backing"].as_str().unwrap_or("");
        let dev_drive = vol["dev_drive"].as_bool().unwrap_or(false);
        let level = vol["level"].as_str().unwrap_or("");
        let free_gb = vol["free_gb"].as_f64().unwrap_or(0.0);
        let roles = vol["roles"].as_array();
        let is_system = roles
            .map(|r| r.iter().any(|x| x.as_str() == Some("system")))
            .unwrap_or(true);
        if (backing == "partition" || dev_drive) && !is_system && level == "normal" && free_gb >= 10.0
        {
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

// ============================================================================
// F5 Tier 1 (5 tests)
// ============================================================================

#[test]
fn t1_f5_disk_report_schema() {
    let td = TempDir::new("t1_f5_disk_report");
    let data = td.join("data");
    fs::create_dir_all(&data).unwrap();

    for args in [&["disk", "--json"][..], &["disk", "--refresh", "--json"][..]] {
        let out = run_mg(&data, args);
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let rep = out.json();

        let level = rep["level"].as_str().expect("level str");
        assert!(
            matches!(level, "normal" | "elevated" | "critical"),
            "unexpected level: {}",
            level
        );

        let vols = rep["volumes"].as_array().expect("volumes array");
        assert!(!vols.is_empty(), "expected at least one volume");
        for v in vols {
            assert!(
                v["volume_id"].as_str().is_some_and(|s| !s.is_empty()),
                "volume_id missing in {}",
                v
            );
            assert!(v["paths"].is_array());
            assert!(v["fs"].is_string());
            assert!(v["dev_drive"].is_boolean());
            assert!(v["block_cloning"].is_boolean());
            let backing = v["backing"].as_str().expect("backing str");
            assert!(matches!(
                backing,
                "partition" | "vhd" | "network" | "unknown"
            ));
            assert_eq!(v["free_basis"], "available_to_caller");
            let total_gb = v["total_gb"].as_f64().expect("total_gb");
            let free_gb = v["free_gb"].as_f64().expect("free_gb");
            assert!(total_gb >= free_gb, "total_gb < free_gb in {}", v);
            let free_pct = v["free_pct"].as_f64().expect("free_pct");
            assert!((0.0..=100.0).contains(&free_pct));
            assert!(v["roles"].is_array());
            assert!(v["notes"].is_array());
        }

        assert!(rep["dev_drive"].is_object());
        assert!(rep["dev_drive"]["av_filters"].is_string());
        assert!(rep["dev_drive"]["defender_performance_mode"].is_string());
        assert!(rep["findings"].is_array());
        assert!(rep["advice"].is_array());
        assert!(rep["recent_cleanups"].is_array());
    }
}

#[test]
fn t1_f5_checkup_and_plan_lifecycle() {
    let td = TempDir::new("t1_f5_checkup_plan");
    let data = td.join("data");
    let proj = td.join("MyGame");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    write_file(&proj.join(r"Intermediate\a.obj"), &vec![0x5Au8; 10 << 20]);
    write_file(&proj.join(r"Saved\Logs\g.log"), &vec![b'L'; 100 << 10]);
    write_file(&proj.join(r".git\lfs\objects\obj1"), &vec![1u8; 1024]);
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let out1 = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(out1.code, 0, "stderr: {}", out1.stderr);
    let rep1 = out1.json();

    let cid = rep1["scan"]["checkup_id"].as_str().expect("checkup_id");
    assert!(cid.starts_with('c'), "checkup_id={}", cid);
    assert_eq!(rep1["scan"]["from_cache"], false);
    assert_eq!(rep1["basis"], "predicted");

    let interm = find_item_by_key(&rep1, "ue:intermediate");
    assert_eq!(interm["class"], "regenerable");
    assert_eq!(interm["actions"][0]["kind"], "now");
    let d_interm = interm["id"].as_str().unwrap().to_string();

    let logs = find_item_by_key(&rep1, "ue:logs");
    assert_eq!(logs["class"], "disposable");
    assert_eq!(logs["actions"][0]["kind"], "bin");
    assert_eq!(logs["actions"][0]["frees_now_gb"], 0.0);
    let d_logs = logs["id"].as_str().unwrap().to_string();

    let lfs = find_item_by_key(&rep1, "git:lfs");
    assert_eq!(lfs["class"], "review");

    let totals = rep1["totals"].as_array().expect("totals");
    assert_eq!(totals[0]["set"], "regenerable");
    assert_eq!(totals[0]["frees_basis"], "predicted");

    let out2 = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--json"]);
    assert_eq!(out2.code, 0, "stderr: {}", out2.stderr);
    let rep2 = out2.json();
    assert_eq!(rep2["scan"]["from_cache"], true);
    assert_eq!(rep2["scan"]["checkup_id"], cid);

    let plan_out = run_mg(&data, &["disk", "plan", &d_interm, &d_logs, "--json"]);
    assert_eq!(plan_out.code, 0, "stderr: {}", plan_out.stderr);
    let plan = plan_out.json();
    let pid = plan["plan_id"].as_str().expect("plan_id");
    assert!(pid.starts_with('p'), "plan_id={}", pid);
    assert_eq!(plan["basis"], "predicted");
    let plans = plan["plans"].as_array().expect("plans array");
    assert!(!plans.is_empty());
    let item_ids: Vec<&str> = plans[0]["item_ids"]
        .as_array()
        .expect("item_ids array")
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert!(
        item_ids.contains(&d_interm.as_str()) && item_ids.contains(&d_logs.as_str()),
        "expected {} and {} in plans[0].item_ids: {:?}",
        d_interm,
        d_logs,
        item_ids
    );
    assert!(
        item_ids.contains(&"d1") && item_ids.contains(&"d2"),
        "expected d1 and d2 in plans[0].item_ids: {:?}",
        item_ids
    );
    assert!(
        plans[0]["frees_together_gb"].as_f64().unwrap_or(0.0) > 0.0,
        "expected frees_together_gb > 0.0 in {}",
        plans[0]
    );
    assert!(plans[0]["free_now_gb"].is_number());
    assert!(plans[0]["free_after_gb"].is_number());
    assert_eq!(plans[0]["free_after_basis"], "predicted");
}

#[test]
fn t1_f5_clean_dry_run_schema() {
    let td = TempDir::new("t1_f5_dry_run");
    let data = td.join("data");
    let proj = td.join("DryProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    let obj = proj.join(r"Intermediate\a.obj");
    write_file(&obj, &vec![0x3Cu8; 10 << 20]);
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let id = find_item_by_key(&rep, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:now", id);
    let dry = run_mg(&data, &["disk", "clean", "--fix", &fix_arg, "--dry-run", "--json"]);
    assert_eq!(dry.code, 0, "stderr: {}", dry.stderr);
    let res = dry.json();
    assert_eq!(res["dry_run"], true);
    assert!(res["plan_id"].as_str().is_some_and(|s| s.starts_with('p')));
    let outcomes = res["outcomes"].as_array().expect("outcomes");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["id"], id);
    assert_eq!(outcomes[0]["action"], "now");
    assert_eq!(outcomes[0]["result"], "would_do");
    assert_eq!(outcomes[0]["predicted_freed_gb"], 0.0);
    assert!(res["volumes"].as_array().is_some_and(|v| v.is_empty()));
    assert!(obj.exists(), "dry-run must not delete files");
}

#[test]
fn t1_f5_clean_now_with_checkup_and_expect() {
    let td = TempDir::new("t1_f5_clean_now");
    let data = td.join("data");
    let proj = td.join("NowProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    write_file(&proj.join(r"Intermediate\a.obj"), &vec![0x77u8; 10 << 20]);
    let keep_uasset = proj.join(r"Content\keep.uasset");
    write_file(&keep_uasset, b"important-content");
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let fp = compute_expect_from_cached_checkup(&data, &id);

    let fix_arg = format!("{}:now", id);
    let expect_arg = format!("{}={}", id, fp);
    let clean = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            &fix_arg,
            "--yes",
            "--checkup",
            &cid,
            "--expect",
            &expect_arg,
            "--json",
        ],
    );
    assert_eq!(clean.code, 0, "stderr: {}\nstdout: {}", clean.stderr, clean.stdout);
    let res = clean.json();
    assert_eq!(res["dry_run"], false);
    let o = &res["outcomes"][0];
    assert_eq!(o["result"], "done");
    assert_eq!(o["action"], "now");
    assert!(o["undo_id"].is_null());
    assert_eq!(o["basis"], "predicted");
    assert!(o["removed"]["files"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(o["removed"]["remains"], false);
    assert!(res["volumes"][0]["predicted_freed_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(res["bin_note"].is_null());
    assert!(!proj.join("Intermediate").exists());
    assert_eq!(fs::read(&keep_uasset).unwrap(), b"important-content");
}

#[test]
fn t1_f5_bin_manifest_and_undo_round_trip() {
    let _lock = bin_lock();
    let td = TempDir::new("t1_f5_bin_undo");
    let data = td.join("data");
    let proj = td.join("BinProj");
    fs::create_dir_all(&data).unwrap();
    let mut bin_guard = BinGuard::new(&data);

    write_uproject(&proj);
    let log_file = proj.join(r"Saved\Logs\app.log");
    write_file(&log_file, &vec![b'A'; 4096]);
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:logs")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:bin", id);
    let clean = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            &fix_arg,
            "--yes",
            "--checkup",
            &cid,
            "--json",
        ],
    );
    assert_eq!(clean.code, 0, "stderr: {}\nstdout: {}", clean.stderr, clean.stdout);
    let res = clean.json();

    // Register undo_id immediately BEFORE further checks so Drop restores on any failure.
    let undo_id = res["outcomes"][0]["undo_id"]
        .as_str()
        .expect("undo_id for :bin")
        .to_string();
    assert!(!undo_id.is_empty());
    bin_guard.register(&undo_id);

    assert_eq!(res["outcomes"][0]["result"], "done");
    assert_eq!(res["outcomes"][0]["action"], "bin");
    assert_eq!(res["outcomes"][0]["predicted_freed_gb"], 0.0);
    assert!(res["bin_note"].is_string());
    assert!(res["volumes"].as_array().is_some_and(|v| v.is_empty()));
    assert!(!log_file.exists(), "original log file should be moved to bin");

    let run_id = res["run_id"].as_str().expect("run_id").to_string();
    let vol_root = rep["volumes"][0]["paths"][0]
        .as_str()
        .map(PathBuf::from)
        .expect("volume root in checkup report");
    let run_path = vol_root.join(".AtherMemGuard-bin").join(&run_id);
    let manifest_path = run_path.join("manifest.json");
    assert!(
        manifest_path.is_file(),
        "expected move-time manifest.json at {} BEFORE calling disk bin (M2 F1.2a)",
        manifest_path.display()
    );
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).expect("manifest JSON");
    assert!(
        manifest["entries"]
            .as_array()
            .is_some_and(|e| !e.is_empty()),
        "expected non-empty move-time manifest entries: {}",
        manifest
    );
    let manifest_holds = manifest["holds"].as_u64().expect("manifest holds u64");
    assert!(manifest_holds >= 4096, "manifest holds={}", manifest_holds);

    let bin_out = run_mg(&data, &["disk", "bin", "--json"]);
    assert_eq!(bin_out.code, 0, "stderr: {}", bin_out.stderr);
    let bin_json = bin_out.json();
    assert_eq!(bin_json["hold_hours"], 24.0);
    let run = bin_json["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|r| r["run_id"].as_str() == Some(&run_id))
        .unwrap_or_else(|| panic!("run_id {} missing from bin list: {}", run_id, bin_json));
    let holds = run["holds"].as_u64().expect("holds u64");
    assert_eq!(holds, manifest_holds);
    assert!(!run["titles"].as_array().unwrap().is_empty());
    assert_eq!(
        PathBuf::from(run["path"].as_str().expect("run path")),
        run_path
    );

    let undo_out = run_mg(&data, &["disk", "undo", &undo_id, "--json"]);
    assert_eq!(undo_out.code, 0, "stderr: {}\nstdout: {}", undo_out.stderr, undo_out.stdout);
    let undo_json = undo_out.json();
    assert_eq!(undo_json["outcomes"][0]["result"], "undone");
    assert_eq!(undo_json["outcomes"][0]["undo_id"], undo_id);
    bin_guard.mark_undone(&undo_id);

    assert!(log_file.exists(), "undo should restore Saved\\Logs\\app.log");
    assert!(!run_path.exists(), "bin run folder should be removed after undo");
}

// ============================================================================
// F5 Tier 2 (8 tests)
// ============================================================================

#[test]
fn t2_f5_min_age_refusal_vs_zero_override() {
    let td = TempDir::new("t2_f5_min_age");
    let data = td.join("data");
    let proj = td.join("AgeProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    let fresh = proj.join(r"Intermediate\fresh.obj");
    write_file(&fresh, b"fresh-bytes");

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:now", id);
    let refused = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            &fix_arg,
            "--yes",
            "--checkup",
            &cid,
            "--json",
        ],
    );
    assert_eq!(refused.code, 1);
    let rjson = refused.json();
    assert_eq!(rjson["outcomes"][0]["result"], "refused");
    assert!(
        rjson["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("last 2 hours"),
        "msg={}",
        rjson["outcomes"][0]["message"]
    );
    assert!(fresh.exists());

    write_file(&data.join("settings.toml"), b"[disk]\nmin_age_hours = 0.0\n");
    let dry = run_mg(&data, &["disk", "clean", "--fix", &fix_arg, "--dry-run", "--json"]);
    assert_eq!(dry.code, 0, "stderr: {}", dry.stderr);
    assert_eq!(dry.json()["outcomes"][0]["result"], "would_do");
}

#[test]
fn t2_f5_missing_checkup_and_invalid_plan_ids() {
    let td = TempDir::new("t2_f5_missing_chk");
    let data = td.join("data");
    fs::create_dir_all(&data).unwrap();

    let p_miss = run_mg(&data, &["disk", "plan", "--json"]);
    assert_eq!(p_miss.code, 1);
    assert!(p_miss.stderr.contains("Run a disk checkup first"));

    let c_miss = run_mg(&data, &["disk", "clean", "--fix", "d1", "--dry-run", "--json"]);
    assert_eq!(c_miss.code, 1);
    assert!(c_miss.stderr.contains("checkup first"));

    let proj = td.join("PlanProj");
    write_uproject(&proj);
    write_file(&proj.join(r"Intermediate\a.obj"), b"1234");
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0);

    let p_bad = run_mg(&data, &["disk", "plan", "d99", "--json"]);
    assert_eq!(p_bad.code, 1);
    assert!(p_bad.stderr.contains("None of these ids"));

    let p_mix = run_mg(&data, &["disk", "plan", "d1", "d99", "--json"]);
    assert_eq!(p_mix.code, 0, "stderr: {}", p_mix.stderr);
    let mix = p_mix.json();
    assert_eq!(mix["plans"][0]["item_ids"], serde_json::json!(["d1"]));
    assert_eq!(mix["refused"][0]["id"], "d99");
}

#[test]
fn t2_f5_clean_cli_guards_and_bad_expect() {
    let td = TempDir::new("t2_f5_cli_guards");
    let data = td.join("data");
    let proj = td.join("GuardProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    let obj = proj.join(r"Intermediate\a.obj");
    write_file(&obj, b"guard-bytes");
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0);
    let cid = chk.json()["scan"]["checkup_id"].as_str().unwrap().to_string();

    let g1 = run_mg(&data, &["disk", "clean", "--fix", "d1", "--yes", "--json"]);
    assert_eq!(g1.code, 1);
    assert!(g1.stderr.contains("--checkup"), "stderr={}", g1.stderr);

    let g2 = run_mg(&data, &["disk", "clean", "--fix", "d1", "--json"]);
    assert_eq!(g2.code, 1);
    assert!(g2.stderr.contains("--yes"), "stderr={}", g2.stderr);

    let g3 = run_mg(
        &data,
        &["disk", "clean", "--fix", "d1", "--yes", "--checkup", "c_wrong", "--json"],
    );
    assert_eq!(g3.code, 1);
    assert!(g3.stderr.contains("checkup changed"), "stderr={}", g3.stderr);

    let g4 = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            "d1",
            "--yes",
            "--checkup",
            &cid,
            "--expect",
            "d1=0000000000000000",
            "--json",
        ],
    );
    assert_eq!(g4.code, 1);
    assert!(g4.stderr.contains("checkup changed"), "stderr={}", g4.stderr);

    let g5 = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            "d1",
            "--checkup",
            &cid,
            "--expect",
            "d1=nothex",
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(g5.code, 1);
    assert!(g5.stderr.contains("16 hex digits"), "stderr={}", g5.stderr);

    let g6 = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            "d1",
            "--expect",
            "d1=0000000000000000",
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(g6.code, 1);
    assert!(g6.stderr.contains("--expect goes with --checkup"), "stderr={}", g6.stderr);

    assert!(obj.exists());
}

#[test]
fn t2_f5_bug6_root_mtime_alignment() {
    let td = TempDir::new("t2_f5_bug6");
    let data1 = td.join("data1");
    let proj1 = td.join("ProjAlign");
    fs::create_dir_all(&data1).unwrap();
    write_uproject(&proj1);
    let inter1 = proj1.join("Intermediate");
    let file1 = inter1.join("a.obj");
    write_file(&file1, &vec![0x11u8; 4096]);

    // Child aged 5h, root Intermediate dir aged 3h (root_mtime > child_mtime, both > 2h).
    set_mtime_win(&file1, SystemTime::now() - Duration::from_secs(5 * 3600));
    set_mtime_win(&inter1, SystemTime::now() - Duration::from_secs(3 * 3600));

    let p1 = proj1.display().to_string();
    let chk1 = run_mg(&data1, &["disk", "checkup", "--path", &p1, "--refresh", "--json"]);
    assert_eq!(chk1.code, 0, "stderr: {}", chk1.stderr);
    let rep1 = chk1.json();
    let cid1 = rep1["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id1 = find_item_by_key(&rep1, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix1 = format!("{}:now", id1);
    let dry1 = run_mg(&data1, &["disk", "clean", "--fix", &fix1, "--dry-run", "--json"]);
    assert_eq!(dry1.code, 0, "dry-run stderr={} stdout={}", dry1.stderr, dry1.stdout);
    assert_eq!(dry1.json()["outcomes"][0]["result"], "would_do");

    let clean1 = run_mg(
        &data1,
        &[
            "disk",
            "clean",
            "--fix",
            &fix1,
            "--yes",
            "--checkup",
            &cid1,
            "--json",
        ],
    );
    assert_eq!(clean1.code, 0, "clean stderr={} stdout={}", clean1.stderr, clean1.stdout);
    assert_eq!(clean1.json()["outcomes"][0]["result"], "done");
    assert!(!inter1.exists());

    // Part 2: Modifying mtime AFTER plan (via MCP session plan_id -> memguard_disk_clean)
    // to 2.5h ago (> min_age 2h, newer than 5h plan) MUST refuse with "after the plan".
    {
        use std::io::{BufRead, BufReader, Write};
        use std::process::Stdio;

        let data2 = td.join("data2");
        let proj2 = td.join("ProjAfterPlan");
        fs::create_dir_all(&data2).unwrap();
        write_uproject(&proj2);
        let inter2 = proj2.join("Intermediate");
        let file2 = inter2.join("b.obj");
        write_file(&file2, &vec![0x22u8; 4096]);
        age_tree_bottom_up(&proj2, Duration::from_secs(5 * 3600));

        let mut child = ChildGuard::new(
            mg_cmd(&data2, &["mcp"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn memguard mcp"),
        );
        let mut stdin = child.child_mut().stdin.take().unwrap();
        let mut stdout = BufReader::new(child.child_mut().stdout.take().unwrap());

        let send_recv = |stdin: &mut std::process::ChildStdin,
                         stdout: &mut BufReader<std::process::ChildStdout>,
                         req: Value|
         -> Value {
            let line = serde_json::to_string(&req).unwrap();
            writeln!(stdin, "{}", line).unwrap();
            stdin.flush().unwrap();
            let mut resp = String::new();
            loop {
                resp.clear();
                let n = stdout.read_line(&mut resp).expect("read mcp line");
                assert!(n > 0, "unexpected EOF from memguard mcp");
                let v: Value = serde_json::from_str(resp.trim()).expect("parse mcp json");
                if v.get("id").is_some() {
                    return v;
                }
            }
        };

        let init = send_recv(
            &mut stdin,
            &mut stdout,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "e2e", "version": "1.0" }
                }
            }),
        );
        assert!(init["result"]["serverInfo"].is_object(), "{}", init);
        writeln!(
            stdin,
            "{}",
            serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .unwrap();
        stdin.flush().unwrap();

        let chk_resp = send_recv(
            &mut stdin,
            &mut stdout,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "memguard_disk_checkup",
                    "arguments": { "path": proj2.display().to_string(), "refresh": true }
                }
            }),
        );
        assert_eq!(chk_resp["result"]["isError"], false, "{}", chk_resp);
        let id2 = find_item_by_key(&chk_resp["result"]["structuredContent"], "ue:intermediate")["id"]
            .as_str()
            .unwrap()
            .to_string();

        let plan_resp = send_recv(
            &mut stdin,
            &mut stdout,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "memguard_disk_plan",
                    "arguments": { "ids": [id2.clone()] }
                }
            }),
        );
        assert_eq!(plan_resp["result"]["isError"], false, "{}", plan_resp);
        let plan_id = plan_resp["result"]["structuredContent"]["plan_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Touch file2 to 2.5h ago (still > 2h min_age, but newer than 5h plan).
        set_mtime_win(&file2, SystemTime::now() - Duration::from_secs(9000));

        let clean_resp = send_recv(
            &mut stdin,
            &mut stdout,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "memguard_disk_clean",
                    "arguments": { "plan_id": plan_id, "ids": [id2] }
                }
            }),
        );
        drop(stdin);
        let _ = child.wait();

        let sc = &clean_resp["result"]["structuredContent"];
        assert_eq!(sc["outcomes"][0]["result"], "refused", "{}", clean_resp);
        assert!(
            sc["outcomes"][0]["message"]
                .as_str()
                .unwrap_or("")
                .contains("after the plan"),
            "msg={}",
            sc["outcomes"][0]["message"]
        );
        assert!(file2.exists());
    }
}

#[test]
fn t2_f5_keep_protection_refusal() {
    let td = TempDir::new("t2_f5_keep");
    let data = td.join("data");
    let proj = td.join("KeepProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    let obj = proj.join(r"Intermediate\a.obj");
    write_file(&obj, b"keep-me");
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk1 = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk1.code, 0);
    let rep1 = chk1.json();
    let cid = rep1["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep1, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let keep_toml = format!("[disk]\nkeep = ['{}']\n", proj.join("Intermediate").display());
    write_file(&data.join("settings.toml"), keep_toml.as_bytes());

    let fix_arg = format!("{}:now", id);
    let clean = run_mg(
        &data,
        &[
            "disk",
            "clean",
            "--fix",
            &fix_arg,
            "--yes",
            "--checkup",
            &cid,
            "--json",
        ],
    );
    assert_eq!(clean.code, 1);
    let cjson = clean.json();
    assert_eq!(cjson["outcomes"][0]["result"], "refused");
    assert!(
        cjson["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("[disk] keep"),
        "msg={}",
        cjson["outcomes"][0]["message"]
    );
    assert!(obj.exists());

    let chk2 = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk2.code, 0);
    assert!(chk2.json()["items"].as_array().unwrap().is_empty());
}

#[test]
fn t2_f5_junction_refusal_and_ro_hardlink_handling() {
    let td = TempDir::new("t2_f5_junc_hl");

    // 1. Root junction refusal
    {
        let data = td.join("data1");
        let proj = td.join("ProjRootJunc");
        let outside = td.join("OutsideSecret");
        fs::create_dir_all(&data).unwrap();
        write_uproject(&proj);
        write_file(&proj.join(r"Intermediate\a.obj"), b"orig");
        let secret = outside.join("secret.txt");
        write_file(&secret, b"do-not-delete");
        age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));
        age_tree_bottom_up(&outside, Duration::from_secs(3 * 3600));

        let p_str = proj.display().to_string();
        let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
        assert_eq!(chk.code, 0);
        let rep = chk.json();
        let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
        let id = find_item_by_key(&rep, "ue:intermediate")["id"]
            .as_str()
            .unwrap()
            .to_string();

        remove_tree_safe(&proj.join("Intermediate"));
        make_junction(&proj.join("Intermediate"), &outside);

        let fix_arg = format!("{}:now", id);
        let dry = run_mg(&data, &["disk", "clean", "--fix", &fix_arg, "--dry-run", "--json"]);
        assert_eq!(dry.code, 1);
        assert_eq!(dry.json()["outcomes"][0]["result"], "refused");
        assert!(secret.exists());

        let clean = run_mg(
            &data,
            &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
        );
        assert_eq!(clean.code, 1);
        assert!(clean.stderr.contains("checkup changed"));
        assert!(secret.exists());
    }

    // 2. Parent-chain Plugins\Foo junction refusal
    {
        let data = td.join("data2");
        let proj = td.join("ProjChainJunc");
        let outside_foo = td.join("OutsideFoo");
        fs::create_dir_all(&data).unwrap();
        write_uproject(&proj);
        let foo = proj.join(r"Plugins\Foo");
        write_uplugin(&foo, "Foo");
        write_file(&foo.join(r"Intermediate\p.obj"), b"plugin-obj");
        write_uplugin(&outside_foo, "Foo");
        let out_obj = outside_foo.join(r"Intermediate\p.obj");
        write_file(&out_obj, b"outside-obj");
        age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));
        age_tree_bottom_up(&outside_foo, Duration::from_secs(3 * 3600));

        let p_str = proj.display().to_string();
        let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
        assert_eq!(chk.code, 0);
        let rep = chk.json();
        let id = find_item_by_key(&rep, "ue:plugin_intermediate")["id"]
            .as_str()
            .unwrap()
            .to_string();

        remove_tree_safe(&foo);
        make_junction(&foo, &outside_foo);

        let fix_arg = format!("{}:now", id);
        let dry = run_mg(&data, &["disk", "clean", "--fix", &fix_arg, "--dry-run", "--json"]);
        assert_eq!(dry.code, 1);
        let dj = dry.json();
        assert_eq!(dj["outcomes"][0]["result"], "refused");
        assert!(
            dj["outcomes"][0]["message"]
                .as_str()
                .unwrap_or("")
                .contains("link (junction or symbolic link)"),
            "msg={}",
            dj["outcomes"][0]["message"]
        );
        assert!(out_obj.exists());
    }

    // 3. Internal junction removed without touching target
    {
        let data = td.join("data3");
        let proj = td.join("ProjInnerJunc");
        fs::create_dir_all(&data).unwrap();
        write_file(&data.join("settings.toml"), b"[disk]\nmin_age_hours = 0.0\n");
        write_uproject(&proj);
        let keep = proj.join(r"Content\keep.uasset");
        write_file(&keep, b"keep-uasset");
        write_file(&proj.join(r"Intermediate\plain.obj"), b"plain");
        make_junction(&proj.join(r"Intermediate\sublink"), &proj.join("Content"));
        age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

        let p_str = proj.display().to_string();
        let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
        assert_eq!(chk.code, 0);
        let rep = chk.json();
        let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
        let id = find_item_by_key(&rep, "ue:intermediate")["id"]
            .as_str()
            .unwrap()
            .to_string();

        let fix_arg = format!("{}:now", id);
        let clean = run_mg(
            &data,
            &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
        );
        assert_eq!(clean.code, 0, "stderr: {}", clean.stderr);
        let cj = clean.json();
        assert_eq!(cj["outcomes"][0]["result"], "done");
        assert!(cj["outcomes"][0]["removed"]["links"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(fs::read(&keep).unwrap(), b"keep-uasset");
    }

    // 4. Read-only hard-linked file skipped leaving external link readonly()
    {
        let data = td.join("data4");
        let proj = td.join("ProjHardLink");
        fs::create_dir_all(&data).unwrap();
        write_uproject(&proj);
        let ext = proj.join(r"Content\shared.uasset");
        write_file(&ext, b"precious-asset");
        let inter_link = proj.join(r"Intermediate\shared.uasset");
        fs::create_dir_all(proj.join("Intermediate")).unwrap();
        fs::hard_link(&ext, &inter_link).expect("hard_link");
        let mut perms = fs::metadata(&ext).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ext, perms).unwrap();

        let plain = proj.join(r"Intermediate\plain.obj");
        write_file(&plain, &vec![0x5Au8; 8 << 20]);
        age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

        let p_str = proj.display().to_string();
        let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
        assert_eq!(chk.code, 0);
        let rep = chk.json();
        let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
        let id = find_item_by_key(&rep, "ue:intermediate")["id"]
            .as_str()
            .unwrap()
            .to_string();

        let fix_arg = format!("{}:now", id);
        let clean = run_mg(
            &data,
            &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
        );
        assert_eq!(clean.code, 0, "stderr: {}", clean.stderr);
        let cj = clean.json();
        let o = &cj["outcomes"][0];
        assert_eq!(o["result"], "done");
        assert_eq!(o["removed"]["files"], 1);
        assert_eq!(o["removed"]["remains"], true);
        assert_eq!(o["removed"]["failed"], 0);
        assert!(
            o["message"]
                .as_str()
                .unwrap_or("")
                .contains("also linked from outside this folder"),
            "msg={}",
            o["message"]
        );
        assert!(!plain.exists());
        assert!(inter_link.exists());
        assert!(ext.exists());
        assert!(fs::metadata(&ext).unwrap().permissions().readonly());
    }
}

#[test]
fn t2_f5_bug9_ntfs_footprint_nonzero_and_bug8_concurrent_writes() {
    let td = TempDir::new("t2_f5_bug9_bug8");
    let data = td.join("data");
    let proj = td.join("FootprintProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    write_file(&proj.join(r"Intermediate\big.bin"), &vec![0xA5u8; 10 << 20]);
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let item = find_item_by_key(&rep, "ue:intermediate");
    assert!(item["logical_gb"].as_f64().unwrap_or(0.0) >= 0.01);
    assert!(item["allocated_gb"].as_f64().unwrap_or(0.0) >= 0.01);
    assert!(
        item["footprint_gb"].as_f64().unwrap_or(0.0) >= 0.01,
        "footprint_gb should be >= 0.01 on 10 MiB NTFS payload: {}",
        item
    );
    assert!(item["frees_alone_gb"].as_f64().unwrap_or(0.0) >= 0.01);

    let id = item["id"].as_str().unwrap().to_string();
    let mut children = Vec::new();
    for _ in 0..3 {
        let mut cmd = mg_cmd(&data, &["disk", "plan", &id, "--json"]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        children.push(ChildGuard::new(cmd.spawn().expect("spawn concurrent disk plan")));
    }
    for ch in children {
        let out = ch.wait_with_output().expect("wait plan child");
        assert!(
            out.status.success(),
            "concurrent plan failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pj: Value = serde_json::from_slice(&out.stdout).expect("valid plan JSON");
        assert!(pj["plan_id"].as_str().is_some_and(|s| s.starts_with('p')));
    }

    let plans_raw = fs::read_to_string(data.join("disk-plans.json")).expect("read disk-plans.json");
    let plans_json: Value = serde_json::from_str(&plans_raw).expect("valid disk-plans.json");
    assert!(plans_json.as_array().is_some_and(|a| !a.is_empty()));

    let leftover_tmp: Vec<_> = fs::read_dir(&data)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftover_tmp.is_empty(),
        "unexpected .tmp files left in data dir: {:?}",
        leftover_tmp
    );
}

#[test]
fn t2_f5_undo_refuses_when_rebuilt() {
    let _lock = bin_lock();
    let td = TempDir::new("t2_f5_undo_rebuilt");
    let data = td.join("data");
    let proj = td.join("RebuiltProj");
    fs::create_dir_all(&data).unwrap();
    let mut bin_guard = BinGuard::new(&data);

    write_uproject(&proj);
    let logs_dir = proj.join(r"Saved\Logs");
    let orig_log = logs_dir.join("a.log");
    write_file(&orig_log, &vec![b'R'; 4096]);
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:logs")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:bin", id);
    let clean = run_mg(
        &data,
        &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
    );
    assert_eq!(clean.code, 0, "stderr: {}", clean.stderr);
    let undo_id = clean.json()["outcomes"][0]["undo_id"]
        .as_str()
        .unwrap()
        .to_string();
    bin_guard.register(&undo_id);

    let rebuilt = logs_dir.join("new.log");
    write_file(&rebuilt, b"rebuilt-log");
    bin_guard.add_blocking_path(logs_dir.clone());

    let u1 = run_mg(&data, &["disk", "undo", &undo_id, "--json"]);
    assert_eq!(u1.code, 1);
    let u1j = u1.json();
    assert_eq!(u1j["outcomes"][0]["result"], "failed");
    assert!(
        u1j["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("stays in the bin"),
        "msg={}",
        u1j["outcomes"][0]["message"]
    );
    assert!(rebuilt.exists());

    remove_tree_safe(&logs_dir);
    bin_guard.clear_blocking_paths();

    let u2 = run_mg(&data, &["disk", "undo", &undo_id, "--json"]);
    assert_eq!(u2.code, 0, "stderr: {}", u2.stderr);
    assert_eq!(u2.json()["outcomes"][0]["result"], "undone");
    bin_guard.mark_undone(&undo_id);
    assert!(orig_log.exists());
}

// ============================================================================
// F6 Tier 1 (8 tests)
// ============================================================================

#[test]
fn t1_f6_relocate_plan_and_dynamic_apply_undo_lifecycle() {
    let td = TempDir::new("t1_f6_reloc_life");
    let data = td.join("data");
    let local_app = td.join("localapp");
    let user_prof = td.join("profile");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&user_prof).unwrap();
    write_file(&local_app.join(r"pip\cache\a.whl"), b"wheel-bytes");

    let env_guard = EnvKeyGuard::new("t1_f6_reloc");

    let plan_out = run_cmd(isolate_relocate_cmd(
        &data,
        env_guard.key(),
        &local_app,
        &user_prof,
        &["disk", "relocate", "--json"],
    ));
    assert_eq!(plan_out.code, 0, "stderr: {}", plan_out.stderr);
    let plan = plan_out.json();
    assert!(plan["store"]
        .as_str()
        .unwrap_or("")
        .starts_with(r"HKCU\Software\AtherMemGuard-test\"));
    assert!(plan["entries"].is_array());
    assert!(plan["notes"].is_array());

    if let Some(vol_root) = find_non_system_dest_root(&data) {
        let dest_td = TempDir::new_in(&vol_root, "reloc_dest");
        let dest_str = dest_td.path().display().to_string();

        let apply_out = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--to", &dest_str, "--apply", "--yes", "--json"],
        ));
        assert_eq!(apply_out.code, 0, "stderr: {}", apply_out.stderr);
        let apply_json = apply_out.json();
        assert_eq!(apply_json["applied"], true);
        assert!(!apply_json["entries"].as_array().unwrap().is_empty());

        let hkcu_key = format!(r"HKCU\{}", env_guard.key());
        let reg_q = Command::new("reg.exe")
            .args(["query", &hkcu_key, "/v", "PIP_CACHE_DIR"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("reg query");
        assert!(
            reg_q.status.success(),
            "expected PIP_CACHE_DIR in {}: {}",
            hkcu_key,
            String::from_utf8_lossy(&reg_q.stderr)
        );
        let reg_txt = String::from_utf8_lossy(&reg_q.stdout);
        assert!(
            norm_win(&reg_txt).contains(&norm_win(&dest_str)),
            "reg stdout: {}",
            reg_txt
        );

        let undo_out = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--undo", "--json"],
        ));
        assert_eq!(undo_out.code, 0, "stderr: {}", undo_out.stderr);
        let undo_json = undo_out.json();
        assert_eq!(undo_json["undone"], true);
        assert!(!undo_json["messages"].as_array().unwrap().is_empty());

        let reg_after = Command::new("reg.exe")
            .args(["query", &hkcu_key, "/v", "PIP_CACHE_DIR"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("reg query after undo");
        assert!(!reg_after.status.success(), "PIP_CACHE_DIR should be removed after undo");
    } else {
        let fallback_out = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--to", r"C:\amg_reloc_e2e_fallback", "--json"],
        ));
        assert_eq!(fallback_out.code, 0);
        assert!(!fallback_out.json()["refused"].is_null());
    }
}

#[test]
fn t1_f6_git_untracked_intermediate_cleanable() {
    let td = TempDir::new("t1_f6_git_untracked");
    let data = td.join("data");
    let repo = td.join("Repo");
    fs::create_dir_all(&data).unwrap();
    init_git_repo(&repo);
    write_uproject(&repo);
    write_file(&repo.join(".gitignore"), b"Intermediate/\n");
    run_git(&repo, &["add", "Game.uproject", ".gitignore"]);
    run_git(&repo, &["commit", "-m", "init"]);

    let temp_obj = repo.join(r"Intermediate\Build\temp.obj");
    write_file(&temp_obj, b"untracked-build-obj");
    write_file(&data.join("settings.toml"), b"[disk]\nmin_age_hours = 0.0\n");

    let r_str = repo.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &r_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:now", id);
    let clean = run_mg(
        &data,
        &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
    );
    assert_eq!(clean.code, 0, "stderr: {}", clean.stderr);
    assert_eq!(clean.json()["outcomes"][0]["result"], "done");
    assert!(!temp_obj.exists());
    assert!(repo.join(".git").exists());
    assert!(repo.join("Game.uproject").exists());
}

#[test]
fn t1_f6_git_abs_worktree_lane_discovery() {
    let td = TempDir::new("t1_f6_abs_wt");
    let data = td.join("data");
    let parent = td.join("ws");
    let main_repo = parent.join("MainRepo");
    fs::create_dir_all(&data).unwrap();
    init_git_repo(&main_repo);
    write_uproject(&main_repo);
    run_git(&main_repo, &["add", "Game.uproject"]);
    run_git(&main_repo, &["commit", "-m", "init"]);
    run_git(&main_repo, &["worktree", "add", "../LaneAbs", "-b", "lane-abs"]);

    let p_str = parent.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let lane = rep["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["key"] == "lane" && item_has_path_ci(it, "LaneAbs"))
        .unwrap_or_else(|| panic!("expected LaneAbs lane item in {}", rep));
    assert_eq!(lane["class"], "review");
    assert!(!lane["no_action"].is_null());
}

#[test]
fn t1_f6_git_relative_worktree_discovery_m2() {
    let td = TempDir::new("t1_f6_rel_wt");
    let data = td.join("data");
    let ws = td.join("ws");
    let main_repo = ws.join("MainRepo");
    let elsewhere = td.join("elsewhere");
    let lane_dir = elsewhere.join("LaneRel");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    init_git_repo(&main_repo);
    write_uproject(&main_repo);
    run_git(&main_repo, &["add", "Game.uproject"]);
    run_git(&main_repo, &["commit", "-m", "init"]);
    // Place LaneRel in a non-sibling directory (`elsewhere/LaneRel` vs `ws/MainRepo`) so
    // baseline sibling `.uproject` scanning cannot mask unimplemented relative `gitdir:` resolution.
    run_git(
        &main_repo,
        &[
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            "../../elsewhere/LaneRel",
            "-b",
            "lane-rel",
        ],
    );
    write_file(&main_repo.join(r"Intermediate\m.obj"), &vec![0x11u8; 4096]);
    age_tree_bottom_up(&main_repo.join("Intermediate"), Duration::from_secs(3 * 3600));

    let gitdir_file = main_repo.join(r".git\worktrees\LaneRel\gitdir");
    let gitdir_txt = fs::read_to_string(&gitdir_file).expect("read relative gitdir");
    assert!(
        gitdir_txt.contains(".."),
        "expected relative gitdir content, got: {:?}",
        gitdir_txt
    );

    // Forward discovery from MainRepo -> finds non-sibling relative worktree LaneRel
    let main_str = main_repo.display().to_string();
    let chk_fwd = run_mg(&data, &["disk", "checkup", "--path", &main_str, "--refresh", "--json"]);
    assert_eq!(chk_fwd.code, 0, "stderr: {}", chk_fwd.stderr);
    let rep_fwd = chk_fwd.json();
    let lane_fwd = rep_fwd["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["key"] == "lane" && item_has_path_ci(it, "LaneRel"))
        .unwrap_or_else(|| panic!("expected relative worktree LaneRel in {}", rep_fwd));
    assert_eq!(lane_fwd["class"], "review");

    // Reverse discovery from LaneRel -> discovers lane AND links/discovers MainRepo (M2 F4.1d)
    let lane_str = lane_dir.display().to_string();
    let chk_rev = run_mg(&data, &["disk", "checkup", "--path", &lane_str, "--refresh", "--json"]);
    assert_eq!(chk_rev.code, 0, "stderr: {}", chk_rev.stderr);
    let rep_rev = chk_rev.json();
    let rev_items = rep_rev["items"].as_array().unwrap();
    assert!(
        rev_items
            .iter()
            .any(|it| it["key"] == "lane" && item_has_path_ci(it, "LaneRel")),
        "expected lane in reverse worktree checkup: {}",
        rep_rev
    );
    assert!(
        rev_items
            .iter()
            .any(|it| it.to_string().to_ascii_lowercase().contains("mainrepo")),
        "expected reverse worktree checkup from LaneRel to reference/discover MainRepo (M2 F4.1d): {}",
        rep_rev
    );
}

#[test]
fn t1_f6_non_ue_git_repo_and_lfs_object_db_discovery() {
    let td = TempDir::new("t1_f6_non_ue_git");
    let data = td.join("data");
    let parent = td.join("repos");
    let repo = parent.join("non_ue_app");
    fs::create_dir_all(&data).unwrap();
    init_git_repo(&repo);
    write_file(&repo.join("Cargo.toml"), b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
    write_file(&repo.join(r"src\main.rs"), b"fn main() {}\n");
    run_git(&repo, &["add", "Cargo.toml", "src/main.rs"]);
    run_git(&repo, &["commit", "-m", "init"]);

    write_file(&repo.join(r".git\lfs\objects\aa\bb\0123456789abcdef"), &vec![0x42u8; 4096]);
    let stale_pack = repo.join(r".git\objects\pack\tmp_pack_stale123");
    write_file(&stale_pack, &vec![0x24u8; 4096]);
    set_mtime_win(&stale_pack, SystemTime::now() - Duration::from_secs(48 * 3600));

    let p_str = parent.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let items = rep["items"].as_array().expect("items array");

    let lfs = items
        .iter()
        .find(|it| it["key"] == "git:lfs")
        .unwrap_or_else(|| panic!("expected git:lfs in non-UE repo checkup: {}", rep));
    assert!(
        lfs["actions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|act| act["kind"] == "tool")),
        "expected actionable tool option on git:lfs (M2 F4.1b): {}",
        lfs
    );
    assert!(
        items.iter().any(|it| {
            it["key"]
                .as_str()
                .is_some_and(|k| k.starts_with("git:") && k != "git:lfs")
        }),
        "expected git object DB / maintenance item (M2 F4.1c) in {}",
        rep
    );
}

#[test]
fn t1_f6_expanded_project_catalog_plugins_ddc_and_archived_builds() {
    let td = TempDir::new("t1_f6_proj_catalog");
    let data = td.join("data");
    let proj = td.join("ExpandedUE");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    let plugin_dir = proj.join(r"Plugins\MyPlugin");
    write_uplugin(&plugin_dir, "MyPlugin");
    write_file(&plugin_dir.join(r"DerivedDataCache\a.bin"), &vec![1u8; 4096]);
    write_file(&proj.join(r"Saved\ArchivedBuilds\Win\b.bin"), &vec![2u8; 4096]);
    age_tree_bottom_up(&proj, Duration::from_secs(3 * 3600));

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let items = rep["items"].as_array().expect("items array");

    assert!(
        items
            .iter()
            .any(|it| item_has_path_ci(it, r"Plugins\MyPlugin\DerivedDataCache")),
        "expected Plugins\\MyPlugin\\DerivedDataCache in {}",
        rep
    );
    assert!(
        items
            .iter()
            .any(|it| item_has_path_ci(it, r"Saved\ArchivedBuilds")),
        "expected Saved\\ArchivedBuilds in {}",
        rep
    );
}

#[test]
fn t1_f6_expanded_global_catalog_cargo_pnpm_uv_env_and_loose_temp() {
    let _lock = bin_lock();
    let td = TempDir::new("t1_f6_global_cat");
    let data = td.join("data");
    fs::create_dir_all(&data).unwrap();

    // Query real dev_drive == true roots and all fixed-drive .AtherMemGuard-bin dirs, and add them
    // to `[disk] keep` so global checkup never scans real Dev Drives or concurrent bin folders.
    let disk_out = run_mg(&data, &["disk", "--json"]);
    assert_eq!(disk_out.code, 0);
    let disk_json = disk_out.json();
    let mut keep_roots: Vec<String> = Vec::new();
    let mut push_keep = |raw: &str| {
        let entry = format!("'{}'", raw);
        if !keep_roots.iter().any(|k| k.eq_ignore_ascii_case(&entry)) {
            keep_roots.push(entry);
        }
    };
    push_keep(r"C:\.AtherMemGuard-bin");
    if let Some(vols) = disk_json["volumes"].as_array() {
        for v in vols {
            let is_dev = v["dev_drive"].as_bool() == Some(true);
            let is_fixed_like = v["backing"].as_str() != Some("network");
            if let Some(paths) = v["paths"].as_array() {
                for p in paths {
                    if let Some(ps) = p.as_str() {
                        if is_dev {
                            push_keep(ps);
                        }
                        if is_fixed_like && ps.len() <= 3 && ps.as_bytes().get(1) == Some(&b':') {
                            let bin_dir = format!(r"{}\.AtherMemGuard-bin", ps.trim_end_matches('\\'));
                            push_keep(&bin_dir);
                        }
                    }
                }
            }
        }
    }
    let settings = format!(
        "[disk]\nscan_depth = 1\nscan_budget_sec = 5\nkeep = [{}]\n",
        keep_roots.join(", ")
    );
    write_file(&data.join("settings.toml"), settings.as_bytes());

    let user_prof = td.join("prof");
    let local_app = td.join("localapp");
    let app_data = td.join("appdata");
    let prog_data = td.join("programdata");
    let temp_dir = td.join("tmp");
    let cargo_home = td.join("cargo_home");
    let pnpm_store = td.join("pnpm_store");
    let custom_uv = td.join("custom_uv");
    let npm_cache = td.join("npm_cache");
    let pip_cache = td.join("pip_cache");
    let nuget_pkg = td.join("nuget_pkg");
    let vcpkg_cache = td.join("vcpkg_cache");
    let yarn_cache = td.join("yarn_cache");
    let bun_cache = td.join("bun_cache");
    let gradle_home = td.join("gradle_home");
    let rustup_home = td.join("rustup_home");
    let sccache_dir = td.join("sccache_dir");

    for d in [
        &user_prof,
        &local_app,
        &app_data,
        &prog_data,
        &temp_dir,
        &cargo_home,
        &pnpm_store,
        &custom_uv,
        &npm_cache,
        &pip_cache,
        &nuget_pkg,
        &vcpkg_cache,
        &yarn_cache,
        &bun_cache,
        &gradle_home,
        &rustup_home,
        &sccache_dir,
    ] {
        fs::create_dir_all(d).unwrap();
    }

    write_file(&cargo_home.join(r"registry\cache\idx\demo.crate"), &vec![1u8; 4096]);
    write_file(&pnpm_store.join(r"v3\files\00\aa"), &vec![2u8; 4096]);
    write_file(&custom_uv.join(r"archive-v0\wheel.whl"), &vec![3u8; 4096]);

    let loose_tmp = temp_dir.join("loose_old.tmp");
    write_file(&loose_tmp, &vec![4u8; 4096]);
    set_mtime_win(&loose_tmp, SystemTime::now() - Duration::from_secs(10 * 86400));

    let mut cmd = mg_cmd(&data, &["disk", "checkup", "--refresh", "--json"]);
    cmd.env("USERPROFILE", &user_prof);
    cmd.env("HOME", &user_prof);
    cmd.env("LOCALAPPDATA", &local_app);
    cmd.env("APPDATA", &app_data);
    cmd.env("ProgramData", &prog_data);
    cmd.env("PROGRAMDATA", &prog_data);
    cmd.env("TMP", &temp_dir);
    cmd.env("TEMP", &temp_dir);
    cmd.env("CARGO_HOME", &cargo_home);
    cmd.env("PNPM_STORE_DIR", &pnpm_store);
    cmd.env("UV_CACHE_DIR", &custom_uv);
    cmd.env("npm_config_cache", &npm_cache);
    cmd.env("PIP_CACHE_DIR", &pip_cache);
    cmd.env("NUGET_PACKAGES", &nuget_pkg);
    cmd.env("VCPKG_DEFAULT_BINARY_CACHE", &vcpkg_cache);
    cmd.env("YARN_CACHE_FOLDER", &yarn_cache);
    cmd.env("BUN_INSTALL_CACHE_DIR", &bun_cache);
    cmd.env("GRADLE_USER_HOME", &gradle_home);
    cmd.env("RUSTUP_HOME", &rustup_home);
    cmd.env("SCCACHE_DIR", &sccache_dir);
    cmd.env_remove("UE-LocalDataCachePath");
    cmd.env_remove("UE-ZenSubprocessDataPath");
    cmd.env_remove("DDC");
    cmd.env_remove("ZEN");

    let chk = run_cmd(cmd);
    assert_eq!(chk.code, 0, "stderr: {}", chk.stderr);
    let rep = chk.json();
    let items = rep["items"].as_array().expect("items array");

    let cargo_s = cargo_home.display().to_string();
    let pnpm_s = pnpm_store.display().to_string();
    assert!(
        items.iter().any(|it| item_has_path_ci(it, &cargo_s)),
        "expected Cargo cache under {} in {}",
        cargo_s,
        rep
    );
    assert!(
        items.iter().any(|it| item_has_path_ci(it, &pnpm_s)),
        "expected pnpm store under {} in {}",
        pnpm_s,
        rep
    );
    assert!(
        items
            .iter()
            .any(|it| it["key"] == "cache:uv" && item_has_path_ci(it, "custom_uv")),
        "expected relocated UV_CACHE_DIR item in {}",
        rep
    );
    assert!(
        items
            .iter()
            .any(|it| it["key"] == "temp:old" && item_has_path_ci(it, "loose_old.tmp")),
        "expected loose TEMP file temp:old in {}",
        rep
    );
}

#[test]
fn t1_f6_disk_report_refs_vhdx_and_trend_fields() {
    // Unconditionally verified across 2 consecutive opaque-box invocations:
    //   - `Volume` schema keys (`backing`, `backed_by`, `trend_gb_per_day`, `block_cloning`,
    //     `free_basis == "available_to_caller"`).
    //   - `dev_drive` object fields (`enabled`, `av_filters`, `defender_performance_mode`) and
    //     `findings` array item structure (`id`, `severity`, `title`, `detail`).
    //   - ReFS volumes present on the host (`block_cloning == true` and non-empty `notes`).
    //   - Consistent volume IDs across consecutive calls.
    // Conditional on host / multi-hour history (cannot be forced in a 1s opaque-box cold run
    // without touching internal disk-history files):
    //   - `trend_gb_per_day` numeric finiteness (when non-null; `null` on a cold 1s run).
    //   - `backed_by` non-empty string (when a volume with `backing == "vhd"` is mounted).
    let td = TempDir::new("t1_f6_refs_vhdx_trend");
    let data = td.join("data");
    fs::create_dir_all(&data).unwrap();

    let mut prev_vol_ids: Option<Vec<String>> = None;
    for call_idx in 0..2 {
        let out = run_mg(&data, &["disk", "--json"]);
        assert_eq!(out.code, 0, "call {} stderr: {}", call_idx, out.stderr);
        let rep = out.json();

        let dev_drive = &rep["dev_drive"];
        assert!(dev_drive.is_object(), "expected dev_drive object: {}", rep);
        assert!(
            dev_drive
                .get("enabled")
                .is_some_and(|e| e.is_boolean() || e.is_null()),
            "expected dev_drive.enabled bool|null: {}",
            dev_drive
        );
        assert!(
            dev_drive["av_filters"].as_str().is_some_and(|s| !s.is_empty()),
            "expected dev_drive.av_filters non-empty string: {}",
            dev_drive
        );
        assert!(
            dev_drive["defender_performance_mode"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "expected dev_drive.defender_performance_mode non-empty string: {}",
            dev_drive
        );

        let findings = rep["findings"].as_array().expect("findings array");
        for f in findings {
            assert!(f.is_object(), "finding should be object: {}", f);
            assert!(
                f["id"].as_str().is_some_and(|s| !s.is_empty()),
                "finding id missing: {}",
                f
            );
            assert!(f["severity"].is_string(), "finding severity missing: {}", f);
            assert!(f["title"].is_string(), "finding title missing: {}", f);
            assert!(f["detail"].is_string(), "finding detail missing: {}", f);
        }

        let vols = rep["volumes"].as_array().expect("volumes");
        assert!(!vols.is_empty());
        let mut cur_vol_ids: Vec<String> = Vec::with_capacity(vols.len());

        for v in vols {
            let vid = v["volume_id"].as_str().expect("volume_id").to_string();
            cur_vol_ids.push(vid);
            assert!(v.get("backing").is_some());
            assert!(v.get("backed_by").is_some());
            assert!(v.get("trend_gb_per_day").is_some());
            if let Some(t) = v["trend_gb_per_day"].as_f64() {
                assert!(t.is_finite(), "non-finite trend_gb_per_day in {}", v);
            } else {
                assert!(v["trend_gb_per_day"].is_null());
            }
            assert_eq!(v["free_basis"], "available_to_caller");

            if v["fs"]
                .as_str()
                .is_some_and(|fs| fs.eq_ignore_ascii_case("ReFS"))
            {
                assert_eq!(v["block_cloning"], true, "ReFS should have block_cloning=true: {}", v);
                assert!(
                    !v["notes"].as_array().unwrap().is_empty(),
                    "ReFS should include delayed-reclaim note: {}",
                    v
                );
            }

            if v["backing"] == "vhd" {
                assert!(
                    v["backed_by"].as_str().is_some_and(|s| !s.is_empty()),
                    "VHD volume should report backed_by (M2 F4.2a): {}",
                    v
                );
            }
        }

        if let Some(ref expected) = prev_vol_ids {
            assert_eq!(&cur_vol_ids, expected, "volumes should stay consistent across calls");
        }
        prev_vol_ids = Some(cur_vol_ids);
    }
}

// ============================================================================
// F6 Tier 2 (6 tests)
// ============================================================================

#[test]
fn t2_f6_relocate_bad_to_paths_refused() {
    let td = TempDir::new("t2_f6_bad_to");
    let data = td.join("data");
    let local_app = td.join("localapp");
    let user_prof = td.join("profile");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&local_app).unwrap();
    fs::create_dir_all(&user_prof).unwrap();

    let env_guard = EnvKeyGuard::new("t2_f6_bad_to");

    for bad in [
        r"\\server\share\caches",
        r"\\?\C:\caches",
        "Caches",
        r"..\caches",
        r"C:\a\..\b",
        r"Q:\amg_nonexistent_drive_e2e\caches",
    ] {
        let out = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--to", bad, "--json"],
        ));
        assert_eq!(
            out.code, 1,
            "expected bad --to '{}' to fail (stdout={} stderr={})",
            bad, out.stdout, out.stderr
        );
    }

    let sys_apply = run_cmd(isolate_relocate_cmd(
        &data,
        env_guard.key(),
        &local_app,
        &user_prof,
        &["disk", "relocate", "--to", r"C:\BadCaches", "--apply", "--yes", "--json"],
    ));
    assert_eq!(sys_apply.code, 1);
}

#[test]
fn t2_f6_relocate_invalid_env_key_refused() {
    let td = TempDir::new("t2_f6_bad_envkey");
    let data = td.join("data");
    let local_app = td.join("localapp");
    let user_prof = td.join("profile");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&local_app).unwrap();
    fs::create_dir_all(&user_prof).unwrap();

    for bad_key in [
        "Environment",
        r"Software\Other\TestKey",
        r"Software\AtherMemGuard-test\..\Environment",
    ] {
        let out = run_cmd(isolate_relocate_cmd(
            &data,
            bad_key,
            &local_app,
            &user_prof,
            &["disk", "relocate", "--json"],
        ));
        assert_eq!(out.code, 1, "expected invalid key '{}' to fail", bad_key);
        assert!(
            out.stderr.contains(r"Software\AtherMemGuard-test\"),
            "stderr={}",
            out.stderr
        );
    }
}

#[test]
fn t2_f6_relocate_double_apply_and_double_undo_refused() {
    let td = TempDir::new("t2_f6_dbl_reloc");
    let data = td.join("data");
    let local_app = td.join("localapp");
    let user_prof = td.join("profile");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&user_prof).unwrap();
    write_file(&local_app.join(r"pip\cache\a.whl"), b"wheel");

    let env_guard = EnvKeyGuard::new("t2_f6_dbl");

    let u0 = run_cmd(isolate_relocate_cmd(
        &data,
        env_guard.key(),
        &local_app,
        &user_prof,
        &["disk", "relocate", "--undo", "--json"],
    ));
    assert_eq!(u0.code, 1);
    assert!(u0.stderr.contains("Nothing to undo"), "stderr={}", u0.stderr);

    if let Some(vol_root) = find_non_system_dest_root(&data) {
        let dest_td = TempDir::new_in(&vol_root, "reloc_dbl");
        let dest_str = dest_td.path().display().to_string();

        let a1 = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--to", &dest_str, "--apply", "--yes", "--json"],
        ));
        assert_eq!(a1.code, 0, "stderr={}", a1.stderr);

        // Create another cache on C: so plan() doesn't bail with "no cache this covers is on
        // the system drive", allowing apply() to enforce its "already moved" guard.
        write_file(&local_app.join(r"npm-cache\a.tgz"), b"npm-bytes");

        let a2 = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--to", &dest_str, "--apply", "--yes", "--json"],
        ));
        assert_eq!(a2.code, 1);
        assert!(a2.stderr.contains("already moved"), "stderr={}", a2.stderr);

        let u1 = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--undo", "--json"],
        ));
        assert_eq!(u1.code, 0, "stderr={}", u1.stderr);

        let u2 = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--undo", "--json"],
        ));
        assert_eq!(u2.code, 1);
        assert!(u2.stderr.contains("Nothing to undo"), "stderr={}", u2.stderr);
    } else {
        let fallback_out = run_cmd(isolate_relocate_cmd(
            &data,
            env_guard.key(),
            &local_app,
            &user_prof,
            &["disk", "relocate", "--to", r"C:\amg_reloc_e2e_fallback", "--json"],
        ));
        assert_eq!(fallback_out.code, 0, "stderr={}", fallback_out.stderr);
        assert!(!fallback_out.json()["refused"].is_null());
    }
}

#[test]
fn t2_f6_git_tracked_files_in_intermediate_refused() {
    let td = TempDir::new("t2_f6_git_tracked");
    let data = td.join("data");
    let repo = td.join("TrackedRepo");
    fs::create_dir_all(&data).unwrap();
    init_git_repo(&repo);
    write_uproject(&repo);
    let tracked = repo.join(r"Intermediate\Build\tracked.txt");
    write_file(&tracked, b"tracked-content");
    run_git(&repo, &["add", "Game.uproject", "Intermediate/Build/tracked.txt"]);
    run_git(&repo, &["commit", "-m", "track intermediate"]);
    write_file(&data.join("settings.toml"), b"[disk]\nmin_age_hours = 0.0\n");

    let r_str = repo.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &r_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:now", id);
    let clean = run_mg(
        &data,
        &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
    );
    assert_eq!(clean.code, 1);
    let cj = clean.json();
    assert_eq!(cj["outcomes"][0]["result"], "refused");
    assert!(
        cj["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Git tracks"),
        "msg={}",
        cj["outcomes"][0]["message"]
    );
    assert!(tracked.exists());
}

#[test]
fn t2_f6_nested_git_repo_in_intermediate_refused() {
    let td = TempDir::new("t2_f6_nested_git");
    let data = td.join("data");
    let proj = td.join("NestedGitProj");
    fs::create_dir_all(&data).unwrap();
    write_uproject(&proj);
    let nested = proj.join(r"Intermediate\NestedRepo");
    init_git_repo(&nested);
    write_file(&nested.join("README.md"), b"nested");
    write_file(&data.join("settings.toml"), b"[disk]\nmin_age_hours = 0.0\n");

    let p_str = proj.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let id = find_item_by_key(&rep, "ue:intermediate")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let fix_arg = format!("{}:now", id);
    let clean = run_mg(
        &data,
        &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
    );
    assert_eq!(clean.code, 1);
    let cj = clean.json();
    assert_eq!(cj["outcomes"][0]["result"], "refused");
    assert!(
        cj["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("git repository"),
        "msg={}",
        cj["outcomes"][0]["message"]
    );
    assert!(nested.join(".git").exists());
}

#[test]
fn t2_f6_git_worktree_lane_never_cleaned_by_disk_clean() {
    let td = TempDir::new("t2_f6_lane_never");
    let data = td.join("data");
    let parent = td.join("ws");
    let main_repo = parent.join("MainRepo");
    fs::create_dir_all(&data).unwrap();
    init_git_repo(&main_repo);
    write_uproject(&main_repo);
    run_git(&main_repo, &["add", "Game.uproject"]);
    run_git(&main_repo, &["commit", "-m", "init"]);
    run_git(&main_repo, &["worktree", "add", "../LaneNever", "-b", "lane-never"]);

    let p_str = parent.display().to_string();
    let chk = run_mg(&data, &["disk", "checkup", "--path", &p_str, "--refresh", "--json"]);
    assert_eq!(chk.code, 0);
    let rep = chk.json();
    let cid = rep["scan"]["checkup_id"].as_str().unwrap().to_string();
    let lane_id = rep["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["key"] == "lane" && item_has_path_ci(it, "LaneNever"))
        .expect("lane item")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let clean_default = run_mg(
        &data,
        &["disk", "clean", "--fix", &lane_id, "--yes", "--checkup", &cid, "--json"],
    );
    assert_eq!(clean_default.code, 1);
    let cdj = clean_default.json();
    assert_eq!(cdj["outcomes"][0]["result"], "refused");
    assert!(
        cdj["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("lanes or worktrees"),
        "msg={}",
        cdj["outcomes"][0]["message"]
    );

    let fix_arg = format!("{}:now", lane_id);
    let clean = run_mg(
        &data,
        &["disk", "clean", "--fix", &fix_arg, "--yes", "--checkup", &cid, "--json"],
    );
    assert_eq!(clean.code, 1);
    let cj = clean.json();
    assert_eq!(cj["outcomes"][0]["result"], "refused");
    assert!(
        cj["outcomes"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("isn't offered"),
        "msg={}",
        cj["outcomes"][0]["message"]
    );
    assert!(parent.join("LaneNever").exists());
}

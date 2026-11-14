//! Updates in place, like AtherScreenshot: a small manifest (`latest.json` on the newest GitHub
//! Release, written by `package.ps1`) names the newest Windows version, its exe, SHA-256 and
//! size. The app checks it, downloads the exe next to itself as `<exe>.update`, verifies it,
//! swaps it for the running exe (which Windows lets us rename, not overwrite) and starts the
//! new one with `--after-update <pid>`. No admin rights: the exe lives in a folder the user owns.
//!
//! No UI here. The tray shows toasts and menu items and calls these functions from a worker
//! thread; errors come back as sentences it can show as they are.

use std::cmp::Ordering;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{net, paths};

pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/AskTinNguyen/AtherMemGuard/releases/latest/download/latest.json";

const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_EXE: u64 = 64 * 1024 * 1024;
const MAX_NOTES: usize = 300;
const UPDATE_HOSTS: [&str; 2] = ["github.com", "objects.githubusercontent.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    Available(Manifest),
    Failed(String),
}

fn test_override() -> Option<String> {
    std::env::var(paths::UPDATE_URL_ENV).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `[updates] manifest_url` read straight from settings.toml, so this works before and after
/// the settings code knows the key. Missing file, table or key = empty.
fn settings_manifest_url() -> String {
    fs::read_to_string(paths::config_file()).map(|text| manifest_url_from_toml(&text)).unwrap_or_default()
}

fn manifest_url_from_toml(text: &str) -> String {
    text.parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("updates")?.get("manifest_url")?.as_str().map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

/// Where to look for updates: `ATHER_MEMGUARD_UPDATE_URL` if set (tests), else
/// `[updates] manifest_url` from settings if set, else the GitHub release.
pub fn manifest_url() -> String {
    if let Some(u) = test_override() {
        return u;
    }
    let s = settings_manifest_url();
    if s.is_empty() {
        DEFAULT_MANIFEST_URL.to_string()
    } else {
        s
    }
}

/// Where the manifest may come from: HTTPS anywhere (the settings may point at a team server),
/// plain http only to this PC and only with the test override.
fn manifest_url_allowed(url: &str, allow_localhost: bool) -> bool {
    match net::parse_url(url) {
        Some(u) => u.https || (allow_localhost && net::is_localhost(&u.host)),
        None => false,
    }
}

/// Where the exe may come from: HTTPS on GitHub, or this PC with the test override.
fn download_url_allowed(url: &str, allow_localhost: bool) -> bool {
    match net::parse_url(url) {
        Some(u) if allow_localhost && net::is_localhost(&u.host) => true,
        Some(u) => u.https && UPDATE_HOSTS.iter().any(|h| u.host.eq_ignore_ascii_case(h)),
        None => false,
    }
}

/// Compare dotted version numbers: "0.0.10" > "0.0.9", "1.0" == "1.0.0". Anything that isn't
/// a digit separates numbers, so a stray suffix doesn't break the comparison.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let nums = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|part| {
                part.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .fold(0u64, |n, c| n.saturating_mul(10).saturating_add(c as u64 - '0' as u64))
            })
            .collect()
    };
    let (x, y) = (nums(a), nums(b));
    for i in 0..x.len().max(y.len()) {
        let (p, q) = (x.get(i).copied().unwrap_or(0), y.get(i).copied().unwrap_or(0));
        match p.cmp(&q) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// The "windows" entry of a manifest, validated. Plain http to this PC is accepted only while
/// `ATHER_MEMGUARD_UPDATE_URL` is set.
pub fn parse_manifest(json: &str) -> Result<Manifest, String> {
    parse_manifest_with(json, test_override().is_some())
}

fn parse_manifest_with(json: &str, allow_localhost: bool) -> Result<Manifest, String> {
    const UNREADABLE: &str = "The update information couldn't be read";
    if json.len() as u64 > MAX_MANIFEST {
        return Err(format!("{}: it's larger than expected.", UNREADABLE));
    }
    let json = json.strip_prefix('\u{feff}').unwrap_or(json); // a BOM from a Windows editor
    let root: serde_json::Value = serde_json::from_str(json).map_err(|_| format!("{}.", UNREADABLE))?;
    let w = root.get("windows").filter(|w| w.is_object()).ok_or_else(|| format!("{}: there's no Windows version in it.", UNREADABLE))?;
    let text = |key: &str| w.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

    let version = text("version");
    let version_ok = !version.is_empty()
        && version.len() <= 20
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version.chars().all(|c| c.is_ascii_digit() || c == '.');
    if !version_ok {
        return Err(format!("{}: the version number isn't valid.", UNREADABLE));
    }
    let sha256 = text("sha256").to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{}: the checksum isn't valid.", UNREADABLE));
    }
    let size = w.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    if size == 0 || size > MAX_EXE {
        return Err(format!("{}: the file size isn't valid.", UNREADABLE));
    }
    let url = text("url");
    if !download_url_allowed(&url, allow_localhost) {
        return Err(format!("{}: the download isn't on GitHub over HTTPS.", UNREADABLE));
    }
    let notes: String = text("notes").chars().take(MAX_NOTES).collect();
    Ok(Manifest { version, url, sha256, size, notes })
}

/// Look for a newer version. Blocking: call it from a worker thread.
pub fn check() -> CheckResult {
    check_against(env!("CARGO_PKG_VERSION"))
}

fn check_against(current: &str) -> CheckResult {
    let url = manifest_url();
    let allow_localhost = test_override().is_some();
    if !manifest_url_allowed(&url, allow_localhost) {
        return CheckResult::Failed(format!("The update address {} has to start with https://.", url));
    }
    let body = match net::get_bytes(&url, MAX_MANIFEST) {
        Ok(b) => b,
        Err(e) => return CheckResult::Failed(e),
    };
    let text = match String::from_utf8(body) {
        Ok(t) => t,
        Err(_) => return CheckResult::Failed("The update information couldn't be read.".into()),
    };
    match parse_manifest_with(&text, allow_localhost) {
        Ok(m) if compare_versions(&m.version, current) == Ordering::Greater => CheckResult::Available(m),
        Ok(_) => CheckResult::UpToDate,
        Err(e) => CheckResult::Failed(e),
    }
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn self_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("Can't find the running app ({}).", e))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// SHA-256 of a file, lowercase hex.
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path).map_err(|_| "The download is missing.".to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|_| "The download couldn't be read.".to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// True when the file is exactly what the manifest describes: size and SHA-256.
fn verify_file(path: &Path, m: &Manifest) -> Result<(), String> {
    let len = fs::metadata(path).map_err(|_| "The download is missing.".to_string())?.len();
    if len != m.size {
        return Err("The download is incomplete, so it wasn't installed.".into());
    }
    if sha256_file(path)? != m.sha256.to_ascii_lowercase() {
        return Err("The download doesn't match its checksum, so it wasn't installed.".into());
    }
    Ok(())
}

/// Download the update to `<exe>.update` next to the running exe and verify its size and
/// SHA-256. `progress(got, total)` runs as it arrives. On any failure nothing is left behind.
/// Blocking: call it from a worker thread.
pub fn download(m: &Manifest, progress: &dyn Fn(u64, u64)) -> Result<PathBuf, String> {
    download_to(m, &with_suffix(&self_exe()?, ".update"), progress)
}

fn download_to(m: &Manifest, staged: &Path, progress: &dyn Fn(u64, u64)) -> Result<PathBuf, String> {
    if !download_url_allowed(&m.url, test_override().is_some()) {
        return Err("The download isn't on GitHub over HTTPS, so it wasn't downloaded.".into());
    }
    if m.size == 0 || m.size > MAX_EXE {
        return Err("The update's file size isn't valid.".into());
    }
    let folder = staged.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let mut file = fs::File::create(staged).map_err(|_| format!("Can't write to {}.", folder))?;
    let mut write_failed = false;
    let result = net::get(
        &m.url,
        m.size,
        &mut |chunk| {
            let ok = file.write_all(chunk).is_ok();
            write_failed |= !ok;
            ok
        },
        &mut |got, _| progress(got, m.size),
    );
    let flushed = file.flush().is_ok();
    drop(file);
    let result = match result {
        Err(_) if write_failed => Err(format!("Can't write to {}.", folder)),
        Err(e) => Err(e),
        Ok(()) if !flushed => Err(format!("Can't write to {}.", folder)),
        Ok(()) => verify_file(staged, m),
    };
    match result {
        Ok(()) => Ok(staged.to_path_buf()),
        Err(e) => {
            let _ = fs::remove_file(staged);
            Err(e)
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Put `staged` in place of `target`, keeping the old one as `<target>.old`. A running exe can
/// be renamed, just not overwritten. On failure everything is put back.
fn swap_exe(target: &Path, staged: &Path) -> Result<(), String> {
    if !staged.is_file() {
        return Err("The download is missing.".into());
    }
    let old = with_suffix(target, ".old");
    let _ = fs::remove_file(&old);
    fs::rename(target, &old).map_err(|e| format!("Can't replace {} ({}).", file_name(target), e))?;
    if let Err(e) = fs::rename(staged, target) {
        let _ = fs::rename(&old, target);
        return Err(format!("Can't put the new version in place ({}).", e));
    }
    Ok(())
}

/// Undo `swap_exe`: the new exe goes back to `staged`, the old one back to `target`.
fn unswap_exe(target: &Path, staged: &Path) {
    let _ = fs::rename(target, staged);
    let _ = fs::rename(with_suffix(target, ".old"), target);
    let _ = fs::remove_file(staged);
}

/// Swap the verified download in for the running exe and start it with
/// `--after-update <this pid>`; it waits for this process to exit. The caller then quits.
pub fn install_staged(staged: &Path) -> Result<(), String> {
    let target = self_exe()?;
    swap_exe(&target, staged)?;
    let params = format!("--after-update {}", std::process::id());
    // ShellExecute: the new copy inherits none of our handles.
    if !crate::tray::launch_detached(&target, &params, false) {
        unswap_exe(&target, staged);
        return Err("Couldn't start the new version, so the current one stays.".into());
    }
    crate::log::write(&format!("update installed, restarting ({})", target.display()));
    Ok(())
}

fn process_running(pid: u32) -> bool {
    crate::win::list_processes().iter().any(|p| p.pid == pid)
}

/// Remove leftovers of an update: `<exe>.old` and `<exe>.update`. The old exe can still be
/// mapped for a moment after its process exits, so retry for a few seconds.
fn clean_up_update_files(exe: &Path) {
    for suffix in [".old", ".update"] {
        let f = with_suffix(exe, suffix);
        for _ in 0..25 {
            if !f.exists() || fs::remove_file(&f).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Run by the new exe started with `--after-update <pid>`, before the tray starts: wait (up to
/// 5 minutes) for the old copy to exit, remove the leftovers, write the CLI that this exe
/// carries next to it, and refresh the Settings > Apps entry if this is the installed copy.
pub fn finish_update(old_pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while old_pid != 0 && old_pid != std::process::id() && process_running(old_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    let Ok(exe) = std::env::current_exe() else { return };
    clean_up_update_files(&exe);
    if let Some(dir) = exe.parent() {
        if let Err(e) = crate::install::write_embedded_cli(dir) {
            crate::log::write(&format!("after update: {}", e));
        }
    }
    crate::install::refresh_install_record();
    crate::log::write(&format!("updated to {}", env!("CARGO_PKG_VERSION")));
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_URL: &str = "https://github.com/AskTinNguyen/AtherMemGuard/releases/download/v0.2.0/AtherMemGuard-Setup-0.2.0.exe";

    fn manifest(url: &str, sha: &str, version: &str, size: &str) -> String {
        format!(
            r#"{{"windows":{{"version":"{}","url":"{}","sha256":"{}","size":{},"notes":"New things"}}}}"#,
            version, url, sha, size
        )
    }

    fn good(url: &str) -> String {
        manifest(url, &"a".repeat(64), "0.2.0", "2795520")
    }

    #[test]
    fn compares_versions() {
        assert_eq!(compare_versions("0.0.10", "0.0.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.0.2", "0.1"), Ordering::Less);
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.2.0", "0.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.2.0.1", "0.2.0"), Ordering::Greater);
        assert_eq!(compare_versions("10.0", "9.99.99"), Ordering::Greater);
        assert_eq!(compare_versions("", "0"), Ordering::Equal);
        assert_eq!(compare_versions("99999999999999999999999", "1"), Ordering::Greater);
    }

    #[test]
    fn parses_a_good_manifest() {
        let m = parse_manifest_with(&good(GOOD_URL), false).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.size, 2795520);
        assert_eq!(m.notes, "New things");
        assert_eq!(m.url, GOOD_URL);
        // A BOM, uppercase hex, a build number, the GitHub file host, no notes.
        assert!(parse_manifest_with(&format!("\u{feff}{}", good(GOOD_URL)), false).is_ok());
        let upper = parse_manifest_with(&manifest(GOOD_URL, &"AB".repeat(32), "0.2.0", "10"), false).unwrap();
        assert_eq!(upper.sha256, "ab".repeat(32));
        assert!(parse_manifest_with(&manifest(GOOD_URL, &"a".repeat(64), "0.2.0.1", "10"), false).is_ok());
        assert!(parse_manifest_with(&good("https://objects.githubusercontent.com/github-production-release-asset/x"), false).is_ok());
        let no_notes = format!(r#"{{"windows":{{"version":"1.0","url":"{}","sha256":"{}","size":5}}}}"#, GOOD_URL, "0".repeat(64));
        assert_eq!(parse_manifest_with(&no_notes, false).unwrap().notes, "");
        // Other platforms' entries are ignored.
        let both = format!(r#"{{"macos":{{"version":"9"}},"windows":{}}}"#, &good(GOOD_URL)[11..good(GOOD_URL).len() - 1]);
        assert!(parse_manifest_with(&both, false).is_ok());
    }

    #[test]
    fn the_published_manifest_parses() {
        let m = parse_manifest_with(include_str!("../downloads/latest.json"), false).unwrap();
        assert!(m.url.ends_with(&format!("AtherMemGuard-Setup-{}.exe", m.version)));
    }

    #[test]
    fn cuts_long_notes() {
        let long = "é".repeat(400);
        let json = format!(
            r#"{{"windows":{{"version":"1.0","url":"{}","sha256":"{}","size":5,"notes":"{}"}}}}"#,
            GOOD_URL,
            "0".repeat(64),
            long
        );
        assert_eq!(parse_manifest_with(&json, false).unwrap().notes.chars().count(), 300);
    }

    #[test]
    fn rejects_bad_manifests() {
        let sha = "a".repeat(64);
        let bad = [
            manifest(GOOD_URL, "1234", "0.2.0", "10"),                       // short hash
            manifest(GOOD_URL, &"g".repeat(64), "0.2.0", "10"),              // not hex
            manifest(GOOD_URL, &sha, "0.2.0; rm", "10"),                     // bad version
            manifest(GOOD_URL, &sha, "v0.2.0", "10"),                        // bad version
            manifest(GOOD_URL, &sha, "", "10"),                              // no version
            manifest(GOOD_URL, &sha, "1.2.3.4.5.6.7.8.9.10.11", "10"),       // version too long
            manifest(GOOD_URL, &sha, "0.2.0", "0"),                          // empty file
            manifest(GOOD_URL, &sha, "0.2.0", "-5"),                         // negative
            manifest(GOOD_URL, &sha, "0.2.0", &(MAX_EXE + 1).to_string()),   // too big
            manifest(GOOD_URL, &sha, "0.2.0", "\"10\""),                     // size as text
            good("http://github.com/AskTinNguyen/AtherMemGuard/x.exe"),      // not HTTPS
            good("https://evil.example/AskTinNguyen/AtherMemGuard/x.exe"),   // another host
            good("https://github.com.evil.example/x.exe"),                   // look-alike host
            good("https://user@github.com/x.exe"),                           // user info
            good("http://localhost:8000/x.exe"),                             // localhost without the test override
            good("http://127.0.0.1:8000/x.exe"),
            r#"{"macos":{}}"#.to_string(),
            r#"{"windows":"0.2.0"}"#.to_string(),
            "not json".to_string(),
        ];
        for json in bad {
            assert!(parse_manifest_with(&json, false).is_err(), "should reject {}", json);
        }
        let huge = format!("{}{}", good(GOOD_URL), " ".repeat(MAX_MANIFEST as usize));
        assert!(parse_manifest_with(&huge, true).unwrap_err().contains("larger"));
    }

    #[test]
    fn localhost_only_with_the_test_override() {
        assert!(parse_manifest_with(&good("http://localhost:8000/x.exe"), true).is_ok());
        assert!(parse_manifest_with(&good("http://127.0.0.1:8000/x.exe"), true).is_ok());
        assert!(parse_manifest_with(&good("http://192.168.1.2:8000/x.exe"), true).is_err());
        assert!(parse_manifest_with(&good("http://evil.example/x.exe"), true).is_err());
        assert!(parse_manifest_with(&good(GOOD_URL), true).is_ok());
    }

    #[test]
    fn validates_manifest_urls() {
        assert!(manifest_url_allowed(DEFAULT_MANIFEST_URL, false));
        assert!(manifest_url_allowed("https://updates.example.com/memguard/latest.json", false));
        assert!(!manifest_url_allowed("http://updates.example.com/latest.json", false));
        assert!(!manifest_url_allowed("http://localhost:9000/latest.json", false));
        assert!(manifest_url_allowed("http://localhost:9000/latest.json", true));
        assert!(!manifest_url_allowed("file:///C:/latest.json", true));
        assert!(!manifest_url_allowed("", true));
    }

    #[test]
    fn reads_the_manifest_url_from_settings_text() {
        assert_eq!(manifest_url_from_toml(""), "");
        assert_eq!(manifest_url_from_toml("[general]\npreset = \"balanced\"\n"), "");
        assert_eq!(manifest_url_from_toml("[updates]\ncheck_automatically = true\n"), "");
        assert_eq!(manifest_url_from_toml("[updates]\nmanifest_url = \"\"\n"), "");
        assert_eq!(
            manifest_url_from_toml("[updates]\nmanifest_url = \" https://x.example/latest.json \"\n"),
            "https://x.example/latest.json"
        );
        assert_eq!(manifest_url_from_toml("[updates]\nmanifest_url = 5\n"), "");
        assert_eq!(manifest_url_from_toml("this is [not toml"), "");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("memguard-updater-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn verifies_and_swaps_files() {
        let dir = temp_dir("swap");
        let staged = dir.join("app.exe.update");
        let target = dir.join("app.exe");
        fs::write(&staged, b"new version").unwrap();
        fs::write(&target, b"old").unwrap();
        let m = Manifest {
            version: "9.9.9".into(),
            url: GOOD_URL.into(),
            sha256: sha256_file(&staged).unwrap(),
            size: 11,
            notes: String::new(),
        };
        // sha256("new version")
        assert_eq!(m.sha256, hex(&Sha256::digest(b"new version")));
        assert!(verify_file(&staged, &m).is_ok());
        let upper = Manifest { sha256: m.sha256.to_ascii_uppercase(), ..m.clone() };
        assert!(verify_file(&staged, &upper).is_ok());
        let mut wrong = m.clone();
        wrong.sha256.replace_range(0..1, if m.sha256.starts_with('0') { "1" } else { "0" });
        assert!(verify_file(&staged, &wrong).unwrap_err().contains("checksum"));
        let short = Manifest { size: 12, ..m.clone() };
        assert!(verify_file(&staged, &short).unwrap_err().contains("incomplete"));
        assert!(verify_file(&dir.join("nothing"), &m).unwrap_err().contains("missing"));

        swap_exe(&target, &staged).unwrap();
        assert!(!staged.exists());
        assert_eq!(fs::read(&target).unwrap(), b"new version");
        assert_eq!(fs::read(dir.join("app.exe.old")).unwrap(), b"old");

        // Undo puts everything back as it was before the swap, minus the staged file.
        unswap_exe(&target, &staged);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!staged.exists() && !dir.join("app.exe.old").exists());

        // A missing staged file leaves the original where it was.
        assert!(swap_exe(&target, &dir.join("nothing.exe")).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");

        fs::write(dir.join("app.exe.old"), b"x").unwrap();
        fs::write(dir.join("app.exe.update"), b"y").unwrap();
        clean_up_update_files(&target);
        assert!(!dir.join("app.exe.old").exists() && !dir.join("app.exe.update").exists());
        assert!(target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_refuses_bad_manifests_without_writing() {
        let dir = temp_dir("refuse");
        let staged = dir.join("app.exe.update");
        let m = Manifest {
            version: "9.9.9".into(),
            url: "https://evil.example/x.exe".into(),
            sha256: "0".repeat(64),
            size: 10,
            notes: String::new(),
        };
        assert!(download_to(&m, &staged, &|_, _| {}).is_err());
        let big = Manifest { url: GOOD_URL.into(), size: MAX_EXE + 1, ..m };
        assert!(download_to(&big, &staged, &|_, _| {}).is_err());
        assert!(!staged.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

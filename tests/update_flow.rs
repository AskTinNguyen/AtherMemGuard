//! End-to-end updater test against a local HTTP server, run on a temp copy of an exe so the
//! real install is never touched.
//!
//! The parent copies this test exe to a temp folder as `AtherMemGuard.exe` and starts it as
//! the "running app" (with `ATHER_MEMGUARD_DIR` and `ATHER_MEMGUARD_UPDATE_URL` pointing at
//! temp folders and the test server). That copy checks for an update, downloads and verifies
//! the real `AtherMemGuard.exe` build, tries tampered manifests, then downloads a slightly
//! different copy of this test exe and installs it with `updater::install_staged`. The new
//! file is started with `--after-update <pid>` (this same `main`), runs
//! `updater::finish_update` and leaves a marker. The real tray app is never started.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use athermemguard::updater::{self, CheckResult, Manifest};

const ROLE: &str = "MEMGUARD_UPDATE_TEST_ROLE";
const SWAP_SHA: &str = "MEMGUARD_UPDATE_TEST_SWAP_SHA";
const SWAP_SIZE: &str = "MEMGUARD_UPDATE_TEST_SWAP_SIZE";
const BASE: &str = "MEMGUARD_UPDATE_TEST_BASE";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--after-update") {
        after_update(&args);
    } else if std::env::var(ROLE).as_deref() == Ok("child") {
        child();
    } else {
        parent();
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{:02x}", b)).collect()
}

fn with_suffix(p: &Path, s: &str) -> PathBuf {
    let mut o = p.as_os_str().to_os_string();
    o.push(s);
    PathBuf::from(o)
}

// ------------------------------------------------------------------ parent

fn serve(listener: TcpListener, routes: Vec<(String, Vec<u8>)>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                break;
            }
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        match routes.iter().find(|(p, _)| *p == path) {
            Some((_, body)) => {
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            }
            None => {
                let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        }
        let _ = stream.flush();
    }
}

fn manifest_json(version: &str, url: &str, bytes: &[u8]) -> Vec<u8> {
    format!(
        r#"{{"windows":{{"version":"{}","url":"{}","sha256":"{}","size":{},"notes":"Test build"}}}}"#,
        version,
        url,
        sha256_hex(bytes),
        bytes.len()
    )
    .into_bytes()
}

fn parent() {
    let tmp = std::env::temp_dir().join(format!("memguard-update-flow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let app_dir = tmp.join("app");
    let data_dir = tmp.join("data");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let me = std::env::current_exe().unwrap();
    let app = app_dir.join("AtherMemGuard.exe");
    std::fs::copy(&me, &app).unwrap();

    // The update the app downloads and verifies: the real GUI build.
    let real = std::fs::read(env!("CARGO_BIN_EXE_AtherMemGuard")).unwrap();
    // The update it installs: this test exe plus a few bytes, so it differs from the running copy.
    let mut swap = std::fs::read(&me).unwrap();
    swap.extend_from_slice(b"\0memguard-update-flow-test\0");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let routes = vec![
        ("/latest.json".to_string(), manifest_json("9.9.9", &format!("{}/real.exe", base), &real)),
        ("/old.json".to_string(), manifest_json("0.0.1", &format!("{}/real.exe", base), &real)),
        ("/elsewhere.json".to_string(), manifest_json("9.9.9", "https://evil.example/real.exe", &real)),
        ("/garbage.json".to_string(), b"<html>not json</html>".to_vec()),
        ("/huge.json".to_string(), vec![b' '; 70 * 1024]),
        ("/real.exe".to_string(), real.clone()),
        ("/swap.exe".to_string(), swap.clone()),
    ];
    std::thread::spawn(move || serve(listener, routes));

    let out = Command::new(&app)
        .env(ROLE, "child")
        .env("ATHER_MEMGUARD_DIR", &data_dir)
        .env("ATHER_MEMGUARD_UPDATE_URL", format!("{}/latest.json", base))
        .env(BASE, &base)
        .env(SWAP_SHA, sha256_hex(&swap))
        .env(SWAP_SIZE, swap.len().to_string())
        .output()
        .unwrap();
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "the running copy failed: {:?}", out.status);

    // The new version was started with --after-update; wait for it to finish.
    let marker = data_dir.join("after-update.txt");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let note = std::fs::read_to_string(&marker).expect("the new version never finished the update");
    println!("relaunched copy: {}", note.trim());
    assert!(note.starts_with("finished"), "{}", note);
    assert_eq!(sha256_hex(&std::fs::read(&app).unwrap()), sha256_hex(&swap), "the new exe is in place");
    assert!(!with_suffix(&app, ".old").exists(), "the old exe was cleaned up");
    assert!(!with_suffix(&app, ".update").exists(), "no download left behind");
    let log = std::fs::read_to_string(data_dir.join("memguard.log")).unwrap_or_default();
    assert!(log.contains("update installed, restarting") && log.contains("updated to"), "log:\n{}", log);

    // The relaunched copy may still be exiting; give it a moment before cleaning up.
    for _ in 0..25 {
        if std::fs::remove_dir_all(&tmp).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("update flow: ok");
}

// ------------------------------------------------------------------ the "running app" (temp copy)

fn child() {
    let base = std::env::var(BASE).unwrap();
    let exe = std::env::current_exe().unwrap();
    let staged_path = with_suffix(&exe, ".update");
    let data_dir = PathBuf::from(std::env::var("ATHER_MEMGUARD_DIR").unwrap());
    let set_url = |path: &str| std::env::set_var("ATHER_MEMGUARD_UPDATE_URL", format!("{}{}", base, path));

    // Where it looks: env override, then settings, then GitHub.
    assert_eq!(updater::manifest_url(), format!("{}/latest.json", base));
    std::env::remove_var("ATHER_MEMGUARD_UPDATE_URL");
    assert_eq!(updater::manifest_url(), updater::DEFAULT_MANIFEST_URL);
    std::fs::write(data_dir.join("settings.toml"), "[updates]\nmanifest_url = \"https://updates.example.com/latest.json\"\n").unwrap();
    assert_eq!(updater::manifest_url(), "https://updates.example.com/latest.json");
    std::fs::remove_file(data_dir.join("settings.toml")).unwrap();
    // Without the test override, a manifest pointing at this PC over http is refused.
    let local = format!(
        r#"{{"windows":{{"version":"9.9.9","url":"{}/real.exe","sha256":"{}","size":5}}}}"#,
        base,
        "0".repeat(64)
    );
    assert!(updater::parse_manifest(&local).is_err());
    set_url("/latest.json");
    assert!(updater::parse_manifest(&local).is_ok());

    // Up to date, unreadable and refused manifests.
    set_url("/old.json");
    assert_eq!(updater::check(), CheckResult::UpToDate);
    for (path, expect) in [
        ("/garbage.json", "couldn't be read"),
        ("/huge.json", "larger than expected"),
        ("/elsewhere.json", "isn't on GitHub"),
        ("/missing.json", "404"),
    ] {
        set_url(path);
        match updater::check() {
            CheckResult::Failed(e) => assert!(e.contains(expect), "{}: {}", path, e),
            other => panic!("{}: expected a failure, got {:?}", path, other),
        }
    }
    std::env::set_var("ATHER_MEMGUARD_UPDATE_URL", "http://127.0.0.1:1/latest.json");
    match updater::check() {
        CheckResult::Failed(e) => println!("unreachable server: {}", e),
        other => panic!("expected a failure, got {:?}", other),
    }

    // Check finds the newer version; download fetches and verifies it.
    set_url("/latest.json");
    let m = match updater::check() {
        CheckResult::Available(m) => m,
        other => panic!("expected an update, got {:?}", other),
    };
    assert_eq!(m.version, "9.9.9");
    assert_eq!(m.notes, "Test build");
    let last = std::cell::Cell::new((0u64, 0u64));
    let staged = updater::download(&m, &|got, total| last.set((got, total))).expect("download");
    assert_eq!(staged, staged_path);
    assert_eq!(last.get(), (m.size, m.size), "progress reached the end");
    assert_eq!(std::fs::metadata(&staged).unwrap().len(), m.size);
    std::fs::remove_file(&staged).unwrap();

    // Tampered manifests never leave a file behind.
    let mut bad = m.clone();
    bad.sha256.replace_range(0..1, if m.sha256.starts_with('0') { "1" } else { "0" });
    let e = updater::download(&bad, &|_, _| {}).unwrap_err();
    assert!(e.contains("checksum"), "{}", e);
    assert!(!staged_path.exists());
    let e = updater::download(&Manifest { size: m.size - 1, ..m.clone() }, &|_, _| {}).unwrap_err();
    assert!(e.contains("larger than expected"), "{}", e);
    assert!(!staged_path.exists());
    let e = updater::download(&Manifest { size: m.size + 1, ..m.clone() }, &|_, _| {}).unwrap_err();
    assert!(e.contains("incomplete"), "{}", e);
    assert!(!staged_path.exists());
    let e = updater::download(&Manifest { url: format!("{}/nothing.exe", base), ..m.clone() }, &|_, _| {}).unwrap_err();
    assert!(e.contains("404"), "{}", e);
    assert!(!staged_path.exists());

    // The real thing: download the new version, swap it in and restart.
    let swap = Manifest {
        version: "9.9.9".into(),
        url: format!("{}/swap.exe", base),
        sha256: std::env::var(SWAP_SHA).unwrap(),
        size: std::env::var(SWAP_SIZE).unwrap().parse().unwrap(),
        notes: String::new(),
    };
    let staged = updater::download(&swap, &|_, _| {}).expect("download the swap build");
    updater::install_staged(&staged).expect("install_staged");
    assert!(with_suffix(&exe, ".old").exists(), "the running exe was moved aside");
    assert!(!staged.exists(), "the download was moved into place");
    println!("running copy: swapped and restarted, exiting");
}

// ------------------------------------------------------------------ the new version

fn after_update(args: &[String]) {
    let pid: u32 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let data_dir = PathBuf::from(std::env::var("ATHER_MEMGUARD_DIR").expect("ATHER_MEMGUARD_DIR must be set"));
    updater::finish_update(pid);
    let exe = std::env::current_exe().unwrap();
    let ok = !with_suffix(&exe, ".old").exists() && !with_suffix(&exe, ".update").exists();
    let note = if ok { format!("finished after pid {}", pid) } else { "leftovers remain".to_string() };
    std::fs::write(data_dir.join("after-update.txt"), note).unwrap();
}

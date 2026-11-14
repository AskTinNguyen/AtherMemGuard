//! Doctor actions against things these tests create themselves, and nothing else:
//! a copy of powershell.exe showing a window ("DoctorTestWindow"), a copy of ping.exe
//! ("DoctorTestPing"), and a throwaway HKCU Run value named `AtherMemGuardDoctorTest`
//! (plus its StartupApproved twin), removed afterwards. Every report here is built by hand
//! with the test's own PIDs, so nothing depends on the live process list.
//!
//! `ATHER_MEMGUARD_DIR` points at a temp folder, so the undo log, settings and log never
//! touch the real data folder.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use athermemguard::doctor::{self, Action, ActionKind, Finding, FindingKind, OutcomeKind, Report, Verdict};
use athermemguard::engine::SystemStatus;
use athermemguard::startup::{self, Location};
use athermemguard::win;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TEST_VALUE: &str = "AtherMemGuardDoctorTest";

static SERIAL: Mutex<()> = Mutex::new(());

/// One temp data folder for the whole test binary, set before anything reads paths.
fn setup() -> (MutexGuard<'static, ()>, PathBuf) {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR
        .get_or_init(|| {
            let d = std::env::temp_dir().join(format!("athermemguard-doctor-test-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            std::env::set_var("ATHER_MEMGUARD_DIR", &d);
            d
        })
        .clone();
    assert_eq!(athermemguard::paths::data_dir(), dir, "tests must use the temp data folder");
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    (guard, dir)
}

/// Copy a system exe under a name no catalogue entry or rule matches.
fn copy_exe(dir: &PathBuf, from: &str, name: &str) -> PathBuf {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let to = dir.join(format!("{}.exe", name));
    if !to.exists() {
        std::fs::copy(PathBuf::from(windir).join(from), &to).unwrap();
    }
    to
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn app_finding(id: &str, name: &str, pids: Vec<u32>, actions: &[ActionKind]) -> Finding {
    Finding {
        id: id.into(),
        kind: FindingKind::App,
        verdict: Verdict::Close,
        title: name.into(),
        detail: "Test app.".into(),
        processes: pids.len(),
        ram_mb: 1,
        committed_mb: 1,
        actions: actions.iter().map(|&a| Action::new(a, Some("Chat"))).collect(),
        recommended: false,
        category: "Test".into(),
        names: vec![name.into()],
        pids,
        has_window: false,
        group: None,
        startup: None,
    }
}

fn report(findings: Vec<Finding>) -> Report {
    Report {
        time: win::local_time_string(),
        system: SystemStatus::from(&win::SysInfo::default()),
        unreal_active: false,
        findings,
        reclaimable_ram_gb: 0.0,
        reclaimable_commit_gb: 0.0,
    }
}

fn exited(child: &mut Child, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn pids_named(name: &str) -> Vec<u32> {
    win::list_processes().into_iter().filter(|p| p.name.eq_ignore_ascii_case(name)).map(|p| p.pid).collect()
}

const FORM_SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms; \
$f = New-Object System.Windows.Forms.Form; $f.Text = 'DoctorTestWindow'; $f.WindowState = 'Minimized'; \
[System.Windows.Forms.Application]::Run($f)";

#[test]
fn close_a_window_app_it_started_then_undo_starts_it_again() {
    let (_g, dir) = setup();
    let exe = copy_exe(&dir, r"System32\WindowsPowerShell\v1.0\powershell.exe", "DoctorTestWindow");
    let child = Command::new(&exe)
        .args(["-NoProfile", "-NonInteractive", "-Command", FORM_SCRIPT])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    let mut child = Kill(child);
    let pid = child.0.id();
    let r = report(vec![app_finding(
        "app:doctortestwindow",
        "DoctorTestWindow",
        vec![pid],
        &[ActionKind::Close, ActionKind::BlockWhileUnreal],
    )]);
    let pick = vec![("app:doctortestwindow".to_string(), ActionKind::Close)];

    // Wait for the window to appear: the dry run says "Would ask ... to close" once it has one.
    let start = Instant::now();
    loop {
        let o = doctor::dry_run(&r, &pick, false);
        if o[0].result == OutcomeKind::WouldDo {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "the test window never appeared: {:?}", o);
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(child.0.try_wait().unwrap().is_none(), "dry run must not close anything");

    let out = doctor::apply(&r, &pick, false);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].result, OutcomeKind::Done, "{:?}", out);
    assert_eq!(out[0].message, "Closed");
    assert!(exited(&mut child.0, Duration::from_secs(5)));
    let undo_id = out[0].undo_id.clone().expect("closing can be undone");

    let log = doctor::UndoLog::load();
    let entry = log.runs.iter().flat_map(|r| &r.entries).find(|e| e.id == undo_id).unwrap();
    match &entry.step {
        doctor::UndoStep::Relaunch { apps } => {
            assert_eq!(apps.len(), 1);
            assert!(apps[0].exe.to_ascii_lowercase().ends_with("doctortestwindow.exe"), "{:?}", apps);
            assert!(apps[0].command_line.contains("DoctorTestWindow"), "{:?}", apps);
        }
        other => panic!("expected a relaunch step, got {:?}", other),
    }

    // Undo starts it again (with the same command line); then end the new copy ourselves.
    let before: Vec<u32> = pids_named("DoctorTestWindow");
    let out = doctor::undo(false, Some(&undo_id));
    assert_eq!(out[0].result, OutcomeKind::Undone, "{:?}", out);
    let start = Instant::now();
    let relaunched = loop {
        let now: Vec<u32> = pids_named("DoctorTestWindow").into_iter().filter(|p| !before.contains(p)).collect();
        if let Some(&p) = now.first() {
            break p;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "undo did not start the app again");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(win::terminate(relaunched));
    // Undoing twice does nothing.
    let out = doctor::undo(false, Some(&undo_id));
    assert_eq!(out[0].result, OutcomeKind::Refused);
}

#[test]
fn windowless_app_needs_force_to_end() {
    let (_g, dir) = setup();
    let exe = copy_exe(&dir, r"System32\PING.EXE", "DoctorTestPing");
    let child = Command::new(&exe).args(["-t", "127.0.0.1"]).creation_flags(CREATE_NO_WINDOW).spawn().unwrap();
    let mut child = Kill(child);
    let pid = child.0.id();
    std::thread::sleep(Duration::from_millis(300));
    let r = report(vec![app_finding("app:doctortestping", "DoctorTestPing", vec![pid], &[ActionKind::Close, ActionKind::End])]);

    let out = doctor::apply(&r, &[("app:doctortestping".into(), ActionKind::Close)], false);
    assert_eq!(out[0].result, OutcomeKind::StillOpen, "{:?}", out);
    assert!(out[0].message.contains("no window"), "{}", out[0].message);

    let out = doctor::apply(&r, &[("app:doctortestping".into(), ActionKind::End)], false);
    assert_eq!(out[0].result, OutcomeKind::Refused);
    assert!(out[0].message.contains("--force"));
    assert!(child.0.try_wait().unwrap().is_none(), "still running without force");

    let out = doctor::dry_run(&r, &[("app:doctortestping".into(), ActionKind::End)], true);
    assert_eq!(out[0].result, OutcomeKind::WouldDo);
    assert!(child.0.try_wait().unwrap().is_none(), "dry run ends nothing");

    let out = doctor::apply(&r, &[("app:doctortestping".into(), ActionKind::End)], true);
    assert_eq!(out[0].result, OutcomeKind::Done, "{:?}", out);
    assert!(out[0].undo_id.is_none(), "ending can't be undone");
    assert!(exited(&mut child.0, Duration::from_secs(5)));

    // Gone now: a second try reports it as already closed.
    let out = doctor::apply(&r, &[("app:doctortestping".into(), ActionKind::End)], true);
    assert_eq!(out[0].message, "Already closed");
}

#[test]
fn never_acts_on_itself_or_unknown_findings() {
    let (_g, _dir) = setup();
    let me = std::process::id();
    let name = win::list_processes().into_iter().find(|p| p.pid == me).unwrap().name;
    let r = report(vec![app_finding("app:me", &name, vec![me], &[ActionKind::Close, ActionKind::End])]);
    for (action, force) in [(ActionKind::Close, false), (ActionKind::End, true)] {
        let out = doctor::apply(&r, &[("app:me".into(), action)], force);
        assert_eq!(out[0].result, OutcomeKind::Refused, "{:?}", out);
        assert!(out[0].message.contains("MemGuard itself"), "{}", out[0].message);
    }
    let out = doctor::apply(&r, &[("app:nope".into(), ActionKind::Close)], false);
    assert_eq!(out[0].result, OutcomeKind::Refused);
    // An action the finding doesn't offer.
    let out = doctor::apply(&r, &[("app:me".into(), ActionKind::DisableStartup)], false);
    assert_eq!(out[0].result, OutcomeKind::Refused);
}

/// Removes the throwaway Run value and its StartupApproved twin, even if the test fails.
struct RunValue;
impl RunValue {
    fn create(command: &str) -> RunValue {
        use winreg::enums::HKEY_CURRENT_USER;
        let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu.create_subkey(startup::RUN_KEY).unwrap();
        assert!(run.get_raw_value(TEST_VALUE).is_err(), "{} already exists; remove it first", TEST_VALUE);
        assert!(startup::approved_value(Location::HkcuRun, TEST_VALUE).is_none());
        run.set_value(TEST_VALUE, &command.to_string()).unwrap();
        RunValue
    }
}
impl Drop for RunValue {
    fn drop(&mut self) {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
        let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(k) = hkcu.open_subkey_with_flags(startup::RUN_KEY, KEY_SET_VALUE) {
            let _ = k.delete_value(TEST_VALUE);
        }
        if let Ok(k) = hkcu.open_subkey_with_flags(startup::APPROVED_RUN, KEY_SET_VALUE) {
            let _ = k.delete_value(TEST_VALUE);
        }
    }
}

#[test]
fn switches_the_throwaway_startup_value_off_and_undoes_it() {
    let (_g, dir) = setup();
    let target = dir.join("DoctorTestStartup.exe");
    let _value = RunValue::create(&format!("\"{}\" --background", target.display()));

    let entry = startup::list().into_iter().find(|e| e.location == Location::HkcuRun && e.name == TEST_VALUE).unwrap();
    assert!(entry.enabled);
    assert_eq!(entry.exe, "DoctorTestStartup");

    let finding = Finding {
        id: format!("startup:hkcu:{}", TEST_VALUE),
        kind: FindingKind::Startup,
        startup: Some(entry.clone()),
        names: vec![entry.exe.clone()],
        actions: vec![Action::new(ActionKind::DisableStartup, None)],
        ..app_finding("x", "DoctorTestStartup", vec![], &[])
    };
    let id = finding.id.clone();
    let r = report(vec![finding]);

    let out = doctor::dry_run(&r, &[(id.clone(), ActionKind::DisableStartup)], false);
    assert_eq!(out[0].result, OutcomeKind::WouldDo);
    assert!(startup::approved_value(Location::HkcuRun, TEST_VALUE).is_none(), "dry run writes nothing");

    let out = doctor::apply(&r, &[(id.clone(), ActionKind::DisableStartup)], false);
    assert_eq!(out[0].result, OutcomeKind::Done, "{:?}", out);
    assert_eq!(out[0].message, "Switched off");
    let bytes = startup::approved_value(Location::HkcuRun, TEST_VALUE).unwrap();
    assert_eq!(bytes.len(), 12);
    assert_eq!(startup::decode_approved(&bytes), Some(false));
    assert!(startup::approved_time(&bytes).is_some());
    let listed = startup::list().into_iter().find(|e| e.name == TEST_VALUE).unwrap();
    assert!(!listed.enabled, "shows as off");

    // Undo by id: the StartupApproved value didn't exist before, so it's removed again.
    let out = doctor::undo(false, out[0].undo_id.as_deref());
    assert_eq!(out[0].result, OutcomeKind::Undone, "{:?}", out);
    assert_eq!(out[0].message, "Switched back on");
    assert!(startup::approved_value(Location::HkcuRun, TEST_VALUE).is_none());
    assert!(startup::list().into_iter().find(|e| e.name == TEST_VALUE).unwrap().enabled);

    // Once more, undone as "the last run". A value that was there before is restored as it was.
    let on = startup::encode_approved(true, 0).to_vec();
    startup::restore(Location::HkcuRun, TEST_VALUE, Some(&on)).unwrap();
    let out = doctor::apply(&r, &[(id.clone(), ActionKind::DisableStartup)], false);
    assert_eq!(out[0].result, OutcomeKind::Done);
    assert!(doctor::UndoLog::load().can_undo());
    let out = doctor::undo(true, None);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert_eq!(out[0].result, OutcomeKind::Undone);
    assert_eq!(startup::approved_value(Location::HkcuRun, TEST_VALUE), Some(on));

    // Machine-wide entries are refused, whatever the report says.
    let mut hklm = r.findings[0].clone();
    hklm.id = "startup:hklm:Fake".into();
    if let Some(e) = hklm.startup.as_mut() {
        e.location = Location::HklmRun;
        e.name = "Fake".into();
    }
    let out = doctor::apply(&report(vec![hklm]), &[("startup:hklm:Fake".into(), ActionKind::DisableStartup)], false);
    assert_eq!(out[0].result, OutcomeKind::Refused);
    assert!(out[0].message.contains("administrator"));
}

#[test]
fn settings_actions_edit_and_undo_the_temp_settings_file() {
    let (_g, dir) = setup();
    let settings = dir.join("settings.toml");
    std::fs::write(&settings, athermemguard::config::DEFAULT_TOML).unwrap();
    let original = std::fs::read_to_string(&settings).unwrap();

    let mut hold = app_finding("app:doctortestchat", "DoctorTestChat", vec![], &[ActionKind::HoldBack]);
    hold.verdict = Verdict::HoldBack;
    hold.group = Some("Chat".into());
    let block = app_finding("app:doctortestgame", "DoctorTestGame", vec![], &[ActionKind::Close, ActionKind::BlockWhileUnreal]);
    let mut review = app_finding("app:doctortestmystery", "DoctorTestMystery", vec![], &[ActionKind::Close, ActionKind::LeaveIt]);
    review.verdict = Verdict::Review;
    let r = report(vec![hold, block, review]);

    let picks = vec![
        ("app:doctortestchat".to_string(), ActionKind::HoldBack),
        ("app:doctortestgame".to_string(), ActionKind::BlockWhileUnreal),
        ("app:doctortestmystery".to_string(), ActionKind::LeaveIt),
    ];
    let out = doctor::apply(&r, &picks, false);
    assert!(out.iter().all(|o| o.result == OutcomeKind::Done), "{:?}", out);
    assert_eq!(out[0].message, "Added to Chat");

    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(text.contains("# AtherMemGuard settings"), "comments are kept");
    let ds = doctor::DoctorSettings::load();
    assert_eq!(ds.close_while_unreal, vec!["DoctorTestGame".to_string()]);
    assert_eq!(ds.keep, vec!["DoctorTestMystery".to_string()]);
    doctor::validate_settings(&text).unwrap();
    let mut guard = doctor::Guard::from_settings();
    let p = win::Proc { pid: 4242, ppid: 1, name: "DoctorTestGame".into(), ram: 0, committed: 0, created: 1 };
    assert_eq!(guard.on_tick(std::slice::from_ref(&p), true).len(), 1, "the guard picks up the new list");

    // Hold back a second time: nothing new to add.
    let out = doctor::apply(&r, &picks[..1], false);
    assert_eq!(out[0].result, OutcomeKind::Done);
    assert!(out[0].undo_id.is_none());

    // Undo the whole run: the file is back to what it was.
    let out = doctor::undo(true, None);
    assert_eq!(out.len(), 3, "{:?}", out);
    assert!(out.iter().all(|o| o.result == OutcomeKind::Undone), "{:?}", out);
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);

    // block_while_unreal is never offered for hold_back apps, even if asked.
    let out = doctor::apply(&r, &[("app:doctortestchat".into(), ActionKind::BlockWhileUnreal)], false);
    assert_eq!(out[0].result, OutcomeKind::Refused);
}

#[test]
fn real_scan_is_read_only_and_sane() {
    let (_g, dir) = setup();
    let cfg = athermemguard::config::Config::builtin();
    let r = doctor::scan(&cfg);
    assert!(r.system.ram_total_gb > 0.0);
    for f in &r.findings {
        assert!(!f.title.is_empty() && !f.detail.is_empty(), "{:?}", f);
        assert_ne!(f.verdict, Verdict::Keep, "keep is never shown: {}", f.id);
        if f.recommended {
            assert!(matches!(f.first_action(), Some(ActionKind::Close) | Some(ActionKind::DisableStartup)), "{}", f.id);
        }
        for name in &f.names {
            let lower = name.to_ascii_lowercase();
            for never in ["claude", "codex", "chatgpt", "chrome", "slack", "wispr flow", "rustdesk", "parsecd", "tailscale-ipn", "git", "dotnet", "devenv"] {
                if lower == never {
                    assert_ne!(f.verdict, Verdict::Close, "{} must never be close", name);
                }
            }
        }
    }
    // Scanning wrote nothing to the data folder.
    assert!(!dir.join("doctor-undo.json.tmp").exists());
}

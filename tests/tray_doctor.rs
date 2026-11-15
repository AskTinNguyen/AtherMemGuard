//! The tray's checkup window source (`tray::RealDoctor`) and the doctor's guard, against things
//! this test creates itself: a temp data folder (`ATHER_MEMGUARD_DIR`) with its own
//! settings.toml, and a copy of ping.exe named "TrayTestGuardPing". Reports are built by hand,
//! so nothing here looks at or acts on anyone else's apps.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, MutexGuard, OnceLock};

use athermemguard::doctor::{self, Action, ActionKind, Finding, FindingKind, GuardEvent, OutcomeKind, Report, Verdict};
use athermemguard::engine::SystemStatus;
use athermemguard::tray::RealDoctor;
use athermemguard::ui::doctor::{self as vm, DoctorSource};
use athermemguard::win;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const GUARDED: &str = "TrayTestGuardPing";

static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> (MutexGuard<'static, ()>, PathBuf) {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR
        .get_or_init(|| {
            let d = std::env::temp_dir().join(format!("athermemguard-tray-doctor-test-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            std::env::set_var("ATHER_MEMGUARD_DIR", &d);
            d
        })
        .clone();
    assert_eq!(athermemguard::paths::data_dir(), dir, "tests must use the temp data folder");
    (SERIAL.lock().unwrap_or_else(|e| e.into_inner()), dir)
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn finding(id: &str, name: &str, verdict: Verdict, actions: &[ActionKind]) -> Finding {
    Finding {
        id: id.into(),
        kind: FindingKind::App,
        verdict,
        title: name.into(),
        detail: "Test app.".into(),
        processes: 0,
        ram_mb: 1,
        committed_mb: 1,
        actions: actions.iter().map(|&a| Action::new(a, Some("Background"))).collect(),
        recommended: false,
        category: String::new(),
        names: vec![name.into()],
        pids: vec![],
        has_window: false,
        group: None,
        startup: None,
        company: String::new(),
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

#[test]
fn leave_it_from_the_window_writes_keep_and_undo_removes_it() {
    let (_g, dir) = setup();
    std::fs::write(dir.join("settings.toml"), athermemguard::config::DEFAULT_TOML).unwrap();
    let r = report(vec![
        finding("app:traytestmystery", "TrayTestMystery", Verdict::Review, &[ActionKind::Close, ActionKind::HoldBack, ActionKind::LeaveIt]),
        finding("app:traytestgame", "TrayTestGame", Verdict::Close, &[ActionKind::Close, ActionKind::BlockWhileUnreal]),
    ]);
    let d = RealDoctor::with_report(r);
    assert!(!d.can_undo(vm::Tab::Memory));

    // The window's "Leave it" is Keep; "Close whenever Unreal is running" sends BlockWhileUnreal
    // (with Close, which finds no process to close here); End without force is refused.
    let (outs, summary) = d.apply_mapped(
        &[
            ("app:traytestmystery".into(), vm::ActionKind::Keep),
            ("app:traytestgame".into(), vm::ActionKind::BlockWhileUnreal),
            ("app:traytestgame".into(), vm::ActionKind::End),
            ("app:nothere".into(), vm::ActionKind::Close),
        ],
        false,
    );
    assert_eq!(outs.len(), 4, "{:?}", outs);
    assert_eq!((outs[0].action, outs[0].status), (vm::ActionKind::Keep, vm::OutcomeStatus::Done), "{:?}", outs[0]);
    assert_eq!(outs[1].status, vm::OutcomeStatus::Done, "{:?}", outs[1]);
    assert_eq!(outs[2].status, vm::OutcomeStatus::Skipped, "{:?}", outs[2]);
    assert_eq!(outs[3].status, vm::OutcomeStatus::Skipped, "{:?}", outs[3]);
    let s = summary.expect("something was done");
    assert_eq!((s.freed_gb, s.startup_off), (0.0, 0), "settings changes free nothing now");

    let ds = doctor::DoctorSettings::load();
    assert_eq!(ds.keep, vec!["TrayTestMystery".to_string()]);
    assert_eq!(ds.close_while_unreal, vec!["TrayTestGame".to_string()]);
    assert!(d.can_undo(vm::Tab::Memory));
    assert!(!d.can_undo(vm::Tab::Disk), "a memory cleanup is not the Disk space tab's to undo");
    assert!(d.undo_last(vm::Tab::Disk).is_empty(), "Undo on Disk space leaves the memory cleanup alone");
    assert!(d.can_undo(vm::Tab::Memory));

    let undone = d.undo_last(vm::Tab::Memory);
    assert_eq!(undone.len(), 2, "{:?}", undone);
    assert!(undone.iter().all(|o| o.status == vm::OutcomeStatus::Done), "{:?}", undone);
    let ds = doctor::DoctorSettings::load();
    assert!(ds.keep.is_empty() && ds.close_while_unreal.is_empty());
    assert!(!d.can_undo(vm::Tab::Memory));
    // Nothing left to undo: nothing mapped onto a row.
    assert!(d.undo_last(vm::Tab::Memory).is_empty());
}

#[test]
fn guard_watches_only_the_listed_app_and_allow_this_time_sticks() {
    let (_g, dir) = setup();
    std::fs::write(dir.join("settings.toml"), format!("[doctor]\nclose_while_unreal = [\"{}\"]\n", GUARDED)).unwrap();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let exe = dir.join(format!("{}.exe", GUARDED));
    if !exe.exists() {
        std::fs::copy(PathBuf::from(windir).join(r"System32\PING.EXE"), &exe).unwrap();
    }
    let child = Kill(Command::new(&exe).args(["-t", "127.0.0.1"]).creation_flags(CREATE_NO_WINDOW).spawn().unwrap());
    let pid = child.0.id();
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut guard = doctor::Guard::from_settings();
    let procs = win::list_processes();
    assert!(guard.on_tick(&procs, false).is_empty(), "nothing while Unreal isn't working");
    let events = guard.on_tick(&procs, true);
    assert_eq!(events.len(), 1, "{:?}", events);
    let GuardEvent::Close { app, pids } = &events[0];
    assert_eq!((app.as_str(), pids.as_slice()), (GUARDED, &[pid][..]), "only the test's own process");
    assert!(guard.on_tick(&procs, true).is_empty(), "each appearance once");

    // It has no window, so the guard can only say so; the process keeps running.
    let out = doctor::guard_close(&events[0]);
    assert_eq!(out.result, OutcomeKind::StillOpen, "{:?}", out);
    assert!(win::list_processes().iter().any(|p| p.pid == pid), "guard_close never kills");

    // "Allow this time" until Unreal stops.
    let mut guard = doctor::Guard::from_settings();
    guard.allow(GUARDED);
    assert!(guard.on_tick(&procs, true).is_empty());
    guard.on_tick(&procs, false);
    assert_eq!(guard.on_tick(&procs, true).len(), 1, "allowed only until Unreal stopped");
    drop(child);
}

//! Open the settings window on the fake store and fire a few toasts, for manual checks.
//! `cargo run --example ui_demo [-- --close-after <seconds> --capture <dir>]`
//! Uses a temporary data folder (ATHER_MEMGUARD_DIR) unless one is set, so ui-state.toml
//! never lands in the real one.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use athermemguard::ui::settings::{open_settings, SettingsHost, UpdateState};
use athermemguard::ui::settings_store::FakeStore;
use athermemguard::ui::status_source::FakeStatus;
use athermemguard::ui::toast::{self, catalog, ToastId};
use athermemguard::ui::{window, Command};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{KillTimer, PostMessageW, SetTimer, WM_CLOSE};

thread_local! {
    static DOWNLOAD: Cell<(ToastId, u32)> = const { Cell::new((ToastId(0), 0)) };
    static SETTINGS: Cell<isize> = const { Cell::new(0) };
    static CAPTURE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

unsafe extern "system" fn progress(_: HWND, _: u32, id: usize, _: u32) {
    DOWNLOAD.with(|d| {
        let (tid, pct) = d.get();
        let pct = (pct + 7).min(100);
        toast::update_body(tid, &catalog::downloading_body(pct));
        d.set((tid, pct));
        if pct >= 100 {
            KillTimer(std::ptr::null_mut(), id);
        }
    });
}

unsafe extern "system" fn close(_: HWND, _: u32, id: usize, _: u32) {
    KillTimer(std::ptr::null_mut(), id);
    let hwnd = SETTINGS.with(|s| s.get()) as HWND;
    CAPTURE.with(|c| {
        if let Some(dir) = c.borrow().as_ref() {
            let path = dir.join("demo-settings-window.png");
            match window::capture_png(hwnd, &path) {
                Ok(()) => println!("captured {}", path.display()),
                Err(e) => eprintln!("capture failed: {}", e),
            }
        }
    });
    toast::dismiss_all();
    PostMessageW(hwnd, WM_CLOSE, 0, 0);
}

fn main() {
    if std::env::var_os("ATHER_MEMGUARD_DIR").is_none() {
        std::env::set_var("ATHER_MEMGUARD_DIR", std::env::temp_dir().join("AtherMemGuard-ui-demo"));
    }
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    CAPTURE.with(|c| *c.borrow_mut() = arg("--capture").map(PathBuf::from));

    toast::set_command_handler(Box::new(|c: Command| println!("toast command: {:?}", c)));
    let hwnd = open_settings(SettingsHost {
        store: Box::new(FakeStore::mockup()),
        status: Box::new(FakeStatus::default()),
        commands: Box::new(|c| println!("settings command: {:?}", c)),
        app_version: "0.2.0".into(),
        update_state: Box::new(|| UpdateState::Available { version: "0.2.1".into() }),
    });
    SETTINGS.with(|s| s.set(hwnd as isize));

    toast::show(catalog::first_run("Ctrl+Alt+M"));
    toast::show(catalog::editor_growing(8.0, "2 hours"));
    toast::show(catalog::memory_critical(4.1, 6.2));
    // A fourth: the oldest leaves early. Its body counts up in place.
    let id = toast::show(catalog::downloading(0));
    DOWNLOAD.with(|d| d.set((id, 0)));
    unsafe {
        SetTimer(std::ptr::null_mut(), 0, 400, Some(progress));
        if let Some(secs) = arg("--close-after").and_then(|s| s.parse::<u32>().ok()) {
            SetTimer(std::ptr::null_mut(), 0, secs * 1000, Some(close));
        }
    }
    window::run_until_closed(hwnd);
}

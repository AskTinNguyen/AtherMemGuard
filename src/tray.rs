//! Tray icon, menu, flyout, toasts and the background loop (docs/UI.md section 3).
//!
//! One hidden window on the UI thread owns the engine and a 1-second timer. Everything the
//! person can do arrives as a `Command` posted to that window (menu, flyout, toasts, the prime
//! hotkey, the settings window) and goes through `dispatch`, the one place commands are carried
//! out. Slow work (freeing memory, update checks and downloads) runs on worker threads, which
//! post their result back as a `Work` message; workers never touch UI. Toasts come from the
//! engine's events (`Engine::take_events`) through `model::ToastBrain`.

use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageTimeoutW,
    SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage, HICON, HWND_BROADCAST, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
    SMTO_ABORTIFHUNG, SM_CXSMICON, SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY,
    WM_LBUTTONDBLCLK, WM_NULL, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW,
};

use crate::engine::{Engine, Event, Level, Status};
use crate::ui::flyout::{self, FlyoutData};
use crate::ui::settings::{SettingsHost, UpdateState};
use crate::ui::toast::{self, catalog, ToastId};
use crate::ui::Command;
use crate::updater::{CheckResult, Manifest};
use crate::win::{self, wide, Handle};
use crate::{ops, paths, updater};

pub use model::*;

const WM_TRAY: u32 = WM_APP + 1;
/// NIN_SELECT | NINF_KEY: Enter or Space on the focused tray icon.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const WM_RUN_COMMAND: u32 = WM_APP + 2;
const WM_WORK: u32 = WM_APP + 3;
const TIMER_ID: usize = 1;
const HOTKEY_ID: i32 = 1;
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(60);
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 3600);
/// How long after start the RAM check runs for its toast (it reads firmware tables; no hurry).
const RAM_CHECK_AFTER: Duration = Duration::from_secs(180);
/// How often the tray reads free disk space (proposal 4 A). The engine tick never does.
const DISK_POLL: Duration = Duration::from_secs(60);
/// The RAM report page "Show me how" writes into the data folder.
pub const RAM_REPORT_FILE: &str = "ram-report.html";
pub const RELEASES_URL: &str = "https://github.com/AskTinNguyen/AtherMemGuard/releases/latest";

/// The tray window, for posting commands from closures (toasts, flyout, settings window).
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

// ---------------------------------------------------------------- the pure parts

/// Decisions with no Win32 in them: tooltip and menu text, hotkey parsing, which toast an
/// engine event shows, update announcements, the `[tray]` table in ui-state.toml.
pub mod model {
    use std::cmp::Ordering;

    use crate::engine::{Event, Level, Status};
    use crate::ui::flyout::{mode_words, pressure_words};
    use crate::ui::toast::{catalog, Toast};
    use crate::ui::Command;

    /// What the tooltip and the menu need to know.
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct TrayView {
        /// Engine mode, "editor+build".
        pub mode: String,
        pub pressure: Option<Level>,
        pub paused: bool,
        /// "15:42" for a timed pause; `None` while paused until resumed.
        pub paused_until: Option<String>,
        pub ram_free_gb: f64,
        pub commit_free_gb: f64,
        /// "Ctrl+Alt+M", empty for none.
        pub hotkey: String,
        /// A newer version that can be installed.
        pub update_version: Option<String>,
        pub updating: bool,
        pub run_at_startup: bool,
    }

    /// "Editor + build · memory is fine"
    pub fn status_line(v: &TrayView) -> String {
        format!("{} · memory is {}", mode_words(&v.mode), pressure_words(v.pressure.unwrap_or(Level::Normal)))
    }

    /// `Ather MemGuard\n{Mode} · memory is {fine|tight|critical}\nRAM free {x} GB · commit free {y} GB`,
    /// with the middle line `Paused until {hh:mm}` while paused. At most 127 characters (the
    /// tray's limit).
    pub fn tooltip(v: &TrayView) -> String {
        let middle = if v.paused {
            match &v.paused_until {
                Some(t) => format!("Paused until {}", t),
                None => "Paused until you resume".to_string(),
            }
        } else {
            status_line(v)
        };
        let s = format!("Ather MemGuard\n{}\nRAM free {:.1} GB · commit free {:.1} GB", middle, v.ram_free_gb, v.commit_free_gb);
        s.chars().take(127).collect()
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum MenuEntry {
        /// The disabled status line at the top.
        Status(String),
        Separator,
        Item { label: String, accel: String, cmd: Command, checked: bool, disabled: bool },
    }

    fn item(label: &str, cmd: Command) -> MenuEntry {
        MenuEntry::Item { label: label.to_string(), accel: String::new(), cmd, checked: false, disabled: false }
    }

    /// The right-click menu (docs/UI.md section 3, plus "Run a checkup" from section 6).
    pub fn menu(v: &TrayView) -> Vec<MenuEntry> {
        let mut m = vec![MenuEntry::Status(status_line(v)), MenuEntry::Separator];
        m.push(MenuEntry::Item { label: "Free memory now".into(), accel: v.hotkey.clone(), cmd: Command::FreeMemory, checked: false, disabled: false });
        m.push(item("Empty Windows file cache…", Command::FreeWindowsCache));
        if v.paused {
            m.push(item("Resume", Command::Resume));
        } else {
            m.push(item("Pause for 30 minutes", Command::Pause30));
            m.push(item("Pause until I resume", Command::PauseUntilResumed));
        }
        m.push(MenuEntry::Separator);
        m.push(item("Settings", Command::OpenSettings));
        m.push(item("Memory history", Command::OpenHistory));
        m.push(item("Run a checkup", Command::OpenDoctor));
        m.push(MenuEntry::Separator);
        m.push(match (&v.update_version, v.updating) {
            (Some(ver), true) => MenuEntry::Item { label: format!("Updating to version {}…", ver), accel: String::new(), cmd: Command::InstallUpdate, checked: false, disabled: true },
            (Some(ver), false) => item(&format!("Update to version {}…", ver), Command::InstallUpdate),
            (None, _) => item("Check for updates…", Command::CheckUpdates),
        });
        m.push(MenuEntry::Item { label: "Run at Windows startup".into(), accel: String::new(), cmd: Command::ToggleStartup, checked: v.run_at_startup, disabled: false });
        m.push(item("Quit Ather MemGuard", Command::Quit));
        m
    }

    pub const MOD_ALT: u32 = 0x1;
    pub const MOD_CONTROL: u32 = 0x2;
    pub const MOD_SHIFT: u32 = 0x4;
    pub const MOD_WIN: u32 = 0x8;
    pub const MOD_NOREPEAT: u32 = 0x4000;

    /// The virtual-key code for a key name as the settings window writes it (`settings::key_name`).
    fn key_vk(name: &str) -> Option<u32> {
        let n = name.trim();
        let upper = n.to_ascii_uppercase();
        if upper.len() == 1 {
            let c = upper.as_bytes()[0];
            if c.is_ascii_alphanumeric() {
                return Some(c as u32);
            }
        }
        if let Some(num) = upper.strip_prefix('F').and_then(|d| d.parse::<u32>().ok()) {
            if (1..=24).contains(&num) {
                return Some(0x6F + num);
            }
        }
        Some(match upper.as_str() {
            "SPACE" => 0x20,
            "PAGEUP" => 0x21,
            "PAGEDOWN" => 0x22,
            "END" => 0x23,
            "HOME" => 0x24,
            "LEFT" => 0x25,
            "UP" => 0x26,
            "RIGHT" => 0x27,
            "DOWN" => 0x28,
            "INSERT" => 0x2D,
            "PAUSE" => 0x13,
            _ => return None,
        })
    }

    /// `"Ctrl+Alt+M"` -> (RegisterHotKey modifiers with MOD_NOREPEAT, virtual key). A key needs
    /// Ctrl, Alt or Win unless it is an F-key.
    pub fn parse_hotkey(s: &str) -> Result<(u32, u32), String> {
        let parts: Vec<&str> = s.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
        let Some((key, mods)) = parts.split_last() else {
            return Err("no shortcut set".into());
        };
        let mut m = 0;
        for p in mods {
            m |= match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => MOD_CONTROL,
                "alt" => MOD_ALT,
                "shift" => MOD_SHIFT,
                "win" | "windows" => MOD_WIN,
                _ => return Err(format!("\"{}\" isn't a modifier key", p)),
            };
        }
        let vk = key_vk(key).ok_or_else(|| format!("\"{}\" isn't a key a shortcut can use", key))?;
        let fkey = (0x70..=0x87).contains(&vk);
        if m & (MOD_CONTROL | MOD_ALT | MOD_WIN) == 0 && !fkey {
            return Err(format!("\"{}\" needs Ctrl, Alt or Win", s.trim()));
        }
        Ok((m | MOD_NOREPEAT, vk))
    }

    /// Announce `version` unless it (or a newer one) was announced before.
    pub fn should_announce(version: &str, last_announced: Option<&str>) -> bool {
        last_announced.map_or(true, |last| crate::updater::compare_versions(version, last) == Ordering::Greater)
    }

    /// "2 hours", "40 minutes", "an hour".
    pub fn runs_out_text(hours: f64) -> String {
        if !hours.is_finite() || hours <= 0.0 {
            return "a few minutes".into();
        }
        let min = (hours * 60.0).round() as u64;
        if min < 2 {
            "a minute".into()
        } else if min < 55 {
            format!("{} minutes", min)
        } else if hours < 1.5 {
            "an hour".into()
        } else {
            format!("{} hours", hours.round() as u64)
        }
    }

    /// "15:42": `add_min` minutes after `h`:`m`.
    pub fn clock_plus(h: u32, m: u32, add_min: u64) -> String {
        let total = (h as u64 * 60 + m as u64 + add_min) % (24 * 60);
        format!("{:02}:{:02}", total / 60, total % 60)
    }

    /// "1.9 MB"
    pub fn size_words(bytes: u64) -> String {
        let mb = bytes as f64 / (1024.0 * 1024.0);
        if mb < 0.1 {
            format!("{} KB", (bytes as f64 / 1024.0).ceil() as u64)
        } else {
            format!("{:.1} MB", mb)
        }
    }

    /// Turns engine events into the catalogue's toasts. Remembers whether memory was critical,
    /// so "Memory is fine again" only follows a critical spell.
    #[derive(Debug, Default)]
    pub struct ToastBrain {
        was_critical: bool,
    }

    impl ToastBrain {
        pub fn toast_for(&mut self, ev: &Event, st: &Status) -> Option<Toast> {
            match ev {
                Event::PressureChanged { to: Level::Critical, .. } => {
                    self.was_critical = true;
                    Some(catalog::memory_critical(st.system.ram_free_gb, st.system.commit_free_gb))
                }
                Event::PressureChanged { to: Level::Normal, .. } if self.was_critical => {
                    self.was_critical = false;
                    Some(catalog::memory_fine(st.system.ram_free_gb))
                }
                // Normal to tight shows nothing: the icon dot and the flyout carry it.
                Event::PressureChanged { .. } => None,
                Event::AutoPrimed { reason, trimmed, groups_ram_before_gb } => Some(catalog::auto_primed(reason, *trimmed, *groups_ram_before_gb)),
                Event::EditorRestartHint { pid, committed_gb } => {
                    let uptime = st.ue.processes.iter().find(|p| p.pid == *pid).map_or(0, |p| p.uptime_min);
                    Some(catalog::editor_restart(*committed_gb, uptime))
                }
                Event::EditorGrowing { gb_per_hour, .. } => {
                    Some(catalog::editor_growing(*gb_per_hour, &runs_out_text(st.system.commit_free_gb / gb_per_hour.max(0.01))))
                }
                Event::SettingsError(message) => Some(catalog::settings_problem(message)),
                Event::SettingsReloaded => None,
            }
        }
    }

    /// May the checkup nudge run now? Never with notifications off, once at a time, at most
    /// once per calendar day.
    pub fn should_nudge(notifications: bool, running: bool, last_day: Option<&str>, today: &str) -> bool {
        notifications && !running && last_day != Some(today)
    }

    /// The nudge for a checkup report: (GB that could be freed, the biggest apps Unreal
    /// doesn't need, largest first), or `None` below `threshold_gb`.
    pub fn nudge_for(r: &crate::doctor::Report, threshold_gb: f64) -> Option<(f64, Vec<String>)> {
        use crate::doctor::{FindingKind, Verdict};
        if r.reclaimable_ram_gb < threshold_gb || r.reclaimable_ram_gb <= 0.0 {
            return None;
        }
        let mut apps: Vec<&crate::doctor::Finding> = r.findings.iter().filter(|f| f.kind == FindingKind::App && f.verdict == Verdict::Close).collect();
        apps.sort_by(|a, b| b.ram_mb.cmp(&a.ram_mb));
        Some((r.reclaimable_ram_gb, apps.iter().take(3).map(|f| f.title.clone()).collect()))
    }

    /// May the RAM check run for its toast? Never with notifications or the check off.
    pub fn should_check_ram(notifications: bool, check_ram_speed: bool) -> bool {
        notifications && check_ram_speed
    }

    /// The RAM check's toast, once per situation: `None` when nothing needs attention or when
    /// this `fingerprint` was announced before (`last_announced`). Running below the rated
    /// speed says so with the numbers; otherwise the first problem finding speaks for itself.
    pub fn ram_toast(r: &crate::ramcheck::RamReport, last_announced: Option<&str>) -> Option<Toast> {
        use crate::ramcheck::{needs_attention, Severity, Verdict};
        if !needs_attention(r) || last_announced == Some(r.fingerprint.as_str()) {
            return None;
        }
        if r.verdict == Verdict::BelowRated {
            if let (Some(now), Some(rated)) = (r.current_mhz, r.rated.as_ref()) {
                return Some(catalog::ram_below_rated(now, rated.speed_mhz, &rated.profile_kind));
            }
        }
        let f = r.findings.iter().find(|f| f.severity == Severity::Problem && f.id != "ram:speed")
            .or_else(|| r.findings.iter().find(|f| f.severity == Severity::Problem))?;
        Some(catalog::ram_problem(&f.title, &f.detail))
    }

    /// A value from the `[tray]` table of ui-state.toml.
    pub fn tray_value(text: &str, key: &str) -> Option<toml::Value> {
        text.parse::<toml::Table>().ok()?.get("tray")?.as_table()?.get(key).cloned()
    }

    /// `text` with `[tray] key = value` set; other tables (window placements) are kept.
    pub fn with_tray_value(text: &str, key: &str, value: toml::Value) -> String {
        let mut doc: toml::Table = text.parse().unwrap_or_default();
        let tray = doc.entry("tray").or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !tray.is_table() {
            *tray = toml::Value::Table(toml::Table::new());
        }
        tray.as_table_mut().unwrap().insert(key.to_string(), value);
        toml::to_string(&doc).unwrap_or_default()
    }
}

// ---------------------------------------------------------------- small Win32 helpers

/// Tell running programs that environment variables changed (after a PATH edit).
pub fn broadcast_environment_change() {
    let env = wide("Environment");
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as LPARAM, SMTO_ABORTIFHUNG, 2000, &mut result);
    }
}

/// Start a program with no inherited handles, so it can't hold a caller's output pipe open.
pub fn launch_detached(file: &Path, params: &str, hidden: bool) -> bool {
    let (op, f, p) = (wide("open"), wide(&file.display().to_string()), wide(params));
    let show = if hidden { 0 } else { SW_SHOWNORMAL };
    let r = unsafe { ShellExecuteW(null_mut(), op.as_ptr(), f.as_ptr(), p.as_ptr(), null(), show) };
    r as usize > 32
}

fn shell_open(target: &str) {
    let (op, t) = (wide("open"), wide(target));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), t.as_ptr(), null(), null(), SW_SHOWNORMAL) };
}

fn open_in_notepad(path: &Path) {
    let (op, exe, arg) = (wide("open"), wide("notepad.exe"), wide(&format!("\"{}\"", path.display())));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), exe.as_ptr(), arg.as_ptr(), null(), SW_SHOWNORMAL) };
}

/// Put `text` on the clipboard as Unicode text.
fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let w: Vec<u16> = text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let mut ok = false;
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(mem);
                ok = !SetClipboardData(CF_UNICODETEXT, mem as _).is_null();
            }
            if !ok {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
        ok
    }
}

/// Local time now as (hour, minute).
fn local_hm() -> (u32, u32) {
    let t = win::local_time_string(); // "YYYY-MM-DD HH:MM:SS"
    let n = |a: usize, b: usize| t.get(a..b).and_then(|s| s.parse().ok()).unwrap_or(0);
    (n(11, 13), n(14, 16))
}

/// "15:42": the local time `d` from now.
fn clock_after(d: Duration) -> String {
    let (h, m) = local_hm();
    clock_plus(h, m, d.as_secs().div_ceil(60))
}

/// Today's local date, "2026-11-14".
fn today() -> String {
    win::local_time_string().chars().take(10).collect()
}

/// The day the checkup nudge last showed (ui-state.toml `[tray] last_nudge_day`).
fn last_nudge_day() -> Option<String> {
    tray_state("last_nudge_day").and_then(|v| v.as_str().map(str::to_string))
}

/// ui-state.toml `[tray]` values (first run, last announced update).
fn tray_state(key: &str) -> Option<toml::Value> {
    tray_value(&std::fs::read_to_string(crate::ui::ui_state::file()).unwrap_or_default(), key)
}

fn set_tray_state(key: &str, value: toml::Value) {
    let path = crate::ui::ui_state::file();
    let text = with_tray_value(&std::fs::read_to_string(&path).unwrap_or_default(), key, value);
    let _ = std::fs::create_dir_all(paths::data_dir());
    let tmp = path.with_extension("toml.tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Send a command to the tray loop (from any UI on the tray thread).
pub fn post_command(cmd: Command) {
    let h = TRAY_HWND.load(Ordering::SeqCst);
    if h != 0 {
        unsafe { PostMessageW(h as HWND, WM_RUN_COMMAND, cmd.id() as WPARAM, 0) };
    }
}

// ---------------------------------------------------------------- memory history and checkup windows

/// Memory history for the window. The last 24 hours come from the engine's in-memory
/// history (filled from disk at start), older samples from the daily CSV files, each past
/// day read once. Called on the tray thread.
#[derive(Default)]
struct TrayHistory {
    days: RefCell<crate::history::DayCache>,
}

impl crate::ui::history::HistorySource for TrayHistory {
    fn samples(&self, since: Duration) -> Vec<crate::history::Sample> {
        let mut days = self.days.borrow_mut();
        with_app(|a| a.engine.history().series_with_days(since, &mut days)).unwrap_or_else(|| crate::history::load_since(since))
    }
    fn latest_unix(&self) -> Option<i64> {
        with_app(|a| a.engine.history().latest().map(|s| s.unix)).flatten()
    }
    fn samples_after(&self, after: i64) -> Option<Vec<crate::history::Sample>> {
        with_app(|a| a.engine.history().series_after(after)).flatten()
    }
    fn interval_sec(&self) -> u64 {
        with_app(|a| a.engine.cfg.interval_sec).unwrap_or(10)
    }
    fn keep_days(&self) -> u32 {
        with_app(|a| a.engine.cfg.history.keep_days).unwrap_or(7)
    }
    fn ram_total_gb(&self) -> f64 {
        win::sys_info().ram_total as f64 / (1024.0 * 1024.0 * 1024.0)
    }
    fn folder(&self) -> PathBuf {
        crate::history::history_dir()
    }
}

pub fn open_history_window() {
    flyout::close();
    crate::ui::history::open_history(Box::new(TrayHistory::default()));
}

pub fn open_doctor_window() {
    flyout::close();
    let tray = TRAY_HWND.load(Ordering::SeqCst);
    crate::ui::doctor::open_doctor(Box::new(RealDoctor::new(tray)), Box::new(post_command));
}

/// Write the RAM report page to `path` (atomically, creating the folder).
pub fn write_ram_report(path: &Path, r: &crate::ramcheck::RamReport) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    }
    let tmp = path.with_extension("html.tmp");
    std::fs::write(&tmp, crate::ramcheck::render_html(r)).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {}", path.display(), e)
    })
}

/// "Show me how" on the RAM check's toast and checkup row: write the report page to the data
/// folder now (the scan only reads) and open it in the default browser.
pub fn show_ram_report() {
    let path = paths::data_dir().join(RAM_REPORT_FILE);
    match write_ram_report(&path, crate::ramcheck::check_cached()) {
        Ok(()) => shell_open(&path.display().to_string()),
        Err(e) => {
            crate::log::write(&format!("couldn't write the RAM report: {}", e));
            // The person just clicked: say so even with notifications off.
            toast::show_always(catalog::ram_report_failed(&e));
        }
    }
}

/// "Undo" on the "Checkup done" toast: reverse the last cleanup on a worker thread.
pub fn undo_doctor() {
    let h = TRAY_HWND.load(Ordering::SeqCst);
    if h != 0 {
        spawn_work(h as HWND, || Work::Undone(crate::doctor::undo(true, None)));
    }
}

/// The checkup window's data and actions over the real doctor (docs/DESIGN.md Part F). Runs
/// on the window's worker thread: it touches no UI state, and tells the tray about a finished
/// cleanup by posting it a message.
pub struct RealDoctor {
    /// The last real report, which `apply` acts on.
    last: std::sync::Mutex<Option<crate::doctor::Report>>,
    /// What a fresh scan after the last `apply` says could still be freed.
    after_apply: std::sync::Mutex<Option<f64>>,
    /// The tray window, for the "Checkup done" toast. 0 = no toast, no rescan (tests).
    tray: isize,
}

impl RealDoctor {
    pub fn new(tray: isize) -> RealDoctor {
        RealDoctor { last: std::sync::Mutex::new(None), after_apply: std::sync::Mutex::new(None), tray }
    }

    /// Start from a report built elsewhere (tests).
    pub fn with_report(report: crate::doctor::Report) -> RealDoctor {
        let d = RealDoctor::new(0);
        *d.last.lock().unwrap() = Some(report);
        d
    }

    fn fresh_scan() -> crate::doctor::Report {
        crate::doctor::scan(&crate::doctor::load_config_readonly())
    }

    /// `doctor::apply` on the last report, mapped both ways. No rescan, no toast.
    pub fn apply_mapped(
        &self,
        picks: &[(String, crate::ui::doctor::ActionKind)],
        force: bool,
    ) -> (Vec<crate::ui::doctor::Outcome>, Option<CleanupSummary>) {
        use crate::ui::doctor as vm;
        let report = self.last.lock().unwrap().clone();
        let Some(report) = report else {
            let outs = picks
                .iter()
                .map(|(id, k)| vm::Outcome { finding_id: id.clone(), action: *k, status: vm::OutcomeStatus::Skipped, message: "Check again first".into() })
                .collect();
            return (outs, None);
        };
        let real: Vec<(String, crate::doctor::ActionKind)> = picks.iter().map(|(id, k)| (id.clone(), doctor_map::action_to_real(*k))).collect();
        let outs = crate::doctor::apply(&report, &real, force);
        let summary = doctor_map::cleanup_summary(&report, &outs);
        (outs.iter().map(doctor_map::outcome_view).collect(), summary)
    }
}

impl crate::ui::doctor::DoctorSource for RealDoctor {
    fn scan(&self) -> crate::ui::doctor::Report {
        let r = Self::fresh_scan();
        let view = doctor_map::report_view(&r);
        *self.last.lock().unwrap() = Some(r);
        view
    }

    fn apply(&self, picks: &[(String, crate::ui::doctor::ActionKind)], force: bool) -> Vec<crate::ui::doctor::Outcome> {
        let (outs, summary) = self.apply_mapped(picks, force);
        if self.tray != 0 {
            // Scan again so the window's hero number goes down; later picks act on this one.
            let fresh = Self::fresh_scan();
            *self.after_apply.lock().unwrap() = Some(fresh.reclaimable_ram_gb);
            *self.last.lock().unwrap() = Some(fresh);
            if let Some(s) = summary {
                post_work(self.tray, Work::CheckupDone(s));
            }
        }
        outs
    }

    fn undo_last(&self) -> Vec<crate::ui::doctor::Outcome> {
        crate::doctor::undo(true, None).iter().filter(|o| !o.finding_id.is_empty()).map(doctor_map::outcome_view).collect()
    }

    fn can_undo(&self) -> bool {
        crate::doctor::UndoLog::load().can_undo()
    }

    fn reclaimable_after_apply(&self) -> Option<f64> {
        self.after_apply.lock().unwrap().take()
    }
}

/// What a cleanup did, for the "Checkup done" toast.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanupSummary {
    /// RAM of the apps that were closed or ended, GB.
    pub freed_gb: f64,
    /// Startup entries switched off.
    pub startup_off: usize,
}

/// Mapping between the real doctor's report and the checkup window's view model. Pure.
pub mod doctor_map {
    use super::CleanupSummary;
    use crate::doctor as real;
    use crate::ui::doctor as vm;

    /// Where "How to fix" goes for a THIS PC finding.
    pub fn help_url(id: &str) -> Option<String> {
        match id {
            "sys:pagefile" | "sys:commit" => Some(
                "https://learn.microsoft.com/en-us/troubleshoot/windows-client/performance/how-to-determine-the-appropriate-page-file-size-for-64-bit-versions-of-windows"
                    .into(),
            ),
            _ => None,
        }
    }

    pub fn action_view(k: real::ActionKind) -> vm::ActionKind {
        match k {
            real::ActionKind::Close => vm::ActionKind::Close,
            real::ActionKind::End => vm::ActionKind::End,
            real::ActionKind::DisableStartup => vm::ActionKind::DisableStartup,
            real::ActionKind::HoldBack => vm::ActionKind::HoldBack,
            real::ActionKind::BlockWhileUnreal => vm::ActionKind::BlockWhileUnreal,
            real::ActionKind::LeaveIt => vm::ActionKind::Keep,
        }
    }

    pub fn action_to_real(k: vm::ActionKind) -> real::ActionKind {
        match k {
            vm::ActionKind::Close => real::ActionKind::Close,
            vm::ActionKind::End => real::ActionKind::End,
            vm::ActionKind::DisableStartup => real::ActionKind::DisableStartup,
            vm::ActionKind::HoldBack => real::ActionKind::HoldBack,
            vm::ActionKind::BlockWhileUnreal => real::ActionKind::BlockWhileUnreal,
            vm::ActionKind::Keep => real::ActionKind::LeaveIt,
        }
    }

    fn kind_view(k: real::FindingKind) -> vm::FindingKind {
        match k {
            real::FindingKind::App => vm::FindingKind::App,
            real::FindingKind::Startup => vm::FindingKind::Startup,
            real::FindingKind::Unreal => vm::FindingKind::Unreal,
            real::FindingKind::System => vm::FindingKind::System,
        }
    }

    fn verdict_view(v: real::Verdict) -> vm::Verdict {
        match v {
            real::Verdict::Close => vm::Verdict::Close,
            real::Verdict::HoldBack => vm::Verdict::HoldBack,
            real::Verdict::Keep => vm::Verdict::Keep,
            real::Verdict::Review => vm::Verdict::Review,
            real::Verdict::HeavyTool => vm::Verdict::HeavyTool,
        }
    }

    pub fn finding_view(f: &real::Finding) -> vm::Finding {
        // Machine-wide startup entries (HKLM, the shared Startup folder) need an administrator.
        let needs_admin = f.kind == real::FindingKind::Startup && f.startup.as_ref().is_some_and(|e| !e.location.switchable());
        vm::Finding {
            id: f.id.clone(),
            kind: kind_view(f.kind),
            verdict: verdict_view(f.verdict),
            title: f.title.clone(),
            detail: f.detail.clone(),
            processes: f.processes,
            ram_mb: f.ram_mb,
            committed_mb: f.committed_mb,
            actions: f.actions.iter().map(|a| vm::Action { kind: action_view(a.kind), detail: a.label.clone() }).collect(),
            recommended: f.recommended,
            needs_admin,
            company: f.company.clone(),
            help_url: if f.kind == real::FindingKind::System { help_url(&f.id) } else { None },
            help_command: (f.kind == real::FindingKind::System && real::is_ram_finding(&f.id)).then_some(crate::ui::Command::ShowRamReport),
        }
    }

    pub fn report_view(r: &real::Report) -> vm::Report {
        let memguard = r.findings.iter().find(|f| f.id == "sys:memguard");
        vm::Report {
            time: r.time.clone(),
            system: vm::SystemStatus {
                ram_total_gb: r.system.ram_total_gb,
                ram_free_gb: r.system.ram_free_gb,
                commit_limit_gb: r.system.commit_limit_gb,
                commit_free_gb: r.system.commit_free_gb,
                memguard_running: memguard.map_or(true, |f| !f.title.contains("isn't running")),
                memguard_paused: memguard.is_some_and(|f| f.title.contains("paused")),
            },
            findings: r.findings.iter().map(finding_view).collect(),
            reclaimable_ram_gb: r.reclaimable_ram_gb,
            reclaimable_commit_gb: r.reclaimable_commit_gb,
        }
    }

    pub fn outcome_view(o: &real::Outcome) -> vm::Outcome {
        let status = match o.result {
            real::OutcomeKind::Done | real::OutcomeKind::Undone => vm::OutcomeStatus::Done,
            real::OutcomeKind::StillOpen => vm::OutcomeStatus::StillOpen,
            real::OutcomeKind::Failed => vm::OutcomeStatus::Failed,
            real::OutcomeKind::Refused | real::OutcomeKind::WouldDo => vm::OutcomeStatus::Skipped,
        };
        vm::Outcome { finding_id: o.finding_id.clone(), action: action_view(o.action), status, message: o.message.clone() }
    }

    /// What the "Checkup done" toast says; `None` when nothing was done.
    pub fn cleanup_summary(report: &real::Report, outs: &[real::Outcome]) -> Option<CleanupSummary> {
        let done: Vec<&real::Outcome> = outs.iter().filter(|o| o.result == real::OutcomeKind::Done).collect();
        if done.is_empty() {
            return None;
        }
        let mut freed_ids: Vec<&str> = done
            .iter()
            .filter(|o| matches!(o.action, real::ActionKind::Close | real::ActionKind::End))
            .map(|o| o.finding_id.as_str())
            .collect();
        freed_ids.sort_unstable();
        freed_ids.dedup();
        let freed_mb: u64 = freed_ids.iter().filter_map(|id| report.finding(id)).map(|f| f.ram_mb).sum();
        let startup_off = done.iter().filter(|o| o.action == real::ActionKind::DisableStartup).count();
        Some(CleanupSummary { freed_gb: freed_mb as f64 / 1024.0, startup_off })
    }
}

// ---------------------------------------------------------------- worker results

/// What a worker thread hands back to the tray thread.
enum Work {
    Primed(ops::PrimeReport),
    /// "Empty Windows file cache" finished (or was declined).
    CacheFlushed(ops::CacheFlushReport),
    Checked { manual: bool, result: CheckResult },
    Progress(u32),
    Downloaded(Result<PathBuf, String>),
    /// The checkup window finished a cleanup.
    CheckupDone(CleanupSummary),
    /// "Undo" on the "Checkup done" toast finished.
    Undone(Vec<crate::doctor::Outcome>),
    /// The guard asked a `close_while_unreal` app to close.
    GuardClosed { app: String, outcome: crate::doctor::Outcome },
    /// The checkup that runs when Unreal work starts: (GB to free, top app names) when it is
    /// worth a nudge.
    Nudge(Option<(f64, Vec<String>)>),
    /// The RAM check a few minutes after start.
    RamChecked(Box<crate::ramcheck::RamReport>),
    /// The 60-second disk poll: every volume with its level.
    DiskPolled(Vec<crate::disk::Volume>),
}

fn post_work(hwnd: isize, w: Work) {
    let p = Box::into_raw(Box::new(w));
    if unsafe { PostMessageW(hwnd as HWND, WM_WORK, 0, p as LPARAM) } == 0 {
        drop(unsafe { Box::from_raw(p) });
    }
}

fn spawn_work(hwnd: HWND, f: impl FnOnce() -> Work + Send + 'static) {
    let h = hwnd as isize;
    std::thread::spawn(move || post_work(h, f()));
}

// ---------------------------------------------------------------- the app

struct Updates {
    /// Shared with the settings window ("Version X" row).
    state: Rc<RefCell<UpdateState>>,
    available: Option<Manifest>,
    checking: bool,
    downloading: bool,
    next_auto: Instant,
    toast: ToastId,
}

struct App {
    engine: Engine,
    hwnd: HWND,
    icon: HICON,
    icon_key: (Level, bool),
    last_tick: Option<Instant>,
    /// The latest status, shared with the settings window.
    status: Rc<RefCell<Option<Status>>>,
    exit_event: Option<Handle>,
    /// `AtherMemGuard.exe --settings` signals this to open the settings window.
    settings_event: Option<Handle>,
    taskbar_created: u32,
    brain: ToastBrain,
    /// The hotkey text last tried, and whether it is registered.
    hotkey_keys: Option<String>,
    hotkey_ok: bool,
    hotkey_warned: bool,
    updates: Updates,
    priming: bool,
    /// "Empty Windows file cache" is running (the administrator prompt may be up).
    flushing: bool,
    dblclick_at: Option<Instant>,
    /// Closes `[doctor] close_while_unreal` apps while Unreal works.
    guard: crate::doctor::Guard,
    /// The app the "Closed {app}" toast is about ("Allow this time").
    guard_pending: Option<String>,
    /// Unreal work (editor, build or cook) on the last tick.
    was_active: bool,
    nudge_running: bool,
    /// When the RAM check for its toast runs; `None` once it has (or was skipped).
    ram_check_at: Option<Instant>,
    /// Disk levels already said, for one toast per level change.
    disk_watch: crate::disk::DiskWatch,
    /// When the next disk poll runs, and whether one is running.
    disk_next: Instant,
    disk_polling: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// Worker results that arrived while the app state was busy.
    static PENDING: RefCell<Vec<Work>> = const { RefCell::new(Vec::new()) };
}

/// Runs a closure on the app state. Returns None if the state is busy (a nested message loop
/// from a menu or dialog is running) or already gone.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => None,
    })
}

/// The Ather mark at the tray's icon size: a dot for the pressure level, faded while paused.
fn make_icon(level: Level, paused: bool) -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as usize;
    if paused {
        crate::logo::create_paused_icon(size, level)
    } else {
        crate::logo::create_icon(size, level)
    }
}

fn copy_to(dst: &mut [u16], text: &str) {
    let w: Vec<u16> = text.encode_utf16().collect();
    let n = w.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&w[..n]);
    dst[n] = 0;
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

/// The tray icon's rectangle on screen, for anchoring the flyout.
fn icon_rect(hwnd: HWND) -> Option<crate::ui::geom::Rect> {
    let mut id: NOTIFYICONIDENTIFIER = unsafe { zeroed() };
    id.cbSize = size_of::<NOTIFYICONIDENTIFIER>() as u32;
    id.hWnd = hwnd;
    id.uID = 1;
    let mut rc: RECT = unsafe { zeroed() };
    let hr = unsafe { Shell_NotifyIconGetRect(&id, &mut rc) };
    (hr >= 0 && rc.right > rc.left).then(|| crate::ui::geom::Rect::ltrb(rc.left, rc.top, rc.right, rc.bottom))
}

impl App {
    fn add_icon(&self) {
        let mut nid = notify_data(self.hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = self.icon;
        copy_to(&mut nid.szTip, &self.status.borrow().as_ref().map(|_| tooltip(&self.view())).unwrap_or_else(|| "Ather MemGuard".into()));
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    fn view(&self) -> TrayView {
        let st = self.status.borrow();
        let paused = self.engine.paused;
        TrayView {
            mode: st.as_ref().map_or_else(|| "idle".into(), |s| s.mode.clone()),
            pressure: st.as_ref().map(|s| s.pressure),
            paused,
            paused_until: self.engine.pause_until().map(|t| clock_after(t.saturating_duration_since(Instant::now()))),
            ram_free_gb: st.as_ref().map_or(0.0, |s| s.system.ram_free_gb),
            commit_free_gb: st.as_ref().map_or(0.0, |s| s.system.commit_free_gb),
            hotkey: self.engine.cfg.general.prime_hotkey.trim().to_string(),
            update_version: self.updates.available.as_ref().map(|m| m.version.clone()),
            updating: self.updates.downloading,
            run_at_startup: crate::install::is_run_at_startup(),
        }
    }

    fn update_icon(&mut self) {
        let Some((level, paused)) = self.status.borrow().as_ref().map(|s| (s.pressure, self.engine.paused)) else { return };
        let mut nid = notify_data(self.hwnd);
        nid.uFlags = NIF_TIP | NIF_SHOWTIP;
        copy_to(&mut nid.szTip, &tooltip(&self.view()));
        let old = (self.icon_key != (level, paused)).then(|| {
            let old = self.icon;
            self.icon = make_icon(level, paused);
            self.icon_key = (level, paused);
            nid.uFlags |= NIF_ICON;
            nid.hIcon = self.icon;
            old
        });
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
            if let Some(old) = old {
                DestroyIcon(old);
            }
        }
    }

    fn flyout_data(&self) -> Option<FlyoutData> {
        let st = self.status.borrow();
        let st = st.as_ref()?;
        let mut st = st.clone();
        st.paused = self.engine.paused;
        let growth = st
            .ue
            .processes
            .iter()
            .filter(|p| p.kind == "editor")
            .max_by(|a, b| a.committed_gb.total_cmp(&b.committed_gb))
            .and_then(|p| self.engine.history().editor_growth_gb_per_hour(p.pid, p.created));
        let note = self.engine.paused.then(|| match self.engine.pause_until() {
            Some(t) => format!("Other apps are back to normal until {}.", clock_after(t.saturating_duration_since(Instant::now()))),
            None => "Other apps are back to normal until you resume.".to_string(),
        });
        Some(FlyoutData::new(&st, &self.engine.cfg.pressure, growth, note))
    }

    /// Follow the settings that live outside the engine: notifications and the hotkey.
    fn apply_settings(&mut self) {
        toast::set_enabled(self.engine.cfg.general.notifications);
        let keys = self.engine.cfg.general.prime_hotkey.trim().to_string();
        if self.hotkey_keys.as_deref() == Some(keys.as_str()) {
            return;
        }
        if self.hotkey_ok {
            unsafe { UnregisterHotKey(self.hwnd, HOTKEY_ID) };
            self.hotkey_ok = false;
        }
        self.hotkey_keys = Some(keys.clone());
        if keys.is_empty() {
            return;
        }
        match parse_hotkey(&keys) {
            Ok((mods, vk)) => {
                self.hotkey_ok = unsafe { RegisterHotKey(self.hwnd, HOTKEY_ID, mods, vk) } != 0;
                if self.hotkey_ok {
                    crate::log::write(&format!("shortcut {} frees memory", keys));
                } else {
                    crate::log::write(&format!("shortcut {} is taken by another app", keys));
                    if !self.hotkey_warned {
                        self.hotkey_warned = true;
                        toast::show(catalog::hotkey_taken(&keys));
                    }
                }
            }
            Err(e) => crate::log::write(&format!("shortcut not registered: {}", e)),
        }
    }

    fn tick(&mut self) {
        let st = self.engine.tick();
        let events = self.engine.take_events();
        *self.status.borrow_mut() = Some(st.clone());
        self.update_icon();
        for ev in &events {
            log_event(ev, &st);
            if matches!(ev, Event::SettingsReloaded) {
                self.apply_settings();
                self.guard.set_patterns(crate::doctor::DoctorSettings::load().close_while_unreal);
            }
            if let Some(t) = self.brain.toast_for(ev, &st) {
                toast::show(t);
            }
        }
        if flyout::is_open() {
            if let Some(d) = self.flyout_data() {
                flyout::update(d);
            }
        }
        self.watch_unreal(&st);
        let now = Instant::now();
        if self.ram_check_at.is_some_and(|at| now >= at) {
            self.ram_check_at = None;
            let g = &self.engine.cfg.general;
            if should_check_ram(g.notifications, g.check_ram_speed) {
                spawn_work(self.hwnd, || Work::RamChecked(Box::new(crate::ramcheck::check_cached().clone())));
            }
        }
        // Disks every 60 s, never on the engine tick, and off the tray thread.
        if now >= self.disk_next && !self.disk_polling {
            self.disk_next = now + DISK_POLL;
            if self.engine.cfg.disk().enabled {
                self.disk_polling = true;
                let (cfg, prev) = (self.engine.cfg.clone(), self.disk_watch.clone());
                spawn_work(self.hwnd, move || Work::DiskPolled(crate::disk::read_volumes(&cfg, &prev)));
            } else {
                self.engine.set_disk(None);
            }
        }
        if self.engine.cfg.updates.check_automatically && now >= self.updates.next_auto && !self.updates.checking && !self.updates.downloading {
            self.updates.next_auto = now + UPDATE_EVERY;
            self.start_check(false);
        }
        self.last_tick = Some(now);
    }

    /// The doctor's guard and the checkup nudge, both driven by Unreal work starting.
    fn watch_unreal(&mut self, st: &Status) {
        let active = st.activities.any();
        let started = active && !self.was_active;
        self.was_active = active;
        if !self.engine.paused {
            for ev in self.guard.on_tick(self.engine.last_procs(), active) {
                let crate::doctor::GuardEvent::Close { app, .. } = &ev;
                let app = app.clone();
                spawn_work(self.hwnd, move || Work::GuardClosed { outcome: crate::doctor::guard_close(&ev), app });
            }
        }
        let today = today();
        if started && should_nudge(self.engine.cfg.general.notifications, self.nudge_running, last_nudge_day().as_deref(), &today) {
            self.nudge_running = true;
            spawn_work(self.hwnd, || {
                let threshold = crate::doctor::DoctorSettings::load().nudge_above_gb;
                let r = crate::doctor::scan(&crate::doctor::load_config_readonly());
                Work::Nudge(nudge_for(&r, threshold))
            });
        }
    }

    fn start_check(&mut self, manual: bool) {
        if self.updates.checking {
            return;
        }
        self.updates.checking = true;
        if manual {
            *self.updates.state.borrow_mut() = UpdateState::Checking;
        }
        spawn_work(self.hwnd, move || Work::Checked { manual, result: updater::check() });
    }

    fn start_install(&mut self) {
        if self.updates.downloading {
            return;
        }
        let Some(m) = self.updates.available.clone() else {
            self.start_check(true);
            return;
        };
        self.updates.downloading = true;
        *self.updates.state.borrow_mut() = UpdateState::Downloading { pct: 0 };
        // The person asked for this, so its progress shows even with notifications off.
        self.updates.toast = toast::show_always(catalog::downloading(0));
        crate::log::write(&format!("downloading Ather MemGuard {}", m.version));
        let h = self.hwnd as isize;
        spawn_work(self.hwnd, move || {
            let last = std::cell::Cell::new(0u32);
            let r = updater::download(&m, &|done, total| {
                let pct = if total == 0 { 0 } else { (done.saturating_mul(100) / total).min(100) as u32 };
                if pct != last.get() {
                    last.set(pct);
                    post_work(h, Work::Progress(pct));
                }
            });
            Work::Downloaded(r)
        });
    }

    fn on_work(&mut self, w: Work) {
        match w {
            Work::CacheFlushed(r) => {
                self.flushing = false;
                if r.ok {
                    toast::show(catalog::cache_flushed(r.file_cache_before_gb, r.ram_free_before_gb, r.ram_free_after_gb));
                } else if r.declined {
                    toast::show(catalog::cache_flush_declined());
                } else {
                    let why = r.error.unwrap_or_else(|| "it failed".into());
                    crate::log::write(&format!("empty file cache: {}", why));
                    toast::show(catalog::cache_flush_failed(&why));
                }
                self.tick();
            }
            Work::Primed(r) => {
                self.priming = false;
                self.engine.forget_applied(); // prime set memory priorities of its own
                toast::show(catalog::freed_manual(r.trimmed_processes, r.before.ram_free_gb, r.after.ram_free_gb));
            }
            Work::Checked { manual, result } => {
                self.updates.checking = false;
                match result {
                    CheckResult::Available(m) => {
                        crate::log::write(&format!("update available: {}", m.version));
                        *self.updates.state.borrow_mut() = UpdateState::Available { version: m.version.clone() };
                        let last = tray_state("last_announced_update").and_then(|v| v.as_str().map(str::to_string));
                        if manual || should_announce(&m.version, last.as_deref()) {
                            toast::show(catalog::update_available(&m.version, &m.notes, &size_words(m.size)));
                            set_tray_state("last_announced_update", toml::Value::String(m.version.clone()));
                        }
                        self.updates.available = Some(m);
                    }
                    CheckResult::UpToDate => {
                        let (h, m) = local_hm();
                        *self.updates.state.borrow_mut() = UpdateState::UpToDate { checked: format!("today at {:02}:{:02}", h, m) };
                        self.updates.available = None;
                        if manual {
                            toast::show(catalog::up_to_date(env!("CARGO_PKG_VERSION")));
                        }
                    }
                    CheckResult::Failed(reason) => {
                        crate::log::write(&format!("update check failed: {}", reason));
                        if manual {
                            *self.updates.state.borrow_mut() = UpdateState::Failed(reason.clone());
                            toast::show_always(catalog::check_failed(&reason));
                        }
                    }
                }
            }
            Work::Progress(pct) => {
                *self.updates.state.borrow_mut() = UpdateState::Downloading { pct };
                toast::update_body(self.updates.toast, &catalog::downloading_body(pct));
            }
            Work::CheckupDone(s) => {
                crate::log::write(&format!("checkup: freed about {:.1} GB, {} startup entries off", s.freed_gb, s.startup_off));
                toast::show(catalog::checkup_done(s.freed_gb, s.startup_off));
            }
            Work::Undone(outs) => {
                let problems: Vec<String> = outs
                    .iter()
                    .filter(|o| !matches!(o.result, crate::doctor::OutcomeKind::Undone))
                    .map(|o| o.message.clone())
                    .fold(Vec::new(), |mut v, m| {
                        if !v.contains(&m) {
                            v.push(m);
                        }
                        v
                    });
                // The person just clicked Undo: say how it went even with notifications off.
                toast::show_always(catalog::checkup_undone(&problems));
            }
            Work::GuardClosed { app, outcome } => {
                use crate::doctor::OutcomeKind;
                match outcome.result {
                    OutcomeKind::Done if outcome.message != "Already closed" => {
                        self.guard_pending = Some(app.clone());
                        toast::show(catalog::guard_closed(&app));
                    }
                    OutcomeKind::Done => {}
                    _ => crate::log::write(&format!("doctor guard: {} not closed: {}", app, outcome.message)),
                }
            }
            Work::Nudge(found) => {
                self.nudge_running = false;
                let today = today();
                if let Some((gb, names)) = found {
                    if should_nudge(self.engine.cfg.general.notifications, false, last_nudge_day().as_deref(), &today) {
                        set_tray_state("last_nudge_day", toml::Value::String(today));
                        crate::log::write(&format!("checkup nudge: {:.1} GB could be freed", gb));
                        toast::show(catalog::checkup_nudge(gb, &names));
                    }
                }
            }
            Work::DiskPolled(vols) => {
                self.disk_polling = false;
                let s = self.engine.cfg.disk();
                let cook = self.engine.cfg.prime.cook.disk_gb;
                for (v, from, to) in self.disk_watch.update(&vols, &s) {
                    crate::log::write(&format!("disk: {} {} -> {} ({:.1} GB free)", v.name(), from.label(), to.label(), v.free_gb));
                    if let Some((title, body)) = crate::disk::level_change_words(&v, from, to, cook) {
                        toast::show(catalog::disk_level(&title, &body, to > from, &v.name()));
                    }
                }
                self.engine.set_disk(Some(crate::disk::brief(&vols, &s)));
                if let Some(st) = self.status.borrow_mut().as_mut() {
                    st.disk = Some(crate::disk::brief(&vols, &s));
                }
            }
            Work::RamChecked(r) => {
                let g = &self.engine.cfg.general;
                if should_check_ram(g.notifications, g.check_ram_speed) {
                    let last = tray_state("last_ram_fingerprint").and_then(|v| v.as_str().map(str::to_string));
                    if let Some(t) = ram_toast(&r, last.as_deref()) {
                        set_tray_state("last_ram_fingerprint", toml::Value::String(r.fingerprint.clone()));
                        crate::log::write(&format!("RAM check: {}", r.headline));
                        toast::show(t);
                    }
                }
            }
            Work::Downloaded(r) => {
                let installed = r.and_then(|path| updater::install_staged(&path));
                match installed {
                    Ok(()) => unsafe { PostQuitMessage(0) },
                    Err(e) => {
                        crate::log::write(&format!("update failed: {}", e));
                        self.updates.downloading = false;
                        let v = self.updates.available.as_ref().map(|m| m.version.clone()).unwrap_or_default();
                        *self.updates.state.borrow_mut() = UpdateState::Available { version: v };
                        toast::show_always(catalog::update_failed());
                    }
                }
            }
        }
    }

    fn shutdown(&mut self) {
        unsafe {
            KillTimer(self.hwnd, TIMER_ID);
            if self.hotkey_ok {
                UnregisterHotKey(self.hwnd, HOTKEY_ID);
            }
        }
        self.engine.restore_all();
        let nid = notify_data(self.hwnd);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid);
            DestroyIcon(self.icon);
        }
        crate::log::write("Ather MemGuard stopped, apps restored to normal");
    }
}

fn log_event(ev: &Event, st: &Status) {
    let text = match ev {
        Event::PressureChanged { from, to } => format!(
            "pressure {} -> {}: RAM free {:.1} GB, commit free {:.1} GB, mode {}",
            from.label(),
            to.label(),
            st.system.ram_free_gb,
            st.system.commit_free_gb,
            st.mode
        ),
        Event::AutoPrimed { .. } => return, // the engine logs it
        Event::EditorRestartHint { pid, committed_gb } => format!("editor {} has {:.1} GB committed: restart hint", pid, committed_gb),
        Event::EditorGrowing { pid, gb_per_hour, committed_gb } => format!("editor {} grows {:.1} GB an hour ({:.1} GB committed)", pid, gb_per_hour, committed_gb),
        Event::SettingsReloaded => "settings changed on disk, reloaded".into(),
        Event::SettingsError(_) => return, // the engine logs it
    };
    crate::log::write(&text);
}

// ---------------------------------------------------------------- the dispatcher

/// Carry out a command. The one place every menu item, flyout button, toast button, the prime
/// hotkey and the settings window end up. Runs on the tray thread, never inside a borrow of
/// the app state while a window opens.
fn dispatch(hwnd: HWND, cmd: Command) {
    match cmd {
        Command::FreeMemory => {
            with_app(|a| {
                if a.priming {
                    return;
                }
                a.priming = true;
                let cfg = a.engine.cfg.clone();
                spawn_work(a.hwnd, move || Work::Primed(ops::prime(&cfg, None, false)));
            });
        }
        Command::FreeWindowsCache => {
            with_app(|a| {
                if a.flushing {
                    return;
                }
                a.flushing = true;
                // Not elevated, this starts AtherMemGuard.exe --flush-cache elevated and waits.
                let exe = std::env::current_exe().unwrap_or_else(|_| "AtherMemGuard.exe".into());
                spawn_work(a.hwnd, move || Work::CacheFlushed(ops::flush_cache(&exe, "--flush-cache")));
            });
        }
        Command::Pause30 => {
            with_app(|a| {
                a.engine.pause_for(Duration::from_secs(30 * 60));
                a.tick();
                toast::show(catalog::paused_30(&clock_after(Duration::from_secs(30 * 60))));
            });
        }
        Command::PauseUntilResumed => {
            with_app(|a| {
                a.engine.set_paused(true);
                a.tick();
                toast::show(catalog::paused());
            });
        }
        Command::Resume => {
            with_app(|a| {
                a.engine.set_paused(false);
                a.tick();
                toast::show(catalog::resumed());
            });
        }
        Command::OpenSettings => open_settings_window(),
        Command::OpenHistory => open_history_window(),
        Command::OpenDoctor => open_doctor_window(),
        Command::UndoDoctor => undo_doctor(),
        Command::AllowGuarded => {
            with_app(|a| {
                if let Some(app) = a.guard_pending.take() {
                    crate::log::write(&format!("doctor guard: {} allowed until Unreal stops", app));
                    a.guard.allow(&app);
                }
            });
        }
        Command::CheckUpdates => {
            with_app(|a| a.start_check(true));
        }
        Command::InstallUpdate => {
            with_app(|a| a.start_install());
        }
        Command::ToggleStartup => {
            let on = !crate::install::is_run_at_startup();
            match crate::install::set_run_at_startup(on) {
                Ok(()) => {
                    toast::show(catalog::startup_toggled(on));
                }
                Err(e) => {
                    crate::log::write(&format!("couldn't change run at startup: {}", e));
                    toast::show_always(catalog::save_failed(&e));
                }
            }
        }
        Command::CopyAgentSetup => {
            let fallback = std::env::current_exe().map(|e| e.with_file_name(paths::CLI_EXE)).unwrap_or_else(|_| paths::CLI_EXE.into());
            let text = crate::mcp::agent_setup_text(&crate::mcp::agent_cli_path(fallback));
            if set_clipboard(hwnd, &text) {
                toast::show(catalog::setup_copied());
            } else {
                toast::show_always(catalog::save_failed("The clipboard is busy. Try again."));
            }
        }
        Command::Quit => unsafe { PostQuitMessage(0) },
        Command::OpenReleasesPage => shell_open(RELEASES_URL),
        Command::OpenSettingsFile => open_in_notepad(&paths::config_file()),
        Command::ShowRamReport => show_ram_report(),
    }
}

fn open_settings_window() {
    let Some((status, update)) = with_app(|a| (a.status.clone(), a.updates.state.clone())) else { return };
    flyout::close();
    crate::ui::settings::open_settings(SettingsHost {
        store: Box::new(crate::ui::real_store::RealStore::new()),
        status: Box::new(move || status.borrow().clone()),
        commands: Box::new(post_command),
        app_version: env!("CARGO_PKG_VERSION").into(),
        update_state: Box::new(move || update.borrow().clone()),
    });
}

fn show_menu(hwnd: HWND) {
    let Some(entries) = with_app(|a| menu(&a.view())) else { return };
    flyout::close();
    let id = unsafe {
        let m = CreatePopupMenu();
        for e in &entries {
            match e {
                MenuEntry::Status(text) => {
                    let w = wide(text);
                    AppendMenuW(m, MF_STRING | MF_GRAYED, 0, w.as_ptr());
                }
                MenuEntry::Separator => {
                    AppendMenuW(m, MF_SEPARATOR, 0, null());
                }
                MenuEntry::Item { label, accel, cmd, checked, disabled } => {
                    let text = if accel.is_empty() { label.clone() } else { format!("{}\t{}", label, accel) };
                    let w = wide(&text);
                    let flags = MF_STRING | if *checked { MF_CHECKED } else { 0 } | if *disabled { MF_GRAYED } else { 0 };
                    AppendMenuW(m, flags, cmd.id() as usize, w.as_ptr());
                }
            }
        }
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(m);
        id as u32
    };
    if let Some(cmd) = Command::from_id(id) {
        dispatch(hwnd, cmd);
    }
}

/// Left click (or Enter on the icon): open the flyout, or close it when it is open.
fn on_select(hwnd: HWND, keyboard: bool) {
    let dbl = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64);
    let recent_dblclick = with_app(|a| a.dblclick_at.is_some_and(|t| t.elapsed() < dbl)).unwrap_or(false);
    if recent_dblclick {
        return;
    }
    if flyout::is_open() {
        if !keyboard {
            flyout::close();
        }
        return;
    }
    // A click on the icon while the flyout was open closed it by taking focus: don't reopen.
    if !keyboard && flyout::closed_within(300) {
        return;
    }
    if let Some(d) = with_app(|a| a.flyout_data()).flatten() {
        flyout::show(d, icon_rect(hwnd), Box::new(post_command));
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            let mut open_settings = false;
            let quit = with_app(|a| {
                if a.exit_event.as_ref().is_some_and(win::event_is_set) {
                    return true;
                }
                open_settings = a.settings_event.as_ref().is_some_and(win::event_is_set);
                let due = a.last_tick.map_or(true, |t| t.elapsed().as_secs() >= a.engine.cfg.interval_sec);
                if due {
                    a.tick();
                }
                for w in PENDING.with(|p| std::mem::take(&mut *p.borrow_mut())) {
                    a.on_work(w);
                }
                false
            });
            if quit == Some(true) {
                crate::log::write("asked to stop");
                PostQuitMessage(0);
            } else if open_settings {
                // Outside the app borrow: opening a window runs its own messages.
                dispatch(hwnd, Command::OpenSettings);
            }
            0
        }
        WM_TRAY => {
            match (lparam & 0xFFFF) as u32 {
                NIN_SELECT => on_select(hwnd, false),
                NIN_KEYSELECT => on_select(hwnd, true),
                WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONDBLCLK => {
                    with_app(|a| a.dblclick_at = Some(Instant::now()));
                    flyout::close();
                    dispatch(hwnd, Command::OpenSettings);
                }
                _ => {}
            }
            0
        }
        WM_RUN_COMMAND => {
            if let Some(cmd) = Command::from_id(wparam as u32) {
                dispatch(hwnd, cmd);
            }
            0
        }
        WM_WORK => {
            let w = *Box::from_raw(lparam as *mut Work);
            // If a nested message loop holds the app state, the next timer tick handles it.
            let mut slot = Some(w);
            with_app(|a| a.on_work(slot.take().unwrap()));
            if let Some(w) = slot {
                PENDING.with(|p| p.borrow_mut().push(w));
            }
            0
        }
        WM_HOTKEY => {
            if wparam as i32 == HOTKEY_ID {
                dispatch(hwnd, Command::FreeMemory);
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => {
            // Explorer restarted: put the icon back.
            if with_app(|a| a.taskbar_created == msg && msg != 0) == Some(true) {
                with_app(|a| a.add_icon());
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Run the tray app until Quit or `memguard stop`. Returns at once if a copy is already running.
pub fn run() {
    run_with(false, false);
}

/// Like `run`, started by the updater: says "Updated to Ather MemGuard {v}" once it is up.
pub fn run_after_update() {
    run_with(true, false);
}

/// Like `run`, and open the settings window once the tray is up (`AtherMemGuard.exe --settings`
/// when no copy was running).
pub fn run_and_open_settings() {
    run_with(false, true);
}

/// Ask the running tray to open its settings window. Waits up to `wait` for a copy that is
/// still starting to begin listening. False when no copy answered.
pub fn ask_running_to_open_settings(wait: Duration) -> bool {
    win::allow_any_foreground();
    let until = Instant::now() + wait;
    loop {
        if win::signal_event(paths::settings_event()) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run_with(after_update: bool, open_settings: bool) {
    let Some(_instance) = win::acquire_instance(paths::mutex(), 8000) else { return };
    crate::disk_scan::allow_cache_writes();
    let exit_event = win::create_event(paths::exit_event());
    let settings_event = win::create_event(paths::settings_event());

    unsafe {
        let class = wide("AtherMemGuardTrayWindow");
        let hinstance = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let title = wide("Ather MemGuard");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinstance, null());
        if hwnd.is_null() {
            crate::log::write("could not create the tray window");
            return;
        }
        TRAY_HWND.store(hwnd as isize, Ordering::SeqCst);
        crate::ui::window::allow_dark_menus();

        let created = wide("TaskbarCreated");
        let app = App {
            engine: Engine::new(),
            hwnd,
            icon: make_icon(Level::Normal, false),
            icon_key: (Level::Normal, false),
            last_tick: None,
            status: Rc::new(RefCell::new(None)),
            exit_event,
            settings_event,
            taskbar_created: RegisterWindowMessageW(created.as_ptr()),
            brain: ToastBrain::default(),
            hotkey_keys: None,
            hotkey_ok: false,
            hotkey_warned: false,
            updates: Updates {
                state: Rc::new(RefCell::new(UpdateState::Unknown)),
                available: None,
                checking: false,
                downloading: false,
                next_auto: Instant::now() + FIRST_UPDATE_CHECK,
                toast: ToastId(0),
            },
            priming: false,
            flushing: false,
            dblclick_at: None,
            guard: crate::doctor::Guard::from_settings(),
            guard_pending: None,
            was_active: false,
            nudge_running: false,
            ram_check_at: Some(Instant::now() + RAM_CHECK_AFTER),
            disk_watch: crate::disk::DiskWatch::default(),
            disk_next: Instant::now() + Duration::from_secs(5),
            disk_polling: false,
        };
        app.add_icon();
        crate::log::write(&format!("Ather MemGuard {} started", env!("CARGO_PKG_VERSION")));
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        toast::set_command_handler(Box::new(post_command));
        with_app(|a| {
            a.apply_settings();
            a.tick();
            // The settings problem at start (if any) came before the first tick: show it.
            let first_run = tray_state("first_run_done").and_then(|v| v.as_bool()) != Some(true);
            if after_update {
                toast::show(catalog::updated(env!("CARGO_PKG_VERSION")));
            } else if first_run {
                toast::show(catalog::first_run(a.engine.cfg.general.prime_hotkey.trim()));
            }
            if first_run {
                set_tray_state("first_run_done", toml::Value::Boolean(true));
            }
        });
        SetTimer(hwnd, TIMER_ID, 1000, None);
        if open_settings {
            post_command(Command::OpenSettings);
        }

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        flyout::close();
        toast::dismiss_all();
        with_app(|a| a.shutdown());
        APP.with(|cell| *cell.borrow_mut() = None);
        TRAY_HWND.store(0, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- CLI helpers

pub fn status_text(st: &Status) -> String {
    let mut s = format!(
        "Mode: {}    Pressure: {}{}\n\nRAM: {:.1} GB free of {:.1} GB\nCommit: {:.1} GB free of {:.1} GB\n",
        st.mode,
        st.pressure.label(),
        if st.paused { "    (PAUSED)" } else { "" },
        st.system.ram_free_gb,
        st.system.ram_total_gb,
        st.system.commit_free_gb,
        st.system.commit_limit_gb
    );
    let r = &st.ram;
    if r.in_use_gb > 0.0 {
        let mut parts = vec![("Unreal".to_string(), r.unreal_gb)];
        parts.extend(r.groups.iter().map(|g| (g.name.clone(), g.ram_gb)));
        parts.extend([
            ("other apps".to_string(), r.other_apps_gb),
            ("Windows file cache".to_string(), r.file_cache_gb),
            ("Windows and drivers".to_string(), r.windows_gb),
            ("squeezed by Windows".to_string(), r.compressed_gb),
            ("shared or not itemized".to_string(), r.unattributed_gb),
        ]);
        parts.sort_by(|a, b| b.1.total_cmp(&a.1));
        let list: Vec<String> = parts.iter().filter(|p| p.1 >= 0.05).map(|(n, gb)| format!("{} {:.1}", n, gb)).collect();
        s.push_str(&format!("RAM in use: {:.1} GB = {} (GB)\n", r.in_use_gb, list.join(", ")));
    }
    if !st.ue.processes.is_empty() || st.ue.workers > 0 {
        s.push_str("\nUnreal\n");
        for p in &st.ue.processes {
            s.push_str(&format!(
                "  {} ({}, PID {}): {:.1} GB committed, {:.1} GB RAM, up {} h {} min\n",
                p.name,
                p.kind,
                p.pid,
                p.committed_gb,
                p.ram_gb,
                p.uptime_min / 60,
                p.uptime_min % 60
            ));
        }
        if st.ue.workers > 0 {
            s.push_str(&format!(
                "  {} compile/shader workers, {:.1} GB committed{}\n",
                st.ue.workers,
                st.ue.workers_committed_gb,
                if st.ue.workers_paused > 0 { format!(", {} paused", st.ue.workers_paused) } else { String::new() }
            ));
        }
    }
    s.push_str("\nGroups\n");
    for g in &st.groups {
        s.push_str(&format!(
            "  {}: {} processes, {} MB RAM, {} MB committed{}\n",
            g.name,
            g.processes,
            g.ram_mb,
            g.committed_mb,
            if g.suspended { ", suspended" } else { "" }
        ));
    }
    if !st.advice.is_empty() {
        s.push_str("\nAdvice\n");
        for a in &st.advice {
            s.push_str(&format!("  - {}\n", a));
        }
    }
    s
}

/// Headless loop for `memguard daemon`: same engine, no tray icon, logs to the console.
pub fn run_headless() {
    let Some(_instance) = win::acquire_instance(paths::mutex(), 8000) else {
        eprintln!("Ather MemGuard is already running.");
        return;
    };
    let exit_event = win::create_event(paths::exit_event());
    let mut engine = Engine::new();
    crate::log::write(&format!("Ather MemGuard {} started (headless)", env!("CARGO_PKG_VERSION")));
    loop {
        let st = engine.tick();
        println!(
            "{}  {}  pressure {}  RAM free {:.1} GB  commit free {:.1} GB",
            st.time,
            st.mode,
            st.pressure.label(),
            st.system.ram_free_gb,
            st.system.commit_free_gb
        );
        for _ in 0..engine.cfg.interval_sec {
            if exit_event.as_ref().is_some_and(win::event_is_set) {
                engine.restore_all();
                crate::log::write("Ather MemGuard stopped, apps restored to normal");
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::status_source::sample_status;
    use crate::ui::toast::ToastKind;

    fn view() -> TrayView {
        TrayView {
            mode: "editor+build".into(),
            pressure: Some(Level::Normal),
            paused: false,
            paused_until: None,
            ram_free_gb: 45.1,
            commit_free_gb: 27.64,
            hotkey: "Ctrl+Alt+M".into(),
            update_version: None,
            updating: false,
            run_at_startup: true,
        }
    }

    fn labels(m: &[MenuEntry]) -> Vec<String> {
        m.iter()
            .map(|e| match e {
                MenuEntry::Status(s) => format!("[{}]", s),
                MenuEntry::Separator => "-".into(),
                MenuEntry::Item { label, accel, checked, disabled, .. } => {
                    let mut s = label.clone();
                    if !accel.is_empty() {
                        s += &format!(" <{}>", accel);
                    }
                    if *checked {
                        s = format!("✓ {}", s);
                    }
                    if *disabled {
                        s += " (off)";
                    }
                    s
                }
            })
            .collect()
    }

    #[test]
    fn tooltip_for_each_state() {
        let mut v = view();
        assert_eq!(tooltip(&v), "Ather MemGuard\nEditor + build · memory is fine\nRAM free 45.1 GB · commit free 27.6 GB");
        v.pressure = Some(Level::Elevated);
        v.mode = "idle".into();
        assert_eq!(tooltip(&v), "Ather MemGuard\nIdle · memory is tight\nRAM free 45.1 GB · commit free 27.6 GB");
        v.pressure = Some(Level::Critical);
        assert!(tooltip(&v).contains("Idle · memory is critical"));
        v.paused = true;
        v.paused_until = Some("15:42".into());
        assert_eq!(tooltip(&v), "Ather MemGuard\nPaused until 15:42\nRAM free 45.1 GB · commit free 27.6 GB");
        v.paused_until = None;
        assert!(tooltip(&v).contains("\nPaused until you resume\n"));
        v.mode = "editor+build+cook".repeat(20);
        v.paused = false;
        assert!(tooltip(&v).chars().count() <= 127);
    }

    #[test]
    fn menu_for_each_state() {
        let v = view();
        assert_eq!(
            labels(&menu(&v)),
            [
                "[Editor + build · memory is fine]",
                "-",
                "Free memory now <Ctrl+Alt+M>",
                "Empty Windows file cache…",
                "Pause for 30 minutes",
                "Pause until I resume",
                "-",
                "Settings",
                "Memory history",
                "Run a checkup",
                "-",
                "Check for updates…",
                "✓ Run at Windows startup",
                "Quit Ather MemGuard",
            ]
        );
        let mut p = view();
        p.paused = true;
        p.hotkey.clear();
        p.run_at_startup = false;
        p.update_version = Some("0.2.1".into());
        p.pressure = Some(Level::Critical);
        let l = labels(&menu(&p));
        assert_eq!(l[0], "[Editor + build · memory is critical]");
        assert_eq!(&l[2..6], ["Free memory now", "Empty Windows file cache…", "Resume", "-"]);
        assert!(l.contains(&"Update to version 0.2.1…".to_string()));
        assert!(l.contains(&"Run at Windows startup".to_string()));
        p.updating = true;
        assert!(labels(&menu(&p)).contains(&"Updating to version 0.2.1… (off)".to_string()));

        // Every item carries the command its label promises.
        let cmds: Vec<Command> = menu(&view()).iter().filter_map(|e| if let MenuEntry::Item { cmd, .. } = e { Some(*cmd) } else { None }).collect();
        assert_eq!(
            cmds,
            [
                Command::FreeMemory,
                Command::FreeWindowsCache,
                Command::Pause30,
                Command::PauseUntilResumed,
                Command::OpenSettings,
                Command::OpenHistory,
                Command::OpenDoctor,
                Command::CheckUpdates,
                Command::ToggleStartup,
                Command::Quit
            ]
        );
        let paused: Vec<Command> = menu(&p).iter().filter_map(|e| if let MenuEntry::Item { cmd, .. } = e { Some(*cmd) } else { None }).collect();
        assert!(paused.contains(&Command::Resume) && !paused.contains(&Command::Pause30) && paused.contains(&Command::InstallUpdate));
    }

    #[test]
    fn hotkey_strings() {
        assert_eq!(parse_hotkey("Ctrl+Alt+M"), Ok((MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'M' as u32)));
        assert_eq!(parse_hotkey(" ctrl + shift + f9 "), Ok((MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, 0x78)));
        assert_eq!(parse_hotkey("F12"), Ok((MOD_NOREPEAT, 0x7B)));
        assert_eq!(parse_hotkey("Win+1"), Ok((MOD_WIN | MOD_NOREPEAT, '1' as u32)));
        assert_eq!(parse_hotkey("Alt+PageDown"), Ok((MOD_ALT | MOD_NOREPEAT, 0x22)));
        assert_eq!(parse_hotkey("Ctrl+Alt+m").unwrap().1, 'M' as u32);
        for bad in ["", "M", "Shift+M", "Ctrl+Hyper+M", "Ctrl+Alt+Foo", "Ctrl+", "F25"] {
            assert!(parse_hotkey(bad).is_err(), "{:?}", bad);
        }
        // Everything the settings window can record parses back.
        use crate::ui::settings::hotkey_text;
        use crate::ui::window::Mods;
        let ctrl_alt = Mods { ctrl: true, alt: true, shift: false };
        for vk in (0x30u16..=0x39).chain(0x41..=0x5A).chain(0x70..=0x87).chain([0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x13]) {
            let text = hotkey_text(ctrl_alt, false, vk).unwrap();
            assert_eq!(parse_hotkey(&text), Ok((MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, vk as u32)), "{}", text);
        }
        assert_eq!(parse_hotkey(&hotkey_text(Mods { shift: true, ..Default::default() }, true, 0x31).unwrap()), Ok((MOD_SHIFT | MOD_WIN | MOD_NOREPEAT, 0x31)));
    }

    #[test]
    fn updates_are_announced_once() {
        assert!(should_announce("0.2.1", None));
        assert!(!should_announce("0.2.1", Some("0.2.1")));
        assert!(!should_announce("0.2.1", Some("0.2.1.0")));
        assert!(!should_announce("0.2.0", Some("0.2.1")), "an older one is never announced again");
        assert!(should_announce("0.2.2", Some("0.2.1")));
        assert!(should_announce("0.10.0", Some("0.9.9")));

        // The remembered version lives in ui-state.toml's [tray] table next to window placements.
        let text = crate::ui::ui_state::with_rect("", "settings", crate::ui::geom::Rect::new(1, 2, 900, 760));
        let text = with_tray_value(&text, "last_announced_update", toml::Value::String("0.2.1".into()));
        let text = with_tray_value(&text, "first_run_done", toml::Value::Boolean(true));
        assert_eq!(tray_value(&text, "last_announced_update").and_then(|v| v.as_str().map(str::to_string)).as_deref(), Some("0.2.1"));
        assert_eq!(tray_value(&text, "first_run_done").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(crate::ui::ui_state::parse(&text, "settings"), Some(crate::ui::geom::Rect::new(1, 2, 900, 760)));
        let text = with_tray_value(&text, "last_announced_update", toml::Value::String("0.2.2".into()));
        assert_eq!(tray_value(&text, "last_announced_update").unwrap().as_str(), Some("0.2.2"));
        assert_eq!(tray_value("broken [", "x"), None);
        assert_eq!(tray_value(&with_tray_value("tray = 3", "k", toml::Value::Integer(1)), "k"), Some(toml::Value::Integer(1)));
    }

    #[test]
    fn events_become_catalogue_toasts() {
        let st = sample_status();
        let mut b = ToastBrain::default();
        let ch = |from, to| Event::PressureChanged { from, to };
        assert_eq!(b.toast_for(&ch(Level::Normal, Level::Elevated), &st), None, "tight shows no toast");
        assert_eq!(b.toast_for(&ch(Level::Elevated, Level::Normal), &st), None, "no 'fine again' without a critical spell");
        let crit = b.toast_for(&ch(Level::Elevated, Level::Critical), &st).unwrap();
        assert_eq!((crit.kind, crit.title.as_str()), (ToastKind::Error, "Memory is critical"));
        assert_eq!(crit.actions, vec![("Free memory".to_string(), Command::FreeMemory)]);
        assert_eq!(b.toast_for(&ch(Level::Critical, Level::Elevated), &st), None);
        let fine = b.toast_for(&ch(Level::Elevated, Level::Normal), &st).unwrap();
        assert_eq!((fine.title.as_str(), fine.body.as_str()), ("Memory is fine again", "45.1 GB RAM free."));
        assert_eq!(b.toast_for(&ch(Level::Elevated, Level::Normal), &st), None, "only once");

        let t = b.toast_for(&Event::AutoPrimed { reason: "cook", trimmed: 131, groups_ram_before_gb: 9.8 }, &st).unwrap();
        assert_eq!((t.title.as_str(), t.body.as_str()), ("Freed memory for the cook", "Held back 131 background processes using 9.8 GB."));
        let t = b.toast_for(&Event::EditorRestartHint { pid: 21640, committed_gb: 52.0 }, &st).unwrap();
        assert_eq!((t.kind, t.title.as_str()), (ToastKind::Warning, "Unreal Editor is at 52 GB"));
        assert!(t.body.starts_with("It has been open 0 h 30 min."));
        // 27.6 GB commit free at 8 GB an hour: about 3 hours.
        let t = b.toast_for(&Event::EditorGrowing { pid: 21640, gb_per_hour: 8.0, committed_gb: 43.8 }, &st).unwrap();
        assert_eq!(t.body, "At this rate memory runs out in about 3 hours.");
        assert_eq!(t.actions[0].1, Command::OpenHistory);
        let t = b.toast_for(&Event::SettingsError("settings.toml: bad key".into()), &st).unwrap();
        assert_eq!((t.kind, t.actions[0].1), (ToastKind::Error, Command::OpenSettingsFile));
        assert_eq!(b.toast_for(&Event::SettingsReloaded, &st), None);
    }

    fn real_finding(id: &str, kind: crate::doctor::FindingKind, verdict: crate::doctor::Verdict, title: &str, ram_mb: u64, actions: &[crate::doctor::ActionKind]) -> crate::doctor::Finding {
        crate::doctor::Finding {
            id: id.into(),
            kind,
            verdict,
            title: title.into(),
            detail: "Detail.".into(),
            processes: 2,
            ram_mb,
            committed_mb: ram_mb * 2,
            actions: actions.iter().map(|&a| crate::doctor::Action::new(a, Some("Background"))).collect(),
            recommended: false,
            category: String::new(),
            names: vec![title.into()],
            pids: vec![],
            has_window: false,
            group: None,
            startup: None,
            company: String::new(),
        }
    }

    /// A hand-built report covering every section of the checkup window.
    fn real_report() -> crate::doctor::Report {
        use crate::doctor::{ActionKind as A, FindingKind as K, Verdict as V};
        use crate::startup::{Entry, Location};
        let entry = |location, name: &str| Entry { location, name: name.into(), command: String::new(), exe: name.into(), enabled: true };
        let mut steam = real_finding("app:steam", K::App, V::Close, "Steam", 1270, &[A::Close, A::BlockWhileUnreal]);
        steam.recommended = true;
        let mut mystery = real_finding("app:mystery", K::App, V::Review, "Mystery", 2150, &[A::Close, A::HoldBack, A::LeaveIt]);
        mystery.company = "Contoso".into();
        let mut user_startup = real_finding("startup:hkcu:Steam", K::Startup, V::Close, "Steam", 1270, &[A::DisableStartup]);
        user_startup.startup = Some(entry(Location::HkcuRun, "Steam"));
        user_startup.recommended = true;
        let mut machine_startup = real_finding("startup:hklm:iCUE", K::Startup, V::Close, "iCUE", 560, &[]);
        machine_startup.startup = Some(entry(Location::HklmRun, "iCUE"));
        let mut shared_folder = real_finding("startup:commonfolder:Tool.lnk", K::Startup, V::Close, "Tool", 0, &[]);
        shared_folder.startup = Some(entry(Location::CommonFolder, "Tool.lnk"));
        crate::doctor::Report {
            time: "2026-11-14 14:02:11".into(),
            system: crate::engine::SystemStatus::from(&crate::win::SysInfo::default()),
            unreal_active: true,
            findings: vec![
                steam,
                real_finding("app:spotify", K::App, V::Close, "Spotify", 640, &[A::Close]),
                real_finding("app:widgets", K::App, V::Close, "Widgets", 410, &[A::Close]),
                real_finding("app:icue", K::App, V::Close, "iCUE", 2000, &[A::Close]),
                user_startup,
                machine_startup,
                shared_folder,
                real_finding("app:blender", K::App, V::HeavyTool, "Blender", 2350, &[A::Close]),
                mystery,
                real_finding("app:discord", K::App, V::HoldBack, "Discord", 890, &[A::HoldBack]),
                real_finding("ue:orphans", K::Unreal, V::Close, "3 leftover Unreal helpers", 1230, &[A::End]),
                real_finding("sys:pagefile", K::System, V::Review, "The page file is small for this much RAM", 0, &[]),
                real_finding("sys:git", K::System, V::Review, "25 git processes are running", 0, &[]),
                real_finding("sys:ram", K::System, V::Review, "Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz.", 0, &[]),
                real_finding("sys:ram:microcode", K::System, V::Review, "Update the BIOS before changing memory settings", 0, &[]),
            ],
            reclaimable_ram_gb: 5.5,
            reclaimable_commit_gb: 11.0,
        }
    }

    #[test]
    fn real_report_maps_onto_the_checkup_window() {
        use crate::ui::doctor::{self as vm, ActionKind as VA, SectionKind};
        let r = real_report();
        let v = doctor_map::report_view(&r);
        assert_eq!(v.findings.len(), r.findings.len());
        assert_eq!((v.reclaimable_ram_gb, v.reclaimable_commit_gb), (5.5, 11.0));
        assert!(v.system.memguard_running && !v.system.memguard_paused);
        let f = |id: &str| v.findings.iter().find(|f| f.id == id).unwrap();

        // "Leave it" is the window's Keep, both ways.
        let mystery = f("app:mystery");
        assert_eq!(mystery.actions.iter().map(|a| a.kind).collect::<Vec<_>>(), [VA::Close, VA::HoldBack, VA::Keep]);
        assert_eq!(mystery.company, "Contoso");
        assert_eq!(vm::choices(mystery).iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Close", "Hold back", "Leave it"]);
        for k in [VA::Close, VA::End, VA::DisableStartup, VA::HoldBack, VA::BlockWhileUnreal, VA::Keep] {
            assert_eq!(doctor_map::action_view(doctor_map::action_to_real(k)), k);
        }
        assert_eq!(doctor_map::action_to_real(VA::Keep), crate::doctor::ActionKind::LeaveIt);

        // Machine-wide startup entries need an administrator; the user's own don't.
        assert!(!f("startup:hkcu:Steam").needs_admin);
        assert!(f("startup:hklm:iCUE").needs_admin);
        assert!(f("startup:commonfolder:Tool.lnk").needs_admin);
        assert!(!f("app:steam").needs_admin);

        // THIS PC rows link to help where there is some.
        assert!(f("sys:pagefile").help_url.as_deref().is_some_and(|u| u.starts_with("https://learn.microsoft.com/")));
        assert_eq!(f("sys:git").help_url, None);
        assert_eq!(f("app:steam").help_url, None);
        // The RAM check's rows say "Show me how" and run the same command as its toast.
        for id in ["sys:ram", "sys:ram:microcode"] {
            assert_eq!(f(id).help_command, Some(Command::ShowRamReport), "{}", id);
            assert_eq!(vm::help_link(f(id)), Some("Show me how"));
            assert_eq!(vm::section_of(f(id)), Some(SectionKind::ThisPc));
        }
        assert_eq!(f("sys:pagefile").help_command, None);
        assert_eq!(vm::help_link(f("sys:pagefile")), Some("How to fix"));
        assert_eq!(vm::help_link(f("sys:git")), None);
        assert_eq!(f("app:steam").help_command, None);

        // Every section shows, in order, and the window's pre-checks follow `recommended`.
        let m = vm::DoctorModel::new(v.clone());
        let kinds: Vec<SectionKind> = m.sections().iter().map(|s| s.kind).collect();
        assert_eq!(kinds, SectionKind::ORDER.to_vec());
        let picks = m.picks();
        assert!(picks.contains(&("app:steam".to_string(), VA::Close)));
        assert!(picks.contains(&("startup:hkcu:Steam".to_string(), VA::DisableStartup)));
        assert!(picks.contains(&("app:discord".to_string(), VA::HoldBack)), "held-back rows start checked");
        assert!(!picks.iter().any(|(id, _)| id == "app:spotify" || id == "app:mystery" || id == "app:blender" || id.starts_with("startup:hklm")));
        assert!(!m.all_clear());

        // A paused copy is reported as such.
        let mut paused = r.clone();
        paused.findings.push(real_finding("sys:memguard", crate::doctor::FindingKind::System, crate::doctor::Verdict::Review, "Ather MemGuard is paused", 0, &[]));
        assert!(doctor_map::report_view(&paused).system.memguard_paused);
    }

    #[test]
    fn outcomes_and_cleanup_summary() {
        use crate::doctor::{ActionKind as A, Outcome, OutcomeKind as O};
        use crate::ui::doctor::{ActionKind as VA, OutcomeStatus as S};
        let o = |id: &str, action, result, message: &str| Outcome { finding_id: id.into(), action, result, message: message.into(), undo_id: None };
        let outs = vec![
            o("app:steam", A::Close, O::Done, "Closed"),
            o("app:steam", A::BlockWhileUnreal, O::Done, "Stays closed while Unreal is running"),
            o("app:spotify", A::Close, O::StillOpen, "Still open — it may be asking you to save"),
            o("startup:hkcu:Steam", A::DisableStartup, O::Done, "Switched off"),
            o("app:mystery", A::LeaveIt, O::Done, "Added to the keep list"),
            o("ue:orphans", A::End, O::Refused, "Ending needs --force"),
            o("app:icue", A::Close, O::Failed, "Access denied"),
        ];
        let v: Vec<_> = outs.iter().map(doctor_map::outcome_view).collect();
        assert_eq!(v.iter().map(|o| o.status).collect::<Vec<_>>(), [S::Done, S::Done, S::StillOpen, S::Done, S::Done, S::Skipped, S::Failed]);
        assert_eq!(v[4].action, VA::Keep);
        assert_eq!(v[2].message, "Still open — it may be asking you to save");

        let s = doctor_map::cleanup_summary(&real_report(), &outs).unwrap();
        // Steam (1270 MB) closed; Spotify stayed open, iCUE failed: only Steam counts, once.
        assert!((s.freed_gb - 1270.0 / 1024.0).abs() < 1e-9);
        assert_eq!(s.startup_off, 1);
        assert_eq!(doctor_map::cleanup_summary(&real_report(), &outs[2..3]), None, "nothing done, no toast");
        let t = catalog::checkup_done(s.freed_gb, s.startup_off);
        assert_eq!(t.body, "Freed about 1.2 GB. 1 app won't start with Windows any more.");
    }

    #[test]
    fn real_doctor_without_a_scan_skips_everything() {
        use crate::ui::doctor::{ActionKind as VA, OutcomeStatus};
        let d = RealDoctor::new(0);
        let (outs, summary) = d.apply_mapped(&[("app:steam".into(), VA::Close)], false);
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].status, OutcomeStatus::Skipped);
        assert_eq!(summary, None);
    }

    #[test]
    fn nudge_rules() {
        let r = real_report();
        // 5.5 GB to free, threshold 4: the three biggest "not needed" apps, largest first.
        assert_eq!(nudge_for(&r, 4.0), Some((5.5, vec!["iCUE".to_string(), "Steam".to_string(), "Spotify".to_string()])));
        assert_eq!(nudge_for(&r, 6.0), None);
        let mut empty = r.clone();
        empty.reclaimable_ram_gb = 0.0;
        assert_eq!(nudge_for(&empty, 0.0), None, "nothing to free is never a nudge");
        let t = catalog::checkup_nudge(5.5, &nudge_for(&r, 4.0).unwrap().1);
        assert_eq!((t.title.as_str(), t.body.as_str()), ("5.5 GB is in use by apps Unreal doesn't need", "iCUE, Steam and Spotify."));

        assert!(should_nudge(true, false, None, "2026-11-14"));
        assert!(should_nudge(true, false, Some("2026-11-13"), "2026-11-14"));
        assert!(!should_nudge(true, false, Some("2026-11-14"), "2026-11-14"), "once per calendar day");
        assert!(!should_nudge(false, false, None, "2026-11-14"), "never with notifications off");
        assert!(!should_nudge(true, true, None, "2026-11-14"), "one scan at a time");

        // The day lives in ui-state.toml's [tray] table.
        let text = with_tray_value("", "last_nudge_day", toml::Value::String("2026-11-14".into()));
        assert_eq!(tray_value(&text, "last_nudge_day").unwrap().as_str(), Some("2026-11-14"));
    }

    fn ram(verdict: crate::ramcheck::Verdict, problems: &[(&str, &str, &str)], fingerprint: &str) -> crate::ramcheck::RamReport {
        use crate::ramcheck::{RamFinding, Rated, Severity};
        let mut r = crate::ramcheck::check_from(&crate::smbios::Smbios::default(), "", None, (2026, 11, 14));
        r.verdict = verdict;
        r.current_mhz = Some(4800);
        r.rated = Some(Rated {
            speed_mhz: 6000,
            cl: Some(30),
            timings: None,
            voltage: Some(1.35),
            voltage_is_typical: true,
            profile_kind: "EXPO".into(),
            confidence: "part_number".into(),
        });
        r.findings = problems
            .iter()
            .map(|(id, title, detail)| RamFinding { id: id.to_string(), severity: Severity::Problem, title: title.to_string(), detail: detail.to_string() })
            .collect();
        r.findings.push(RamFinding { id: "ram:cpu".into(), severity: Severity::Caution, title: "Keep SoC voltage at or under 1.3 V".into(), detail: String::new() });
        r.fingerprint = fingerprint.into();
        r
    }

    #[test]
    fn ram_toast_once_per_fingerprint() {
        use crate::ramcheck::Verdict as V;
        let below = ram(V::BelowRated, &[("ram:speed", "Your RAM is running below its rated speed", "4800 MHz now.")], "fp1");
        let t = ram_toast(&below, None).expect("announced the first time");
        assert_eq!(t, catalog::ram_below_rated(4800, 6000, "EXPO"));
        assert_eq!(t.title, "Your RAM is running below its rated speed");
        assert_eq!(t.body, "4800 MHz now, rated 6000 MHz. Turning on EXPO in the BIOS takes about five minutes.");
        assert_eq!(t.actions, vec![("Show me how".to_string(), Command::ShowRamReport)]);
        // Once per fingerprint: the same situation stays quiet, a changed one speaks again.
        assert_eq!(ram_toast(&below, Some("fp1")), None);
        assert!(ram_toast(&below, Some("fp0")).is_some());

        // Speed fine but a problem: that finding's own words.
        let micro = ram(V::AtRated, &[("ram:microcode", "Update the BIOS before changing memory settings", "This CPU runs microcode 0x129.")], "fp2");
        let t = ram_toast(&micro, Some("fp1")).unwrap();
        assert_eq!((t.title.as_str(), t.body.as_str()), ("Update the BIOS before changing memory settings", "This CPU runs microcode 0x129."));
        assert_eq!(t.kind, crate::ui::toast::ToastKind::Warning);
        assert_eq!(t.actions, vec![("Show me how".to_string(), Command::ShowRamReport)]);
        assert_eq!(ram_toast(&micro, Some("fp2")), None);

        // Below rated wins over another problem: the speed toast, with the numbers.
        let both = ram(V::BelowRated, &[("ram:speed", "x", "y"), ("ram:slots", "Both sticks are on the same memory channel", "Move them.")], "fp3");
        assert_eq!(ram_toast(&both, None).unwrap().title, "Your RAM is running below its rated speed");

        // Nothing to act on: no toast, cautions included.
        for v in [V::AtRated, V::NoProfile, V::MixedKit, V::Unknown, V::NotApplicable] {
            assert_eq!(ram_toast(&ram(v, &[], "fp"), None), None, "{:?}", v);
        }

        // Never with notifications or the check off.
        assert!(should_check_ram(true, true));
        assert!(!should_check_ram(false, true));
        assert!(!should_check_ram(true, false));
        assert!(!should_check_ram(false, false));

        // The last announced fingerprint lives in ui-state.toml's [tray] table.
        let text = with_tray_value("", "last_nudge_day", toml::Value::String("2026-11-14".into()));
        let text = with_tray_value(&text, "last_ram_fingerprint", toml::Value::String("fp1".into()));
        assert_eq!(tray_value(&text, "last_ram_fingerprint").unwrap().as_str(), Some("fp1"));
        assert_eq!(tray_value(&text, "last_nudge_day").unwrap().as_str(), Some("2026-11-14"));
    }

    #[test]
    fn ram_report_page_is_written_where_asked() {
        let dir = std::env::temp_dir().join(format!("amg-ramreport-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join(RAM_REPORT_FILE);
        let r = crate::ramcheck::check_from(&crate::smbios::Smbios::default(), "", None, (2026, 11, 14));
        write_ram_report(&path, &r).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, crate::ramcheck::render_html(&r));
        assert!(!path.with_extension("html.tmp").exists());
        // Again over the old page.
        write_ram_report(&path, &r).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_words() {
        assert_eq!(runs_out_text(2.2), "2 hours");
        assert_eq!(runs_out_text(0.66), "40 minutes");
        assert_eq!(runs_out_text(1.1), "an hour");
        assert_eq!(runs_out_text(0.01), "a minute");
        assert_eq!(runs_out_text(f64::INFINITY), "a few minutes");
        assert_eq!(clock_plus(15, 12, 30), "15:42");
        assert_eq!(clock_plus(23, 50, 30), "00:20");
        assert_eq!(size_words(1_992_294), "1.9 MB");
        assert_eq!(size_words(2_000), "2 KB");
    }
}

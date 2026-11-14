//! Tray icon and background loop. One hidden window, a 1-second timer, and the engine.

use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, MessageBoxW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SendMessageTimeoutW, SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage,
    HICON, HWND_BROADCAST, MB_ICONINFORMATION, MB_OK, MF_SEPARATOR, MF_STRING, MSG, SMTO_ABORTIFHUNG,
    SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW,
};

use crate::engine::{Engine, Level, Status};
use crate::win::{self, wide, Handle};
use crate::{ops, paths};

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

const CMD_STATUS: usize = 1;
const CMD_PRIME: usize = 2;
const CMD_PAUSE: usize = 3;
const CMD_SETTINGS: usize = 4;
const CMD_LOG: usize = 5;
const CMD_FOLDER: usize = 6;
const CMD_EXIT: usize = 9;

struct App {
    engine: Engine,
    hwnd: HWND,
    icon: HICON,
    icon_level: Level,
    last_tick: Option<Instant>,
    status: Option<Status>,
    exit_event: Option<Handle>,
    taskbar_created: u32,
    hinted_restart: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Tell running programs that environment variables changed (after a PATH edit).
pub fn broadcast_environment_change() {
    let env = wide("Environment");
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as LPARAM, SMTO_ABORTIFHUNG, 2000, &mut result);
    }
}

/// 32x32 icon: a memory chip whose colour shows the pressure level.
fn make_icon(level: Level) -> HICON {
    let (r, g, b) = match level {
        Level::Normal => (46u8, 160u8, 90u8),
        Level::Elevated => (230, 160, 30),
        Level::Critical => (215, 55, 55),
    };
    let mut px = vec![0u8; 32 * 32 * 4];
    let mut put = |x: usize, y: usize, c: (u8, u8, u8)| {
        let i = (y * 32 + x) * 4;
        px[i] = c.2;
        px[i + 1] = c.1;
        px[i + 2] = c.0;
        px[i + 3] = 255;
    };
    for y in 6..26 {
        for x in 4..28 {
            put(x, y, (r, g, b));
        }
    }
    for x in [7usize, 13, 19, 25] {
        for y in (2..6).chain(26..30) {
            put(x, y, (r, g, b));
            put(x - 1, y, (r, g, b));
        }
    }
    for (x0, top) in [(8usize, 10usize), (14, 14), (20, 18)] {
        for y in top..22 {
            for x in x0..x0 + 4 {
                put(x, y, (255, 255, 255));
            }
        }
    }
    let and_mask = vec![0u8; 32 * 32 / 8];
    unsafe { CreateIcon(GetModuleHandleW(null()), 32, 32, 1, 32, and_mask.as_ptr(), px.as_ptr()) }
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

fn tooltip(st: &Status) -> String {
    format!(
        "MemCap{}: {}, pressure {}\nRAM free {:.1} GB, commit free {:.1} GB",
        if st.paused { " (paused)" } else { "" },
        st.mode,
        st.pressure.label(),
        st.system.ram_free_gb,
        st.system.commit_free_gb
    )
}

impl App {
    fn add_icon(&self) {
        let mut nid = notify_data(self.hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = self.icon;
        copy_to(&mut nid.szTip, "MemCap");
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    }

    fn update_icon(&mut self, st: &Status) {
        let mut nid = notify_data(self.hwnd);
        nid.uFlags = NIF_TIP;
        copy_to(&mut nid.szTip, &tooltip(st));
        if st.pressure != self.icon_level {
            let old = self.icon;
            self.icon = make_icon(st.pressure);
            self.icon_level = st.pressure;
            nid.uFlags |= NIF_ICON;
            nid.hIcon = self.icon;
            unsafe {
                Shell_NotifyIconW(NIM_MODIFY, &nid);
                DestroyIcon(old);
            }
        } else {
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        }
    }

    fn balloon(&self, title: &str, text: &str, warning: bool) {
        let mut nid = notify_data(self.hwnd);
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        copy_to(&mut nid.szInfoTitle, title);
        copy_to(&mut nid.szInfo, text);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }

    fn tick(&mut self) {
        let previous = self.status.as_ref().map(|s| s.pressure);
        let st = self.engine.tick();
        self.update_icon(&st);
        if st.pressure == Level::Critical && previous != Some(Level::Critical) {
            self.balloon(
                "Memory is critical",
                &format!(
                    "RAM free {:.1} GB, commit free {:.1} GB. Background apps are being trimmed.",
                    st.system.ram_free_gb, st.system.commit_free_gb
                ),
                true,
            );
            crate::log::write(&format!(
                "pressure critical: RAM free {:.1} GB, commit free {:.1} GB, mode {}",
                st.system.ram_free_gb, st.system.commit_free_gb, st.mode
            ));
        } else if previous.is_some() && previous != Some(st.pressure) {
            crate::log::write(&format!("pressure {} (mode {})", st.pressure.label(), st.mode));
        }
        let restart_hint = st.advice.iter().find(|a| a.contains("Restart it"));
        match restart_hint {
            Some(hint) if !self.hinted_restart => {
                self.balloon("Unreal Editor is using a lot of memory", hint, false);
                self.hinted_restart = true;
            }
            None => self.hinted_restart = false,
            _ => {}
        }
        self.status = Some(st);
        self.last_tick = Some(Instant::now());
    }

    fn shutdown(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER_ID) };
        self.engine.restore_all();
        let nid = notify_data(self.hwnd);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid);
            DestroyIcon(self.icon);
        }
        crate::log::write("MemCap stopped, apps restored to normal");
    }
}

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

fn message_box(hwnd: HWND, text: &str) {
    let (t, c) = (wide(text), wide("MemCap"));
    unsafe { MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION) };
}

fn open_in_notepad(path: &std::path::Path) {
    let (op, exe, arg) = (wide("open"), wide("notepad.exe"), wide(&format!("\"{}\"", path.display())));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), exe.as_ptr(), arg.as_ptr(), null(), SW_SHOWNORMAL) };
}

/// Start a program with no inherited handles, so it can't hold a caller's output pipe open.
pub fn launch_detached(file: &std::path::Path, params: &str, hidden: bool) -> bool {
    let (op, f, p) = (wide("open"), wide(&file.display().to_string()), wide(params));
    let show = if hidden { 0 } else { SW_SHOWNORMAL };
    let r = unsafe { ShellExecuteW(null_mut(), op.as_ptr(), f.as_ptr(), p.as_ptr(), null(), show) };
    r as usize > 32
}

fn open_folder(path: &std::path::Path) {
    let (op, dir) = (wide("open"), wide(&path.display().to_string()));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), dir.as_ptr(), null(), null(), SW_SHOWNORMAL) };
}

/// Runs a closure on the app state. Returns None if the state is busy (a nested
/// message loop from a menu or dialog is running) or already gone.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => None,
    })
}

fn show_menu(hwnd: HWND) {
    let paused = with_app(|a| a.engine.paused).unwrap_or(false);
    let cmd = unsafe {
        let menu = CreatePopupMenu();
        let add = |id: usize, text: &str| {
            let w = wide(text);
            AppendMenuW(menu, MF_STRING, id, w.as_ptr());
        };
        add(CMD_STATUS, "Status");
        add(CMD_PRIME, "Prime now (free RAM for Unreal)");
        add(CMD_PAUSE, if paused { "Resume" } else { "Pause" });
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        add(CMD_SETTINGS, "Edit settings");
        add(CMD_LOG, "Open log");
        add(CMD_FOLDER, "Open data folder");
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        add(CMD_EXIT, "Exit");
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
        DestroyMenu(menu);
        cmd as usize
    };
    run_command(hwnd, cmd);
}

fn run_command(hwnd: HWND, cmd: usize) {
    match cmd {
        CMD_STATUS => {
            // Take the text out first: the dialog runs its own message loop.
            let text = with_app(|a| {
                a.tick();
                a.status.as_ref().map(status_text)
            })
            .flatten();
            if let Some(text) = text {
                message_box(hwnd, &text);
            }
        }
        CMD_PRIME => {
            let cfg = with_app(|a| a.engine.cfg.clone());
            if let Some(cfg) = cfg {
                let report = ops::prime(&cfg, None, false);
                with_app(|a| {
                    a.balloon(
                        "Primed for Unreal",
                        &format!(
                            "Trimmed {} processes. RAM free {:.1} GB -> {:.1} GB.",
                            report.trimmed_processes, report.before.ram_free_gb, report.after.ram_free_gb
                        ),
                        false,
                    );
                    a.tick();
                });
            }
        }
        CMD_PAUSE => {
            with_app(|a| {
                let paused = !a.engine.paused;
                a.engine.set_paused(paused);
                a.tick();
            });
        }
        CMD_SETTINGS => open_in_notepad(&paths::config_file()),
        CMD_LOG => open_in_notepad(&paths::log_file()),
        CMD_FOLDER => open_folder(&paths::data_dir()),
        CMD_EXIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            let quit = with_app(|a| {
                if a.exit_event.as_ref().is_some_and(win::event_is_set) {
                    return true;
                }
                let due = a.last_tick.map_or(true, |t| t.elapsed().as_secs() >= a.engine.cfg.interval_sec);
                if due {
                    a.tick();
                }
                false
            });
            if quit == Some(true) {
                PostQuitMessage(0);
            }
            0
        }
        WM_TRAY => {
            match lparam as u32 {
                WM_RBUTTONUP => show_menu(hwnd),
                WM_LBUTTONDBLCLK => run_command(hwnd, CMD_STATUS),
                _ => {}
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

/// Run the tray app until Exit or `memcap stop`. Returns at once if a copy is already running.
pub fn run() {
    let Some(_instance) = win::acquire_instance(paths::mutex(), 8000) else { return };
    let exit_event = win::create_event(paths::exit_event());

    unsafe {
        let class = wide("MemCapTrayWindow");
        let hinstance = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let title = wide("MemCap");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinstance, null());
        if hwnd.is_null() {
            crate::log::write("could not create the tray window");
            return;
        }

        let created = wide("TaskbarCreated");
        let app = App {
            engine: Engine::new(),
            hwnd,
            icon: make_icon(Level::Normal),
            icon_level: Level::Normal,
            last_tick: None,
            status: None,
            exit_event,
            taskbar_created: RegisterWindowMessageW(created.as_ptr()),
            hinted_restart: false,
        };
        app.add_icon();
        crate::log::write(&format!("MemCap {} started", env!("CARGO_PKG_VERSION")));
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        with_app(|a| a.tick());
        SetTimer(hwnd, TIMER_ID, 1000, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        with_app(|a| a.shutdown());
        APP.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Headless loop for `memcap daemon`: same engine, no tray icon, logs to the console.
pub fn run_headless() {
    let Some(_instance) = win::acquire_instance(paths::mutex(), 8000) else {
        eprintln!("MemCap is already running.");
        return;
    };
    let exit_event = win::create_event(paths::exit_event());
    let mut engine = Engine::new();
    crate::log::write(&format!("MemCap {} started (headless)", env!("CARGO_PKG_VERSION")));
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
                crate::log::write("MemCap stopped, apps restored to normal");
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

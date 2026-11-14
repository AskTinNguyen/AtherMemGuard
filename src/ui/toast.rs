//! Toasts: custom, non-activating, layered, topmost popups in the bottom-right corner of the
//! primary work area (docs/UI.md section 2).
//!
//! - `Stack` is the pure model: stacking, eviction (at most three, the oldest leaves early),
//!   timing (6 s, 15 s with buttons, hover pauses), enter/leave animation phases, `key`
//!   replacement, in-place body updates. It takes explicit times so tests can drive it.
//! - `paint` / `height` draw one toast into a `Canvas` (windows and snapshots share them).
//! - `show`, `update_body`, ... drive real windows on the calling thread, which must run a
//!   message loop (the tray thread). Commands go to the handler set with `set_command_handler`.
//! - `catalog` builds every toast in the catalogue, so wording lives in one place.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::canvas::{Align, Canvas};
use super::commands::Command;
use super::geom::{Pt, Rect};
use super::kit::{Button, ButtonStyle, WState};
use super::theme::*;
use crate::win::wide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub body: String,
    pub actions: Vec<(String, Command)>,
    pub on_click: Option<Command>,
    /// A toast with the same key replaces the one already showing.
    pub key: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToastId(pub u64);

pub const WIDTH: f32 = 360.0;
pub const MARGIN: f32 = 16.0;
pub const GAP: f32 = 12.0;
pub const MAX_VISIBLE: usize = 3;
pub const ENTER_MS: u64 = 150;
pub const LEAVE_MS: u64 = 200;
pub const DEFAULT_MS: u64 = 6000;
pub const WITH_BUTTONS_MS: u64 = 15000;
/// Fully shown opacity (a touch translucent, like AtherScreenshot's toast).
pub const OPAQUE: u8 = 248;

impl Toast {
    /// How long it stays up when not hovered.
    pub fn duration_ms(&self) -> u64 {
        if self.actions.is_empty() {
            DEFAULT_MS
        } else {
            WITH_BUTTONS_MS
        }
    }
    pub fn edge(&self) -> Rgb {
        match self.kind {
            ToastKind::Info => ACCENT,
            ToastKind::Warning => WARN,
            ToastKind::Error => ERROR,
        }
    }
}

// ---------------------------------------------------------------- model

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Entering { since: u64 },
    Shown,
    Leaving { since: u64 },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: ToastId,
    pub toast: Toast,
    pub phase: Phase,
    /// Milliseconds left before it starts leaving.
    pub remaining_ms: u64,
    pub hovered: bool,
}

impl Entry {
    pub fn leaving(&self) -> bool {
        matches!(self.phase, Phase::Leaving { .. })
    }
    /// 0..1: how far in it is (opacity and slide).
    pub fn presence(&self, now: u64) -> f32 {
        match self.phase {
            Phase::Entering { since } => (now.saturating_sub(since) as f32 / ENTER_MS as f32).min(1.0),
            Phase::Shown => 1.0,
            Phase::Leaving { since } => 1.0 - (now.saturating_sub(since) as f32 / LEAVE_MS as f32).min(1.0),
        }
    }
}

/// The toasts on screen, oldest first (the oldest sits at the bottom).
#[derive(Debug, Default)]
pub struct Stack {
    pub entries: Vec<Entry>,
    next_id: u64,
    last_tick: Option<u64>,
}

impl Stack {
    pub fn new() -> Stack {
        Stack::default()
    }

    /// Show `toast` at time `now` (ms). Same `key` as a toast still showing replaces its
    /// content in place, restarts its timer and keeps its id.
    pub fn show(&mut self, toast: Toast, now: u64) -> ToastId {
        self.tick(now);
        if let Some(key) = toast.key {
            if let Some(e) = self.entries.iter_mut().find(|e| !e.leaving() && e.toast.key == Some(key)) {
                e.remaining_ms = toast.duration_ms();
                e.toast = toast;
                return e.id;
            }
        }
        self.next_id += 1;
        let id = ToastId(self.next_id);
        self.entries.push(Entry { id, remaining_ms: toast.duration_ms(), toast, phase: Phase::Entering { since: now }, hovered: false });
        // At most three: the oldest ones leave early.
        while self.entries.iter().filter(|e| !e.leaving()).count() > MAX_VISIBLE {
            if let Some(old) = self.entries.iter_mut().find(|e| !e.leaving()) {
                old.phase = Phase::Leaving { since: now };
            }
        }
        id
    }

    /// Replace the body of toast `id` if it is still up; keeps it up for another full duration.
    pub fn update_body(&mut self, id: ToastId, body: &str) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id && !e.leaving()) {
            Some(e) => {
                e.toast.body = body.to_string();
                e.remaining_ms = e.remaining_ms.max(e.toast.duration_ms());
                true
            }
            None => false,
        }
    }

    pub fn set_hover(&mut self, id: ToastId, hovered: bool) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.hovered = hovered;
        }
    }

    /// Start the leave animation of `id`.
    pub fn dismiss(&mut self, id: ToastId, now: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id && !e.leaving()) {
            e.phase = Phase::Leaving { since: now };
        }
    }

    pub fn dismiss_all(&mut self, now: u64) {
        for e in self.entries.iter_mut().filter(|e| !e.leaving()) {
            e.phase = Phase::Leaving { since: now };
        }
    }

    /// Advance to `now`: count down unhovered toasts, finish animations. Returns the ids removed.
    pub fn tick(&mut self, now: u64) -> Vec<ToastId> {
        let dt = now.saturating_sub(self.last_tick.unwrap_or(now));
        self.last_tick = Some(now);
        for e in &mut self.entries {
            if let Phase::Entering { since } = e.phase {
                if now >= since + ENTER_MS {
                    e.phase = Phase::Shown;
                }
            }
            if !e.leaving() && !e.hovered {
                e.remaining_ms = e.remaining_ms.saturating_sub(dt);
                if e.remaining_ms == 0 {
                    e.phase = Phase::Leaving { since: now };
                }
            }
        }
        let mut removed = Vec::new();
        self.entries.retain(|e| match e.phase {
            Phase::Leaving { since } if now >= since + LEAVE_MS => {
                removed.push(e.id);
                false
            }
            _ => true,
        });
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Top edges for toasts of `heights` (oldest first) stacked upwards from the bottom of `work`.
pub fn stack_positions(heights: &[i32], work_bottom: i32, margin: i32, gap: i32) -> Vec<i32> {
    let mut y = work_bottom - margin;
    heights
        .iter()
        .map(|h| {
            y -= h;
            let top = y;
            y -= gap;
            top
        })
        .collect()
}

// ---------------------------------------------------------------- painting

struct ToastLayout {
    title: Vec<String>,
    body: Vec<String>,
    buttons: Vec<Rect>,
    height: i32,
}

fn layout(c: &Canvas, t: &Toast) -> ToastLayout {
    let w = c.s(WIDTH);
    let inner = w - c.s(3.0) - c.s(16.0) * 2;
    let title = c.wrap_lines(&t.title.to_uppercase(), F_TOAST_TITLE, inner, 2);
    let body = if t.body.is_empty() { Vec::new() } else { c.wrap_lines(&t.body, F_VALUE, inner, 4) };
    let mut y = c.s(14.0) + title.len() as i32 * c.line_height(F_TOAST_TITLE);
    if !body.is_empty() {
        y += c.s(4.0) + body.len() as i32 * c.line_height(F_VALUE);
    }
    let mut buttons = Vec::new();
    if !t.actions.is_empty() {
        y += c.s(10.0);
        let mut x = c.s(3.0) + c.s(16.0);
        let mut bh = 0;
        for (i, (label, _)) in t.actions.iter().enumerate() {
            let (bw, h) = button(t, i, label).size(c);
            buttons.push(Rect::new(x, y, bw, h));
            x += bw + c.s(8.0);
            bh = h;
        }
        y += bh;
    }
    ToastLayout { title, body, buttons, height: y + c.s(14.0) }
}

fn button(t: &Toast, i: usize, label: &str) -> Button {
    // The first button of an error toast is the thing to do now.
    let style = if i == 0 && t.kind == ToastKind::Error { ButtonStyle::Primary } else { ButtonStyle::Normal };
    Button::small(label, style)
}

/// Height of a toast at the canvas's scale.
pub fn height(c: &Canvas, t: &Toast) -> i32 {
    layout(c, t).height
}

/// The action button under `p`, if any.
pub fn hit_button(c: &Canvas, t: &Toast, p: Pt) -> Option<usize> {
    layout(c, t).buttons.iter().position(|r| r.contains(p))
}

/// Paint a toast filling the canvas (sized `S(360)` x `height`).
pub fn paint(c: &mut Canvas, t: &Toast, mouse: Option<Pt>) {
    let l = layout(c, t);
    let full = c.bounds();
    c.fill_rect(full, SURFACE);
    c.fill_rect(Rect::new(0, 0, c.s(3.0).max(2), full.h), t.edge());
    let x = c.s(3.0) + c.s(16.0);
    let w = full.w - x - c.s(16.0);
    let mut y = c.s(14.0);
    let th = c.line_height(F_TOAST_TITLE);
    for line in &l.title {
        c.text_top(Rect::new(x, y, w, th), line, F_TOAST_TITLE, TEXT, Align::Left);
        y += th;
    }
    if !l.body.is_empty() {
        y += c.s(4.0);
        let bh = c.line_height(F_VALUE);
        for line in &l.body {
            c.text_top(Rect::new(x, y, w, bh), line, F_VALUE, TEXT_DIM, Align::Left);
            y += bh;
        }
    }
    for (i, r) in l.buttons.iter().enumerate() {
        let hover = mouse.map(|m| r.contains(m)).unwrap_or(false);
        button(t, i, &t.actions[i].0).paint(c, *r, &WState { hover, ..Default::default() });
    }
}

/// A canvas with the toast painted on it (snapshots).
pub fn render(t: &Toast, scale: Scale) -> Canvas {
    let probe = Canvas::new(1, 1, scale);
    let h = height(&probe, t);
    drop(probe);
    let mut c = Canvas::new(scale.s(WIDTH), h, scale);
    paint(&mut c, t, None);
    c
}

// ---------------------------------------------------------------- windows

const CLASS: &str = "AtherMemGuardToast";
const HOST_CLASS: &str = "AtherMemGuardToastHost";
const FRAME_TIMER: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

struct ToastWin {
    hwnd: HWND,
    canvas: Canvas,
    y: f32,
    mouse: Option<Pt>,
    tracking: bool,
}

struct Ui {
    stack: Stack,
    wins: HashMap<ToastId, ToastWin>,
    host: HWND,
    start: Instant,
    enabled: bool,
    handler: Option<Box<dyn Fn(Command)>>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn now_ms(ui: &Ui) -> u64 {
    ui.start.elapsed().as_millis() as u64
}

/// Where the commands of toast buttons and clicks go (the tray loop).
pub fn set_command_handler(handler: Box<dyn Fn(Command)>) {
    with_ui(|ui| ui.handler = Some(handler));
}

/// Mirror `general.notifications`: when off, `show` skips everything; use `show_always`
/// for errors caused by something the person just did.
pub fn set_enabled(on: bool) {
    with_ui(|ui| ui.enabled = on);
}

/// Show a toast unless notifications are off. Returns its id (also when skipped, so
/// `update_body` on it is harmless).
pub fn show(toast: Toast) -> ToastId {
    let enabled = with_ui(|ui| ui.enabled);
    if !enabled {
        return ToastId(0);
    }
    show_always(toast)
}

/// Show a toast even when notifications are off.
pub fn show_always(toast: Toast) -> ToastId {
    let id = with_ui(|ui| {
        let now = now_ms(ui);
        ui.stack.show(toast, now)
    });
    sync();
    id
}

/// Update the body of a toast that is still showing (download progress).
pub fn update_body(id: ToastId, body: &str) {
    let changed = with_ui(|ui| ui.stack.update_body(id, body));
    if changed {
        sync();
    }
}

/// Close every toast.
pub fn dismiss_all() {
    with_ui(|ui| {
        let now = now_ms(ui);
        ui.stack.dismiss_all(now)
    });
    sync();
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> R {
    UI.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Ui { stack: Stack::new(), wins: HashMap::new(), host: null_mut(), start: Instant::now(), enabled: true, handler: None });
        }
        f(slot.as_mut().unwrap())
    })
}

fn register() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let hinst = GetModuleHandleW(null());
        let procs: [(&str, WNDPROC); 2] = [(CLASS, Some(toast_proc)), (HOST_CLASS, Some(host_proc))];
        for (name, proc_) in procs {
            let n = wide(name);
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: proc_,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_HAND),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: n.as_ptr(),
            };
            RegisterClassW(&wc);
        }
    });
}

/// The primary monitor's work area and DPI scale.
fn primary_work() -> (Rect, Scale) {
    unsafe {
        let mut rc: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rc as *mut RECT as *mut _, 0);
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) < 0 {
            dx = 96;
        }
        (Rect::ltrb(rc.left, rc.top, rc.right, rc.bottom), Scale::from_dpi(dx))
    }
}

fn find_id(hwnd: HWND) -> Option<ToastId> {
    with_ui(|ui| ui.wins.iter().find(|(_, w)| w.hwnd == hwnd).map(|(id, _)| *id))
}

/// Bring the windows in line with the model: create, move, fade, destroy.
fn sync() {
    register();
    let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let (work, scale) = primary_work();
    let mut destroy: Vec<HWND> = Vec::new();
    let mut moves: Vec<(HWND, i32, i32, i32, i32, u8, bool)> = Vec::new();
    let animating = with_ui(|ui| unsafe {
        if ui.host.is_null() {
            let n = wide(HOST_CLASS);
            ui.host = CreateWindowExW(0, n.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), GetModuleHandleW(null()), null());
        }
        let now = now_ms(ui);
        for id in ui.stack.tick(now) {
            if let Some(w) = ui.wins.remove(&id) {
                destroy.push(w.hwnd);
            }
        }
        // Heights at the current scale; create missing windows.
        let mut heights = Vec::new();
        for e in &ui.stack.entries {
            let w = ui.wins.entry(e.id).or_insert_with(|| {
                let n = wide(CLASS);
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                    n.as_ptr(),
                    null(),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                let pref: i32 = 3; // DWMWCP_ROUNDSMALL
                DwmSetWindowAttribute(hwnd, 33 /* DWMWA_WINDOW_CORNER_PREFERENCE */, &pref as *const i32 as *const _, 4);
                let off: i32 = 1;
                DwmSetWindowAttribute(hwnd, 3 /* DWMWA_TRANSITIONS_FORCEDISABLED */, &off as *const i32 as *const _, 4);
                ToastWin { hwnd, canvas: Canvas::new(1, 1, scale), y: f32::NAN, mouse: None, tracking: false }
            });
            w.canvas.set_scale(scale);
            let h = height(&w.canvas, &e.toast);
            if w.canvas.height() != h || w.canvas.width() != scale.s(WIDTH) {
                w.canvas.resize(scale.s(WIDTH), h);
            }
            heights.push(h);
        }
        let tops = stack_positions(&heights, work.bottom(), scale.s(MARGIN), scale.s(GAP));
        let x0 = work.right() - scale.s(MARGIN) - scale.s(WIDTH);
        let mut animating = false;
        for (i, e) in ui.stack.entries.iter().enumerate() {
            let w = ui.wins.get_mut(&e.id).unwrap();
            let target = tops[i] as f32;
            if w.y.is_nan() {
                w.y = target;
            } else if (w.y - target).abs() > 0.5 {
                w.y += (target - w.y) * 0.35;
                animating = true;
            } else {
                w.y = target;
            }
            let p = e.presence(now);
            if p < 1.0 {
                animating = true;
            }
            let slide = ((1.0 - p) * scale.s(24.0) as f32).round() as i32;
            let alpha = (OPAQUE as f32 * p).round() as u8;
            paint(&mut w.canvas, &e.toast, w.mouse);
            moves.push((w.hwnd, x0 + slide, w.y.round() as i32, scale.s(WIDTH), heights[i], alpha, true));
        }
        if ui.stack.is_empty() {
            KillTimer(ui.host, FRAME_TIMER);
        } else {
            // Fast frames while something moves; otherwise a slower tick for the countdowns.
            SetTimer(ui.host, FRAME_TIMER, if animating { 15 } else { 100 }, None);
        }
        animating
    });
    let _ = animating;
    unsafe {
        for h in destroy {
            DestroyWindow(h);
        }
        for (h, x, y, w, hh, alpha, _) in moves {
            SetLayeredWindowAttributes(h, 0, alpha.max(1), LWA_ALPHA);
            SetWindowPos(h, HWND_TOPMOST, x, y, w, hh, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            InvalidateRect(h, null(), 0);
        }
        SetThreadDpiAwarenessContext(previous);
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_TIMER && w == FRAME_TIMER {
        sync();
        return 0;
    }
    DefWindowProcW(hwnd, msg, w, l)
}

fn run_command(cmd: Command) {
    // Take the handler out while it runs: it may show another toast.
    let handler = with_ui(|ui| ui.handler.take());
    if let Some(h) = handler {
        h(cmd);
        with_ui(|ui| {
            if ui.handler.is_none() {
                ui.handler = Some(h);
            }
        });
    }
}

unsafe extern "system" fn toast_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let pt = || Pt::new((l & 0xFFFF) as u16 as i16 as i32, ((l >> 16) & 0xFFFF) as u16 as i16 as i32);
    match msg {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_ui(|ui| {
                if let Some(win) = ui.wins.values().find(|x| x.hwnd == hwnd) {
                    win.canvas.blit(hdc, 0, 0);
                }
            });
            EndPaint(hwnd, &ps);
            return 0;
        }
        WM_MOUSEMOVE => {
            if let Some(id) = find_id(hwnd) {
                with_ui(|ui| {
                    ui.stack.set_hover(id, true);
                    if let Some(win) = ui.wins.get_mut(&id) {
                        win.mouse = Some(pt());
                        if !win.tracking {
                            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                            TrackMouseEvent(&mut tme);
                            win.tracking = true;
                        }
                    }
                });
                sync();
            }
            return 0;
        }
        WM_MOUSELEAVE => {
            if let Some(id) = find_id(hwnd) {
                with_ui(|ui| {
                    ui.stack.set_hover(id, false);
                    if let Some(win) = ui.wins.get_mut(&id) {
                        win.mouse = None;
                        win.tracking = false;
                    }
                });
                sync();
            }
            return 0;
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            if let Some(id) = find_id(hwnd) {
                let cmd = with_ui(|ui| {
                    let now = now_ms(ui);
                    let entry = ui.stack.entries.iter().find(|e| e.id == id).cloned();
                    ui.stack.dismiss(id, now);
                    let entry = entry?;
                    if msg == WM_RBUTTONUP {
                        return None;
                    }
                    let win = ui.wins.get(&id)?;
                    match hit_button(&win.canvas, &entry.toast, pt()) {
                        Some(i) => Some(entry.toast.actions[i].1),
                        None => entry.toast.on_click,
                    }
                });
                sync();
                if let Some(c) = cmd {
                    run_command(c);
                }
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, w, l)
}

// ---------------------------------------------------------------- catalogue

/// Every toast in docs/UI.md's catalogue, with its exact wording.
pub mod catalog {
    use super::{Command, Toast, ToastKind};

    fn toast(kind: ToastKind, title: String, body: String) -> Toast {
        Toast { kind, title, body, actions: Vec::new(), on_click: None, key: None }
    }
    fn with(mut t: Toast, label: &str, cmd: Command) -> Toast {
        t.actions.push((label.to_string(), cmd));
        t
    }
    fn keyed(mut t: Toast, key: &'static str) -> Toast {
        t.key = Some(key);
        t
    }
    fn gb(v: f64) -> String {
        format!("{:.1}", v)
    }

    /// First run after install.
    pub fn first_run(hotkey: &str) -> Toast {
        let body = if hotkey.is_empty() {
            "It keeps memory free for Unreal.".to_string()
        } else {
            format!("It keeps memory free for Unreal. {} frees memory now.", hotkey)
        };
        with(toast(ToastKind::Info, "Ather MemGuard is running".into(), body), "Open settings", Command::OpenSettings)
    }

    /// Pressure becomes critical.
    pub fn memory_critical(ram_free_gb: f64, commit_free_gb: f64) -> Toast {
        let body = format!(
            "{} GB RAM free, {} GB commit free. Background apps are being held back. Don't start a build yet.",
            gb(ram_free_gb),
            gb(commit_free_gb)
        );
        keyed(with(toast(ToastKind::Error, "Memory is critical".into(), body), "Free memory", Command::FreeMemory), "pressure")
    }

    /// Pressure back to normal after critical.
    pub fn memory_fine(ram_free_gb: f64) -> Toast {
        keyed(toast(ToastKind::Info, "Memory is fine again".into(), format!("{} GB RAM free.", gb(ram_free_gb))), "pressure")
    }

    /// Auto-primed when a build or cook started; `reason` is "build" or "cook".
    pub fn auto_primed(reason: &str, processes: usize, gb_used: f64) -> Toast {
        toast(
            ToastKind::Info,
            format!("Freed memory for the {}", reason),
            format!("Held back {} background processes using {} GB.", processes, gb(gb_used)),
        )
    }

    /// Manual "Free memory".
    pub fn freed_manual(processes: usize, ram_before_gb: f64, ram_after_gb: f64) -> Toast {
        toast(
            ToastKind::Info,
            "Freed memory for Unreal".into(),
            format!("Held back {} background processes. RAM free went from {} to {} GB.", processes, gb(ram_before_gb), gb(ram_after_gb)),
        )
    }

    /// "Free Windows file cache" worked.
    pub fn cache_flushed(cache_before_gb: f64, ram_before_gb: f64, ram_after_gb: f64) -> Toast {
        toast(
            ToastKind::Info,
            "Emptied Windows' file cache".into(),
            format!("It held {} GB. RAM free went from {} to {} GB.", gb(cache_before_gb), gb(ram_before_gb), gb(ram_after_gb)),
        )
    }

    /// "Free Windows file cache" didn't run: administrator permission was declined.
    pub fn cache_flush_declined() -> Toast {
        toast(ToastKind::Info, "Windows' file cache is unchanged".into(), "Emptying it needs administrator permission, and that was declined.".into())
    }

    /// "Free Windows file cache" failed.
    pub fn cache_flush_failed(why: &str) -> Toast {
        toast(ToastKind::Error, "Couldn't empty Windows' file cache".into(), format!("{}.", why.trim_end_matches('.')))
    }

    /// Editor restart hint.
    pub fn editor_restart(committed_gb: f64, uptime_min: u64) -> Toast {
        toast(
            ToastKind::Warning,
            format!("Unreal Editor is at {:.0} GB", committed_gb),
            format!("It has been open {} h {} min. Restarting it gets that memory back.", uptime_min / 60, uptime_min % 60),
        )
    }

    /// Editor growing; `runs_out_in` is ready to read ("2 hours", "40 minutes").
    pub fn editor_growing(gb_per_hour: f64, runs_out_in: &str) -> Toast {
        with(
            toast(
                ToastKind::Warning,
                format!("Unreal Editor is growing {:.0} GB an hour", gb_per_hour),
                format!("At this rate memory runs out in about {}.", runs_out_in),
            ),
            "Show history",
            Command::OpenHistory,
        )
    }

    /// Paused for 30 minutes; `until` is "15:42".
    pub fn paused_30(until: &str) -> Toast {
        keyed(
            with(toast(ToastKind::Info, "Paused for 30 minutes".into(), format!("Other apps are back to normal until {}.", until)), "Resume now", Command::Resume),
            "pause",
        )
    }

    /// Paused until resumed.
    pub fn paused() -> Toast {
        keyed(with(toast(ToastKind::Info, "Paused".into(), "Other apps are back to normal until you resume.".into()), "Resume", Command::Resume), "pause")
    }

    pub fn resumed() -> Toast {
        keyed(toast(ToastKind::Info, "Ather MemGuard resumed".into(), String::new()), "pause")
    }

    /// Update available; clicking installs. `size` is ready to read ("1.9 MB").
    pub fn update_available(version: &str, notes: &str, size: &str) -> Toast {
        let notes = notes.trim();
        let body = if notes.is_empty() {
            format!("Click to update ({}). Your settings stay.", size)
        } else {
            let sep = if notes.ends_with(['.', '!', '?']) { "" } else { "." };
            format!("{}{} Click to update ({}). Your settings stay.", notes, sep, size)
        };
        let mut t = keyed(toast(ToastKind::Info, format!("Ather MemGuard {} is available", version), body), "update");
        t.on_click = Some(Command::InstallUpdate);
        t
    }

    /// Download progress; update the body in place with `downloading_body`.
    pub fn downloading(pct: u32) -> Toast {
        keyed(toast(ToastKind::Info, "Updating Ather MemGuard".into(), downloading_body(pct)), "update")
    }

    pub fn downloading_body(pct: u32) -> String {
        format!("Downloading… {}%", pct.min(100))
    }

    pub fn updated(version: &str) -> Toast {
        keyed(toast(ToastKind::Info, format!("Updated to Ather MemGuard {}", version), "Your settings are just as you left them.".into()), "update")
    }

    /// Manual check: up to date.
    pub fn up_to_date(version: &str) -> Toast {
        keyed(toast(ToastKind::Info, "Ather MemGuard is up to date".into(), format!("Version {}", version)), "update")
    }

    /// Manual check failed.
    pub fn check_failed(reason: &str) -> Toast {
        keyed(toast(ToastKind::Error, "Couldn't check for updates".into(), reason.to_string()), "update")
    }

    /// The update didn't install; clicking opens the releases page.
    pub fn update_failed() -> Toast {
        let mut t = keyed(toast(ToastKind::Error, "The update didn't install".into(), "Click to download it from GitHub instead.".into()), "update");
        t.on_click = Some(Command::OpenReleasesPage);
        t
    }

    pub fn startup_toggled(on: bool) -> Toast {
        keyed(toast(ToastKind::Info, format!("Run at startup: {}", if on { "On" } else { "Off" }), String::new()), "startup")
    }

    /// The settings file has a problem.
    pub fn settings_problem(message: &str) -> Toast {
        let m = message.trim();
        let sep = if m.is_empty() || m.ends_with(['.', '!', '?']) { "" } else { "." };
        keyed(
            with(
                toast(ToastKind::Error, "There's a problem in your settings".into(), format!("{}{} The last good settings are still in use.", m, sep).trim_start().to_string()),
                "Open file",
                Command::OpenSettingsFile,
            ),
            "settings",
        )
    }

    pub fn setup_copied() -> Toast {
        toast(ToastKind::Info, "Agent setup copied".into(), "Paste it into your agent's terminal.".into())
    }

    /// The prime hotkey couldn't be registered (docs/UI.md section 3).
    pub fn hotkey_taken(keys: &str) -> Toast {
        toast(ToastKind::Error, format!("Shortcut {} is taken", keys), "Pick another one in Settings.".into())
    }

    /// A settings write failed (settings window). Not in the catalogue table; docs/UI.md
    /// section 1 asks for "an error toast with the message".
    pub fn save_failed(message: &str) -> Toast {
        keyed(toast(ToastKind::Error, "Couldn't save that setting".into(), message.to_string()), "save")
    }

    /// The RAM runs below its rated speed (docs/RAMCHECK.md, "For the integrator"). `profile`
    /// is the kit's fast profile, "XMP" or "EXPO".
    pub fn ram_below_rated(current_mhz: u32, rated_mhz: u32, profile: &str) -> Toast {
        let profile = match profile {
            "XMP" | "EXPO" => profile,
            _ => "XMP or EXPO",
        };
        keyed(
            with(
                toast(
                    ToastKind::Warning,
                    "Your RAM is running below its rated speed".into(),
                    format!("{} MHz now, rated {} MHz. Turning on {} in the BIOS takes about five minutes.", current_mhz, rated_mhz, profile),
                ),
                "Show me how",
                Command::ShowRamReport,
            ),
            "ram",
        )
    }

    /// The RAM check found a problem other than the speed (old Intel microcode, both sticks on
    /// one channel): its own title and detail.
    pub fn ram_problem(title: &str, detail: &str) -> Toast {
        keyed(with(toast(ToastKind::Warning, title.trim().to_string(), detail.trim().to_string()), "Show me how", Command::ShowRamReport), "ram")
    }

    /// A watched drive changed level (`disk::level_change_words` gives the words). Getting
    /// fuller offers the checkup; one toast per drive at a time.
    /// One toast per drive: two drives changing level in the same poll each get theirs.
    pub fn disk_level(title: &str, body: &str, worse: bool, volume: &str) -> Toast {
        let key = disk_key(volume);
        if worse {
            keyed(with(toast(ToastKind::Warning, title.to_string(), body.to_string()), "Open the checkup", Command::OpenDoctor), key)
        } else {
            keyed(toast(ToastKind::Info, title.to_string(), body.to_string()), key)
        }
    }

    /// "disk:D" for "D:", "disk" for a drive without a letter.
    pub fn disk_key(volume: &str) -> &'static str {
        const KEYS: [&str; 26] = [
            "disk:A", "disk:B", "disk:C", "disk:D", "disk:E", "disk:F", "disk:G", "disk:H", "disk:I", "disk:J", "disk:K", "disk:L", "disk:M",
            "disk:N", "disk:O", "disk:P", "disk:Q", "disk:R", "disk:S", "disk:T", "disk:U", "disk:V", "disk:W", "disk:X", "disk:Y", "disk:Z",
        ];
        let mut c = volume.chars();
        match (c.next(), c.next()) {
            (Some(l), Some(':')) if l.is_ascii_alphabetic() => KEYS[(l.to_ascii_uppercase() as u8 - b'A') as usize],
            _ => "disk",
        }
    }

    /// "Show me how" couldn't write the RAM report page. Not in the catalogue table: the person
    /// just clicked and needs an answer.
    pub fn ram_report_failed(message: &str) -> Toast {
        keyed(toast(ToastKind::Error, "Couldn't open the RAM report".into(), message.to_string()), "ram")
    }

    /// "Steam, Spotify and Blender"
    fn and_list(names: &[String]) -> String {
        match names {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
        }
    }

    /// Unreal work started and the checkup found at least `nudge_above_gb` to free
    /// (docs/UI.md section 6); `top` are the biggest apps, at most three are named.
    pub fn checkup_nudge(gb_free: f64, top: &[String]) -> Toast {
        let names: Vec<String> = top.iter().take(3).cloned().collect();
        let body = if names.is_empty() { String::new() } else { format!("{}.", and_list(&names)) };
        keyed(
            with(toast(ToastKind::Info, format!("{} GB is in use by apps Unreal doesn't need", gb(gb_free)), body), "Review", Command::OpenDoctor),
            "checkup",
        )
    }

    /// A cleanup from the checkup window finished.
    pub fn checkup_done(freed_gb: f64, startup_off: usize) -> Toast {
        let mut parts = Vec::new();
        if freed_gb >= 0.05 {
            parts.push(format!("Freed about {} GB.", gb(freed_gb)));
        }
        match startup_off {
            0 => {}
            1 => parts.push("1 app won't start with Windows any more.".to_string()),
            n => parts.push(format!("{} apps won't start with Windows any more.", n)),
        }
        keyed(with(toast(ToastKind::Info, "Checkup done".into(), parts.join(" ")), "Undo", Command::UndoDoctor), "checkup")
    }

    /// The guard asked a `close_while_unreal` app to close.
    pub fn guard_closed(app: &str) -> Toast {
        keyed(
            with(
                toast(ToastKind::Info, format!("Closed {}", app), "It's on your list to stay closed while Unreal is running.".into()),
                "Allow this time",
                Command::AllowGuarded,
            ),
            "guard",
        )
    }

    /// "Undo" on "Checkup done" finished. Not in the catalogue table: the undo needs an answer.
    /// `problems` are the reasons parts couldn't be undone (empty when everything was).
    pub fn checkup_undone(problems: &[String]) -> Toast {
        if problems.is_empty() {
            keyed(toast(ToastKind::Info, "Undid the last cleanup".into(), "Everything it changed is back as it was.".into()), "checkup")
        } else {
            keyed(toast(ToastKind::Warning, "Couldn't undo all of it".into(), problems.join(" ")), "checkup")
        }
    }

    /// One sample of every toast with the mockup's numbers: (snapshot name, toast).
    pub fn samples() -> Vec<(&'static str, Toast)> {
        vec![
            ("first-run", first_run("Ctrl+Alt+M")),
            ("critical", memory_critical(4.1, 6.2)),
            ("fine-again", memory_fine(38.4)),
            ("auto-primed", auto_primed("build", 131, 9.8)),
            ("freed", freed_manual(131, 31.2, 41.0)),
            ("editor-restart", editor_restart(52.0, 450)),
            ("editor-growing", editor_growing(8.0, "2 hours")),
            ("paused-30", paused_30("15:42")),
            ("paused", paused()),
            ("resumed", resumed()),
            ("update-available", update_available("0.2.1", "Auto-prime now also runs before cooks.", "1.9 MB")),
            ("downloading", downloading(42)),
            ("updated", updated("0.2.1")),
            ("up-to-date", up_to_date("0.2.0")),
            ("check-failed", check_failed("GitHub didn't answer. Check your connection and try again.")),
            ("update-failed", update_failed()),
            ("startup", startup_toggled(true)),
            ("settings-problem", settings_problem("Line 14: preset must be gentle, balanced or aggressive.")),
            ("setup-copied", setup_copied()),
            ("hotkey-taken", hotkey_taken("Ctrl+Alt+M")),
            ("save-failed", save_failed("settings.toml is read-only.")),
            ("checkup-nudge", checkup_nudge(6.3, &["Steam".into(), "Blender".into(), "Spotify".into(), "iCUE".into()])),
            ("checkup-done", checkup_done(3.2, 2)),
            ("guard-closed", guard_closed("Steam")),
            ("checkup-undone", checkup_undone(&[])),
            ("ram-speed", ram_below_rated(4800, 6000, "EXPO")),
            (
                "ram-problem",
                ram_problem(
                    "Update the BIOS before changing memory settings",
                    "This CPU runs microcode 0x129. Versions before 0x12B can slowly damage 13th and 14th gen Core CPUs. Install the newest BIOS from the board maker first; it carries the fix.",
                ),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(title: &str) -> Toast {
        Toast { kind: ToastKind::Info, title: title.into(), body: String::new(), actions: Vec::new(), on_click: None, key: None }
    }

    #[test]
    fn stacking_and_eviction() {
        let mut s = Stack::new();
        let a = s.show(t("a"), 0);
        let b = s.show(t("b"), 10);
        let c = s.show(t("c"), 20);
        assert_eq!(s.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a, b, c]);
        // A fourth: the oldest leaves early, the rest stay.
        let d = s.show(t("d"), 30);
        let leaving: Vec<ToastId> = s.entries.iter().filter(|e| e.leaving()).map(|e| e.id).collect();
        assert_eq!(leaving, vec![a]);
        assert_eq!(s.entries.iter().filter(|e| !e.leaving()).count(), MAX_VISIBLE);
        // A fifth: next oldest (b) goes.
        s.show(t("e"), 40);
        let leaving: Vec<ToastId> = s.entries.iter().filter(|e| e.leaving()).map(|e| e.id).collect();
        assert_eq!(leaving, vec![a, b]);
        // Once the leave animation is over they are gone, in order.
        let removed = s.tick(30 + LEAVE_MS);
        assert_eq!(removed, vec![a]);
        let removed = s.tick(40 + LEAVE_MS);
        assert_eq!(removed, vec![b]);
        assert_eq!(s.entries.iter().map(|e| e.id).collect::<Vec<_>>()[..2], [c, d]);
    }

    #[test]
    fn timing_hover_and_buttons() {
        let mut s = Stack::new();
        let a = s.show(t("plain"), 0);
        let mut with_button = t("button");
        with_button.actions.push(("Resume".into(), Command::Resume));
        let b = s.show(with_button, 0);
        assert_eq!(s.entries[1].remaining_ms, WITH_BUTTONS_MS);
        s.tick(ENTER_MS);
        assert_eq!(s.entries[0].phase, Phase::Shown);
        // Hover pauses the countdown.
        s.set_hover(a, true);
        s.tick(DEFAULT_MS + 100);
        assert!(!s.entries[0].leaving());
        s.set_hover(a, false);
        s.tick(DEFAULT_MS + 200);
        let e = s.entries.iter().find(|e| e.id == a).unwrap();
        assert_eq!(e.remaining_ms, DEFAULT_MS - ENTER_MS - 100);
        s.tick(2 * DEFAULT_MS + 200);
        assert!(s.entries.iter().find(|e| e.id == a).unwrap().leaving());
        assert!(!s.entries.iter().find(|e| e.id == b).unwrap().leaving());
        s.tick(2 * DEFAULT_MS + 200 + LEAVE_MS);
        assert!(s.entries.iter().all(|e| e.id != a));
        // Presence ramps in and out.
        let mut s = Stack::new();
        s.show(t("x"), 1000);
        assert_eq!(s.entries[0].presence(1000), 0.0);
        assert!((s.entries[0].presence(1075) - 0.5).abs() < 0.01);
        assert_eq!(s.entries[0].presence(1150), 1.0);
    }

    #[test]
    fn key_replaces_and_body_updates() {
        let mut s = Stack::new();
        let a = s.show(catalog::paused_30("15:42"), 0);
        s.tick(3000);
        let b = s.show(catalog::resumed(), 3000);
        assert_eq!(a, b);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].toast.title, "Ather MemGuard resumed");
        assert_eq!(s.entries[0].remaining_ms, DEFAULT_MS);
        let d = s.show(catalog::downloading(0), 3100);
        assert_ne!(d, a, "different keys stack");
        assert_eq!(s.show(catalog::update_available("0.2.1", "", "1 MB"), 3100), d, "same key replaces");
        s.show(catalog::downloading(0), 3100);
        assert!(s.update_body(d, &catalog::downloading_body(55)));
        assert_eq!(s.entries.iter().find(|e| e.id == d).unwrap().toast.body, "Downloading… 55%");
        s.dismiss(d, 3200);
        assert!(!s.update_body(d, "late"));
        assert!(!s.update_body(ToastId(999), "nope"));
    }

    #[test]
    fn positions_stack_upwards() {
        let tops = stack_positions(&[80, 100, 60], 1000, 16, 12);
        assert_eq!(tops, vec![1000 - 16 - 80, 1000 - 16 - 80 - 12 - 100, 1000 - 16 - 80 - 12 - 100 - 12 - 60]);
    }

    #[test]
    fn catalogue_wording() {
        let c = catalog::memory_critical(4.1, 6.2);
        assert_eq!(c.body, "4.1 GB RAM free, 6.2 GB commit free. Background apps are being held back. Don't start a build yet.");
        assert_eq!(c.actions, vec![("Free memory".to_string(), Command::FreeMemory)]);
        assert_eq!(c.duration_ms(), WITH_BUTTONS_MS);
        assert_eq!(catalog::editor_restart(52.0, 450).body, "It has been open 7 h 30 min. Restarting it gets that memory back.");
        assert_eq!(catalog::update_available("0.2.1", "Faster.", "1.9 MB").body, "Faster. Click to update (1.9 MB). Your settings stay.");
        assert_eq!(catalog::update_available("0.2.1", "", "1.9 MB").on_click, Some(Command::InstallUpdate));
        assert_eq!(catalog::startup_toggled(false).title, "Run at startup: Off");
        assert_eq!(catalog::first_run("Ctrl+Alt+M").body, "It keeps memory free for Unreal. Ctrl+Alt+M frees memory now.");
        assert_eq!(catalog::settings_problem("Bad value").body, "Bad value. The last good settings are still in use.");
        assert!(catalog::samples().len() >= 19);

        // docs/UI.md section 6.
        let n = catalog::checkup_nudge(6.34, &["Steam".into(), "Blender".into(), "Spotify".into(), "iCUE".into()]);
        assert_eq!(n.title, "6.3 GB is in use by apps Unreal doesn't need");
        assert_eq!(n.body, "Steam, Blender and Spotify.");
        assert_eq!(n.actions, vec![("Review".to_string(), Command::OpenDoctor)]);
        assert_eq!(catalog::checkup_nudge(4.0, &["Steam".into()]).body, "Steam.");
        let d = catalog::checkup_done(3.2, 2);
        assert_eq!((d.title.as_str(), d.body.as_str()), ("Checkup done", "Freed about 3.2 GB. 2 apps won't start with Windows any more."));
        assert_eq!(d.actions, vec![("Undo".to_string(), Command::UndoDoctor)]);
        assert_eq!(catalog::checkup_done(1.0, 0).body, "Freed about 1.0 GB.");
        assert_eq!(catalog::checkup_done(0.0, 1).body, "1 app won't start with Windows any more.");
        let g = catalog::guard_closed("Steam");
        assert_eq!((g.title.as_str(), g.body.as_str()), ("Closed Steam", "It's on your list to stay closed while Unreal is running."));
        assert_eq!(g.actions, vec![("Allow this time".to_string(), Command::AllowGuarded)]);

        // docs/RAMCHECK.md, "For the integrator".
        let r = catalog::ram_below_rated(4800, 6000, "EXPO");
        assert_eq!(r.kind, ToastKind::Warning);
        assert_eq!(r.title, "Your RAM is running below its rated speed");
        assert_eq!(r.body, "4800 MHz now, rated 6000 MHz. Turning on EXPO in the BIOS takes about five minutes.");
        assert_eq!(r.actions, vec![("Show me how".to_string(), Command::ShowRamReport)]);
        assert_eq!(catalog::ram_below_rated(2133, 3600, "XMP").body, "2133 MHz now, rated 3600 MHz. Turning on XMP in the BIOS takes about five minutes.");
        assert!(catalog::ram_below_rated(4800, 6000, "JEDEC").body.contains("Turning on XMP or EXPO in the BIOS"));
        let p = catalog::ram_problem("Update the BIOS first", "Old microcode.");
        assert_eq!((p.kind, p.title.as_str(), p.body.as_str()), (ToastKind::Warning, "Update the BIOS first", "Old microcode."));
        assert_eq!(p.actions, r.actions, "same action as the checkup link");
        assert_eq!(p.key, r.key);
    }

    #[test]
    fn toast_layout_fits_at_two_scales() {
        for scale in [1.0, 1.5] {
            for (name, t) in catalog::samples() {
                let c = render(&t, Scale(scale));
                assert_eq!(c.width(), Scale(scale).s(WIDTH));
                let l = layout(&c, &t);
                for r in &l.buttons {
                    assert!(c.bounds().encloses(r), "{name} at {scale}");
                }
                for (i, a) in l.buttons.iter().enumerate() {
                    for b in &l.buttons[i + 1..] {
                        assert!(!a.intersects(b), "{name}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod disk_toast_tests {
    use super::catalog::{disk_key, disk_level};

    #[test]
    fn each_drive_gets_its_own_toast() {
        assert_eq!(disk_key("D:"), "disk:D");
        assert_eq!(disk_key("c:"), "disk:C");
        assert_eq!(disk_key(r"\?\Volume{1234}\"), "disk");
        let d = disk_level("D: is almost full", "body", true, "D:");
        let c = disk_level("C: is low on space", "body", true, "C:");
        assert_ne!(d.key, c.key, "two drives in one poll must not replace each other");
    }
}

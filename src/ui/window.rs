//! The generic window layer: one window class and window proc for every surface.
//!
//! A surface implements `Surface` (paint into a `Canvas`, react to `Event`s). Handlers get a
//! `Host` that *records* requests (repaint, capture, popup menu, temporary EDIT, file dialog,
//! close); they run after the handler returns, so modal loops never re-enter a surface that is
//! still borrowed. Results come back as `Event::Menu`, `Event::EditDone`, `Event::FileChosen`.
//!
//! Also here: DPI awareness and `WM_DPICHANGED`, the dark title bar, dark popup menus,
//! hover tracking, wheel, keyboard, and one window per kind (opening again brings it forward).

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MonitorFromRect,
    ScreenToClient, SetBkColor, SetTextColor, HBRUSH, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
    PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL, VK_ESCAPE, VK_MENU,
    VK_RETURN, VK_SHIFT, VK_TAB,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::canvas::{create_hfont, Canvas};
use super::geom::{Pt, Rect};
use super::theme::{self, Scale, F_VALUE};
use crate::win::wide;

const CLASS: &str = "AtherMemGuardUi";
const WM_EDIT_DONE: u32 = WM_APP + 40;
const EDIT_ID: usize = 100;
const WM_MOUSELEAVE: u32 = 0x02A3;
const EM_SETSEL: u32 = 0x00B1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Client size or DPI changed.
    Resized,
    MouseMove(Pt),
    MouseLeave,
    MouseDown(Pt, MouseButton),
    MouseUp(Pt, MouseButton),
    /// Wheel delta, 120 per notch, positive away from the user.
    Wheel(i32),
    /// Key down (also Alt combinations); `vk` is the virtual-key code.
    Key(u16, Mods),
    KeyUp(u16, Mods),
    /// A typed character (control characters are delivered as `Key`).
    Char(char),
    Timer(usize),
    /// The temporary EDIT closed; `commit` is false for Esc.
    EditDone { tag: u32, commit: bool, text: String },
    /// A popup menu closed; `choice` is the picked item's id.
    Menu { tag: u32, choice: Option<u32> },
    FileChosen { tag: u32, path: Option<PathBuf> },
    CaptureLost,
    /// The window is being destroyed: last chance to save its placement.
    Destroyed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub id: u32,
    pub label: String,
    pub checked: bool,
    pub disabled: bool,
}

impl MenuItem {
    pub fn new(id: u32, label: &str) -> MenuItem {
        MenuItem { id, label: label.to_string(), checked: false, disabled: false }
    }
    pub fn separator() -> MenuItem {
        MenuItem { id: 0, label: String::new(), checked: false, disabled: true }
    }
    pub fn checked(mut self, on: bool) -> MenuItem {
        self.checked = on;
        self
    }
    pub fn disabled(mut self, off: bool) -> MenuItem {
        self.disabled = off;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    Hand,
}

enum Request {
    Invalidate,
    Close,
    Capture(bool),
    Timer(usize, u32),
    KillTimer(usize),
    Menu { tag: u32, items: Vec<MenuItem>, at: Pt },
    Edit { tag: u32, rect: Rect, text: String, numeric: bool },
    EndEdit(bool),
    OpenFile { tag: u32, title: String, filters: Vec<(String, String)> },
    SaveFile { tag: u32, title: String, name: String, filters: Vec<(String, String)> },
    Shell(String),
    Focus,
}

/// What a surface can ask of its window while handling an event.
pub struct Host {
    pub hwnd: HWND,
    pub scale: Scale,
    /// Client size in device pixels.
    pub size: (i32, i32),
    cursor: Option<Cursor>,
    requests: Vec<Request>,
}

impl Host {
    fn new(hwnd: HWND, scale: Scale) -> Host {
        let mut rc: RECT = unsafe { zeroed() };
        if !hwnd.is_null() {
            unsafe { GetClientRect(hwnd, &mut rc) };
        }
        Host { hwnd, scale, size: (rc.right, rc.bottom), cursor: None, requests: Vec::new() }
    }
    /// A host not attached to a window (tests, snapshots): every request is dropped.
    pub fn detached(size: (i32, i32), scale: Scale) -> Host {
        Host { hwnd: null_mut(), scale, size, cursor: None, requests: Vec::new() }
    }
    pub fn invalidate(&mut self) {
        self.requests.push(Request::Invalidate);
    }
    pub fn close(&mut self) {
        self.requests.push(Request::Close);
    }
    pub fn capture(&mut self, on: bool) {
        self.requests.push(Request::Capture(on));
    }
    pub fn set_timer(&mut self, id: usize, ms: u32) {
        self.requests.push(Request::Timer(id, ms));
    }
    pub fn kill_timer(&mut self, id: usize) {
        self.requests.push(Request::KillTimer(id));
    }
    /// Pop up a menu at `at` (client coordinates); the result arrives as `Event::Menu { tag }`.
    pub fn menu(&mut self, tag: u32, items: Vec<MenuItem>, at: Pt) {
        self.requests.push(Request::Menu { tag, items, at });
    }
    /// Put a temporary EDIT over `rect`; the text arrives as `Event::EditDone { tag }`.
    pub fn edit(&mut self, tag: u32, rect: Rect, text: &str, numeric: bool) {
        self.requests.push(Request::Edit { tag, rect, text: text.to_string(), numeric });
    }
    pub fn end_edit(&mut self, commit: bool) {
        self.requests.push(Request::EndEdit(commit));
    }
    pub fn open_file(&mut self, tag: u32, title: &str, filters: &[(&str, &str)]) {
        let filters = filters.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        self.requests.push(Request::OpenFile { tag, title: title.to_string(), filters });
    }
    pub fn save_file(&mut self, tag: u32, title: &str, name: &str, filters: &[(&str, &str)]) {
        let filters = filters.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        self.requests.push(Request::SaveFile { tag, title: title.to_string(), name: name.to_string(), filters });
    }
    /// Open a file or URL with its default program.
    pub fn shell_open(&mut self, target: &str) {
        self.requests.push(Request::Shell(target.to_string()));
    }
    pub fn focus(&mut self) {
        self.requests.push(Request::Focus);
    }
    pub fn set_cursor(&mut self, c: Cursor) {
        self.cursor = Some(c);
    }
    /// The window's outer rectangle on screen, in device pixels.
    pub fn window_rect(&self) -> Option<Rect> {
        if self.hwnd.is_null() {
            return None;
        }
        let mut wp: WINDOWPLACEMENT = unsafe { zeroed() };
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        unsafe { GetWindowPlacement(self.hwnd, &mut wp) };
        let r = wp.rcNormalPosition;
        Some(Rect::ltrb(r.left, r.top, r.right, r.bottom))
    }
}

pub trait Surface {
    /// Paint the whole client area. `c` is sized to the client and scaled to the window DPI.
    fn paint(&mut self, c: &mut Canvas);
    /// React to an event. Return false to let Windows handle keys it would normally handle (Alt+F4).
    fn event(&mut self, ev: Event, host: &mut Host) -> bool;
}

/// How to open a window.
pub struct WindowSpec {
    /// One window per kind: opening a kind that is open brings that window forward.
    pub kind: &'static str,
    pub title: String,
    /// Size at 96 DPI.
    pub size: (i32, i32),
    /// Minimum size at 96 DPI.
    pub min_size: (i32, i32),
    /// Remembered outer rectangle in device pixels; used when it is on a monitor.
    pub placement: Option<Rect>,
    pub resizable: bool,
}

struct WinData {
    surface: Box<dyn Surface>,
    canvas: Canvas,
    scale: Scale,
    kind: &'static str,
    min_size: (i32, i32),
    tracking: bool,
    cursor: Cursor,
    edit: HWND,
    edit_tag: u32,
    edit_proc: isize,
    edit_font: HFONT,
    edit_brush: HBRUSH,
    swallow_syschar: bool,
}

type Cell = RefCell<WinData>;

thread_local! {
    static OPEN: RefCell<HashMap<&'static str, isize>> = RefCell::new(HashMap::new());
}

/// The open window of `kind`, if any.
pub fn find(kind: &str) -> Option<HWND> {
    OPEN.with(|o| o.borrow().get(kind).map(|h| *h as HWND)).filter(|h| unsafe { IsWindow(*h) } != 0)
}

/// Restore and focus a window.
pub fn bring_to_front(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

/// Repaint a window of `kind` (after outside data changed).
pub fn invalidate(kind: &str) {
    if let Some(h) = find(kind) {
        unsafe { InvalidateRect(h, null(), 0) };
    }
}

/// Make standard popup menus dark (uxtheme's undocumented SetPreferredAppMode, ordinal 135).
pub(crate) fn allow_dark_menus() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let lib = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(lib, 135usize as *const u8) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(1); // AllowDark
        }
        if let Some(f) = GetProcAddress(lib, 136usize as *const u8) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    });
}

fn allow_dark_for_window(hwnd: HWND) {
    unsafe {
        let lib = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(lib, 133usize as *const u8) {
            let allow: unsafe extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(f);
            allow(hwnd, 1);
        }
    }
}

/// Dark title bar in the theme's background colour.
pub(crate) fn dark_title_bar(hwnd: HWND) {
    unsafe {
        let on: i32 = 1;
        DwmSetWindowAttribute(hwnd, 20 /* DWMWA_USE_IMMERSIVE_DARK_MODE */, &on as *const i32 as *const _, 4);
        let caption: COLORREF = theme::BG.colorref();
        DwmSetWindowAttribute(hwnd, 35 /* DWMWA_CAPTION_COLOR */, &caption as *const u32 as *const _, 4);
        let border: COLORREF = theme::BORDER.colorref();
        DwmSetWindowAttribute(hwnd, 34 /* DWMWA_BORDER_COLOR */, &border as *const u32 as *const _, 4);
    }
}

fn register_class() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let name = wide(CLASS);
        let hinst = GetModuleHandleW(null());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: crate::logo::create_icon(32, crate::engine::Level::Normal),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: name.as_ptr(),
            hIconSm: crate::logo::create_icon(16, crate::engine::Level::Normal),
        };
        RegisterClassExW(&wc);
    });
}

fn cursor_point() -> POINT {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut pt) };
    pt
}

fn work_area_at(pt: POINT) -> Rect {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let r = mi.rcWork;
        Rect::ltrb(r.left, r.top, r.right, r.bottom)
    }
}

fn dpi_at(pt: POINT) -> u32 {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y) < 0 {
            x = 96;
        }
        x
    }
}

/// True when `r` is at least partly on a monitor.
fn on_screen(r: Rect) -> bool {
    let rc = r.to_win();
    unsafe { !MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_null() }
}

/// Open a window for `surface`, or bring the open window of the same kind forward
/// (the new surface is then dropped). Call on a thread with a message loop.
pub fn open(spec: WindowSpec, surface: Box<dyn Surface>) -> HWND {
    if let Some(h) = find(spec.kind) {
        bring_to_front(h);
        return h;
    }
    register_class();
    allow_dark_menus();
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let pt = cursor_point();
        let scale = Scale::from_dpi(dpi_at(pt));
        let work = work_area_at(pt);
        let rect = match spec.placement.filter(|r| r.w > 100 && r.h > 100 && on_screen(*r)) {
            Some(r) => r,
            None => {
                let w = scale.s(spec.size.0 as f32).min(work.w * 95 / 100);
                let h = scale.s(spec.size.1 as f32).min(work.h * 95 / 100);
                Rect::new(work.x + (work.w - w) / 2, work.y + (work.h - h) / 2, w, h)
            }
        };
        let data = Box::new(RefCell::new(WinData {
            surface,
            canvas: Canvas::new(rect.w.max(1), rect.h.max(1), scale),
            scale,
            kind: spec.kind,
            min_size: spec.min_size,
            tracking: false,
            cursor: Cursor::Arrow,
            edit: null_mut(),
            edit_tag: 0,
            edit_proc: 0,
            edit_font: null_mut(),
            edit_brush: CreateSolidBrush(theme::RAISED.colorref()),
            swallow_syschar: false,
        }));
        let style = if spec.resizable { WS_OVERLAPPEDWINDOW } else { WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX };
        let title = wide(&spec.title);
        let class = wide(CLASS);
        let raw = Box::into_raw(data);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            raw as *const _,
        );
        SetThreadDpiAwarenessContext(previous);
        if hwnd.is_null() {
            drop(Box::from_raw(raw));
            return hwnd;
        }
        // The DPI of the monitor the window actually landed on.
        let real = Scale::from_dpi(GetDpiForWindow(hwnd));
        if let Ok(mut d) = (*raw).try_borrow_mut() {
            d.scale = real;
            d.canvas.set_scale(real);
        }
        dark_title_bar(hwnd);
        allow_dark_for_window(hwnd);
        OPEN.with(|o| o.borrow_mut().insert(spec.kind, hwnd as isize));
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
        dispatch(hwnd, Event::Resized);
        hwnd
    }
}

/// Run a message loop until `hwnd` is closed (examples and tools; the tray has its own loop).
pub fn run_until_closed(hwnd: HWND) {
    unsafe {
        let mut msg: MSG = zeroed();
        while IsWindow(hwnd) != 0 && GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn cell(hwnd: HWND) -> Option<&'static Cell> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Cell;
    if p.is_null() {
        None
    } else {
        Some(unsafe { &*p })
    }
}

fn mods() -> Mods {
    unsafe {
        Mods {
            ctrl: GetKeyState(VK_CONTROL as i32) < 0,
            shift: GetKeyState(VK_SHIFT as i32) < 0,
            alt: GetKeyState(VK_MENU as i32) < 0,
        }
    }
}

/// Deliver an event to the window's surface, then carry out what it asked for.
fn dispatch(hwnd: HWND, ev: Event) -> bool {
    let Some(c) = cell(hwnd) else { return false };
    let (handled, host) = {
        let Ok(mut d) = c.try_borrow_mut() else { return false };
        let mut host = Host::new(hwnd, d.scale);
        let handled = d.surface.event(ev, &mut host);
        if let Some(cur) = host.cursor {
            d.cursor = cur;
        }
        (handled, host)
    };
    apply(hwnd, host.requests);
    handled
}

fn apply(hwnd: HWND, requests: Vec<Request>) {
    for r in requests {
        unsafe {
            match r {
                Request::Invalidate => {
                    InvalidateRect(hwnd, null(), 0);
                }
                Request::Close => {
                    DestroyWindow(hwnd);
                    return;
                }
                Request::Capture(true) => {
                    SetCapture(hwnd);
                }
                Request::Capture(false) => {
                    ReleaseCapture();
                }
                Request::Timer(id, ms) => {
                    SetTimer(hwnd, id, ms, None);
                }
                Request::KillTimer(id) => {
                    KillTimer(hwnd, id);
                }
                Request::Menu { tag, items, at } => {
                    let choice = popup_menu(hwnd, &items, at);
                    dispatch(hwnd, Event::Menu { tag, choice });
                }
                Request::Edit { tag, rect, text, numeric } => begin_edit(hwnd, tag, rect, &text, numeric),
                Request::EndEdit(commit) => end_edit(hwnd, commit),
                Request::OpenFile { tag, title, filters } => {
                    let f: Vec<(&str, &str)> = filters.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                    let path = super::dialogs::open_file(hwnd, &title, &f);
                    dispatch(hwnd, Event::FileChosen { tag, path });
                }
                Request::SaveFile { tag, title, name, filters } => {
                    let f: Vec<(&str, &str)> = filters.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                    let path = super::dialogs::save_file(hwnd, &title, &name, &f);
                    dispatch(hwnd, Event::FileChosen { tag, path });
                }
                Request::Shell(target) => {
                    let op = wide("open");
                    let t = wide(&target);
                    ShellExecuteW(hwnd, op.as_ptr(), t.as_ptr(), null(), null(), SW_SHOWNORMAL);
                }
                Request::Focus => {
                    SetFocus(hwnd);
                }
            }
        }
    }
}

fn popup_menu(hwnd: HWND, items: &[MenuItem], at: Pt) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu();
        for it in items {
            if it.id == 0 && it.label.is_empty() {
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
                continue;
            }
            let mut flags = MF_STRING;
            if it.checked {
                flags |= MF_CHECKED;
            }
            if it.disabled {
                flags |= MF_GRAYED;
            }
            let label = wide(&it.label.replace('&', "&&"));
            AppendMenuW(menu, flags, it.id as usize, label.as_ptr());
        }
        let mut pt = POINT { x: at.x, y: at.y };
        windows_sys::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
        let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
        DestroyMenu(menu);
        if id > 0 {
            Some(id as u32)
        } else {
            None
        }
    }
}

unsafe extern "system" fn edit_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let parent = GetParent(h);
    let Some(c) = cell(parent) else { return DefWindowProcW(h, m, w, l) };
    let prev = match c.try_borrow() {
        Ok(d) => d.edit_proc,
        Err(_) => return DefWindowProcW(h, m, w, l),
    };
    match m {
        WM_KEYDOWN if w == VK_RETURN as usize || w == VK_ESCAPE as usize || w == VK_TAB as usize => {
            PostMessageW(parent, WM_EDIT_DONE, (w != VK_ESCAPE as usize) as usize, 0);
            return 0;
        }
        WM_CHAR if w == 13 || w == 27 || w == 9 => return 0, // no beep
        WM_KILLFOCUS => {
            PostMessageW(parent, WM_EDIT_DONE, 1, 0);
        }
        _ => {}
    }
    let prev: WNDPROC = std::mem::transmute(prev);
    CallWindowProcW(prev, h, m, w, l)
}

fn begin_edit(hwnd: HWND, tag: u32, rect: Rect, text: &str, numeric: bool) {
    end_edit(hwnd, true);
    let Some(c) = cell(hwnd) else { return };
    let scale = match c.try_borrow() {
        Ok(d) => d.scale,
        Err(_) => return,
    };
    unsafe {
        let class = wide("EDIT");
        let t = wide(text);
        let style = WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32 | if numeric { ES_RIGHT as u32 } else { 0 };
        let edit = CreateWindowExW(
            0,
            class.as_ptr(),
            t.as_ptr(),
            style,
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            hwnd,
            EDIT_ID as _,
            GetModuleHandleW(null()),
            null(),
        );
        if edit.is_null() {
            return;
        }
        let font = create_hfont(F_VALUE, scale);
        SendMessageW(edit, WM_SETFONT, font as usize, 1);
        SendMessageW(edit, EM_SETSEL, 0, -1);
        let prev = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        if let Ok(mut d) = c.try_borrow_mut() {
            d.edit = edit;
            d.edit_tag = tag;
            d.edit_proc = prev;
            d.edit_font = font;
        }
        SetFocus(edit);
    }
}

fn end_edit(hwnd: HWND, commit: bool) {
    let Some(c) = cell(hwnd) else { return };
    let (edit, tag, font) = match c.try_borrow_mut() {
        Ok(mut d) => {
            let e = (d.edit, d.edit_tag, d.edit_font);
            d.edit = null_mut(); // first: DestroyWindow sends WM_KILLFOCUS, which must not re-enter
            d.edit_font = null_mut();
            e
        }
        Err(_) => return,
    };
    if edit.is_null() {
        return;
    }
    let text = unsafe {
        let n = GetWindowTextLengthW(edit);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(edit, buf.as_mut_ptr(), n + 1);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    };
    unsafe {
        DestroyWindow(edit);
        DeleteObject(font);
        SetFocus(hwnd);
    }
    dispatch(hwnd, Event::EditDone { tag, commit, text: text.trim().to_string() });
}

fn lparam_pt(l: LPARAM) -> Pt {
    Pt::new((l & 0xFFFF) as u16 as i16 as i32, ((l >> 16) & 0xFFFF) as u16 as i16 as i32)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(l as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, w, l);
        }
        WM_NCDESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Cell;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !p.is_null() {
                let b = Box::from_raw(p);
                if let Ok(d) = b.try_borrow() {
                    DeleteObject(d.edit_brush);
                }
                drop(b);
            }
            return DefWindowProcW(hwnd, msg, w, l);
        }
        _ => {}
    }
    let Some(c) = cell(hwnd) else { return DefWindowProcW(hwnd, msg, w, l) };
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc: HDC = BeginPaint(hwnd, &mut ps);
            if let Ok(mut d) = c.try_borrow_mut() {
                let mut rc: RECT = zeroed();
                GetClientRect(hwnd, &mut rc);
                let d = &mut *d;
                d.canvas.resize(rc.right, rc.bottom);
                d.canvas.set_scale(d.scale);
                d.surface.paint(&mut d.canvas);
                d.canvas.blit(hdc, 0, 0);
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            dispatch(hwnd, Event::Resized);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_GETMINMAXINFO => {
            if let Ok(d) = c.try_borrow() {
                let mmi = &mut *(l as *mut MINMAXINFO);
                mmi.ptMinTrackSize.x = d.scale.s(d.min_size.0 as f32);
                mmi.ptMinTrackSize.y = d.scale.s(d.min_size.1 as f32);
            }
            0
        }
        WM_DPICHANGED => {
            let scale = Scale::from_dpi((w >> 16) as u32 & 0xFFFF);
            if let Ok(mut d) = c.try_borrow_mut() {
                d.scale = scale;
                d.canvas.set_scale(scale);
            }
            end_edit(hwnd, true);
            let r = &*(l as *const RECT);
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            dispatch(hwnd, Event::Resized);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEMOVE => {
            if let Ok(mut d) = c.try_borrow_mut() {
                if !d.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut tme);
                    d.tracking = true;
                }
            }
            dispatch(hwnd, Event::MouseMove(lparam_pt(l)));
            0
        }
        WM_MOUSELEAVE => {
            if let Ok(mut d) = c.try_borrow_mut() {
                d.tracking = false;
            }
            dispatch(hwnd, Event::MouseLeave);
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            SetFocus(hwnd);
            let b = if msg == WM_LBUTTONDOWN { MouseButton::Left } else { MouseButton::Right };
            dispatch(hwnd, Event::MouseDown(lparam_pt(l), b));
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            let b = if msg == WM_LBUTTONUP { MouseButton::Left } else { MouseButton::Right };
            dispatch(hwnd, Event::MouseUp(lparam_pt(l), b));
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((w >> 16) & 0xFFFF) as u16 as i16 as i32;
            dispatch(hwnd, Event::Wheel(delta));
            0
        }
        WM_CAPTURECHANGED => {
            dispatch(hwnd, Event::CaptureLost);
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let handled = dispatch(hwnd, Event::Key(w as u16, mods()));
            if let Ok(mut d) = c.try_borrow_mut() {
                d.swallow_syschar = msg == WM_SYSKEYDOWN && handled;
            }
            if handled {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if dispatch(hwnd, Event::KeyUp(w as u16, mods())) {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_SYSCHAR => {
            let swallow = c.try_borrow().map(|d| d.swallow_syschar).unwrap_or(false);
            if swallow {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_CHAR => {
            if let Some(ch) = char::from_u32(w as u32) {
                if !ch.is_control() {
                    dispatch(hwnd, Event::Char(ch));
                }
            }
            0
        }
        WM_TIMER => {
            dispatch(hwnd, Event::Timer(w));
            0
        }
        WM_EDIT_DONE => {
            end_edit(hwnd, w != 0);
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = w as HDC;
            SetTextColor(dc, theme::TEXT.colorref());
            SetBkColor(dc, theme::RAISED.colorref());
            c.try_borrow().map(|d| d.edit_brush as LRESULT).unwrap_or(0)
        }
        WM_SETCURSOR => {
            if (l & 0xFFFF) as u32 == HTCLIENT {
                let cur = c.try_borrow().map(|d| d.cursor).unwrap_or(Cursor::Arrow);
                SetCursor(LoadCursorW(null_mut(), if cur == Cursor::Hand { IDC_HAND } else { IDC_ARROW }));
                return 1;
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_DESTROY => {
            dispatch(hwnd, Event::Destroyed);
            let kind = c.try_borrow().map(|d| d.kind).ok();
            if let Some(kind) = kind {
                OPEN.with(|o| {
                    let mut o = o.borrow_mut();
                    if o.get(kind) == Some(&(hwnd as isize)) {
                        o.remove(kind);
                    }
                });
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// Convert a screen point to client coordinates of `hwnd`.
pub fn screen_to_client(hwnd: HWND, p: Pt) -> Pt {
    let mut pt = POINT { x: p.x, y: p.y };
    unsafe { ScreenToClient(hwnd, &mut pt) };
    Pt::new(pt.x, pt.y)
}

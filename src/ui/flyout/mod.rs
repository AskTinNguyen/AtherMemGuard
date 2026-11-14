//! The tray flyout (docs/UI.md section 3): a 340 px borderless popup above the tray icon that
//! answers "is memory fine?" at a glance. Left-click the icon to open it; it closes on focus
//! loss or Esc.
//!
//! - `model`: `FlyoutData`, what it shows, built from the engine's `Status`, and each row's
//!   menu (pure).
//! - `view`: `layout` / `paint` / `hit` draw it into a `Canvas` (window and snapshots share
//!   them).
//! - Here: `show`, `update`, `close` drive the one popup window on the tray thread. Clicks and
//!   menu picks become `UiAction`s handed to the callback given to `show`; the flyout never
//!   acts itself.
//! - `snapshots(dir)` writes `flyout-*.png` from fixed sample data.

use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_SPACE, VK_TAB};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

mod model;
mod view;

pub use model::*;
pub use view::*;

use super::canvas::Canvas;
use super::commands::UiAction;
use super::geom::{Pt, Rect};
use super::theme::*;
use crate::win::wide;

/// Space between the flyout and the taskbar / screen edge.
pub const EDGE_GAP: f32 = 12.0;

// ---------------------------------------------------------------- placement

/// Where a `w` x `h` flyout goes for a tray icon at `anchor` on a monitor whose work area is
/// `work`: next to the taskbar the icon sits on, centred on the icon, inside the work area.
pub fn place(anchor: Rect, work: Rect, w: i32, h: i32, gap: i32) -> (i32, i32) {
    let clamp_x = |x: i32| x.clamp(work.x + gap, (work.right() - w - gap).max(work.x + gap));
    let clamp_y = |y: i32| y.clamp(work.y + gap, (work.bottom() - h - gap).max(work.y + gap));
    let (cx, cy) = (anchor.cx(), anchor.cy());
    if cy >= work.bottom() {
        (clamp_x(cx - w / 2), work.bottom() - h - gap)
    } else if cy < work.y {
        (clamp_x(cx - w / 2), work.y + gap)
    } else if cx < work.x {
        (work.x + gap, clamp_y(cy - h / 2))
    } else if cx >= work.right() {
        (work.right() - w - gap, clamp_y(cy - h / 2))
    } else {
        // Inside the work area (the overflow area, or no anchor): above the point.
        (clamp_x(cx - w / 2), clamp_y(anchor.y - h - gap))
    }
}

// ---------------------------------------------------------------- the window

const CLASS: &str = "AtherMemGuardFlyout";
const WM_MOUSELEAVE: u32 = 0x02A3;

struct Fly {
    hwnd: HWND,
    data: FlyoutData,
    canvas: Canvas,
    mouse: Option<Pt>,
    focus: Option<Target>,
    tracking: bool,
    on_command: Box<dyn Fn(UiAction)>,
}

thread_local! {
    static FLY: RefCell<Option<Fly>> = const { RefCell::new(None) };
    static CLOSED_AT: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

fn register() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let n = wide(CLASS);
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(fly_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(null()),
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: n.as_ptr(),
        };
        RegisterClassW(&wc);
    });
}

fn monitor_at(p: POINT) -> (Rect, Scale) {
    unsafe {
        let mon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) < 0 {
            dx = 96;
        }
        let r = mi.rcWork;
        (Rect::ltrb(r.left, r.top, r.right, r.bottom), Scale::from_dpi(dx))
    }
}

pub fn is_open() -> bool {
    FLY.with(|f| f.borrow().is_some())
}

/// True if the flyout closed less than `ms` ago (a click on the tray icon that closed it by
/// taking focus should not open it again at once).
pub fn closed_within(ms: u64) -> bool {
    CLOSED_AT.with(|c| c.borrow().is_some_and(|t| t.elapsed().as_millis() < ms as u128))
}

/// Open the flyout for a tray icon at `anchor` (screen rectangle; `None` = at the cursor).
/// Replaces an open one. Call on the tray thread.
pub fn show(data: FlyoutData, anchor: Option<Rect>, on_command: Box<dyn Fn(UiAction)>) {
    close();
    register();
    let anchor = anchor.unwrap_or_else(|| {
        let mut p = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut p) };
        Rect::new(p.x, p.y, 1, 1)
    });
    let (work, scale) = monitor_at(POINT { x: anchor.cx(), y: anchor.cy() });
    let canvas = Canvas::new(1, 1, scale);
    let l = layout(&canvas, &data);
    let (x, y) = place(anchor, work, l.width, l.height, scale.s(EDGE_GAP));
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let n = wide(CLASS);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            n.as_ptr(),
            wide("Ather MemGuard").as_ptr(),
            WS_POPUP,
            x,
            y,
            l.width,
            l.height,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        SetThreadDpiAwarenessContext(previous);
        if hwnd.is_null() {
            crate::log::write("could not open the tray flyout");
            return;
        }
        let round: i32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33 /* DWMWA_WINDOW_CORNER_PREFERENCE */, &round as *const i32 as *const _, 4);
        let border: u32 = BORDER.colorref();
        DwmSetWindowAttribute(hwnd, 34 /* DWMWA_BORDER_COLOR */, &border as *const u32 as *const _, 4);
        let dark: i32 = 1;
        DwmSetWindowAttribute(hwnd, 20 /* DWMWA_USE_IMMERSIVE_DARK_MODE */, &dark as *const i32 as *const _, 4);
        let mut canvas = canvas;
        canvas.resize(l.width, l.height);
        paint(&mut canvas, &data, None, None);
        FLY.with(|f| *f.borrow_mut() = Some(Fly { hwnd, data, canvas, mouse: None, focus: None, tracking: false, on_command }));
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

/// Show new data in the open flyout (once a second from the tray tick).
pub fn update(data: FlyoutData) {
    let resize = FLY.with(|f| {
        let mut f = f.borrow_mut();
        let fly = f.as_mut()?;
        if fly.data == data {
            return None;
        }
        fly.data = data;
        let l = layout(&fly.canvas, &fly.data);
        let changed = l.height != fly.canvas.height() || l.width != fly.canvas.width();
        if changed {
            fly.canvas.resize(l.width, l.height);
        }
        paint(&mut fly.canvas, &fly.data, fly.mouse, fly.focus);
        Some((fly.hwnd, changed, l.height))
    });
    if let Some((hwnd, changed, h)) = resize {
        unsafe {
            if changed {
                // Keep the bottom edge where it is (the flyout grows upwards from the taskbar).
                let mut rc: RECT = zeroed();
                GetWindowRect(hwnd, &mut rc);
                SetWindowPos(hwnd, null_mut(), rc.left, rc.bottom - h, rc.right - rc.left, h, SWP_NOZORDER | SWP_NOACTIVATE);
            }
            InvalidateRect(hwnd, null(), 0);
        }
    }
}

/// Close the flyout if it is open.
pub fn close() {
    let hwnd = FLY.with(|f| f.borrow_mut().take().map(|fly| fly.hwnd));
    if let Some(h) = hwnd {
        CLOSED_AT.with(|c| *c.borrow_mut() = Some(Instant::now()));
        unsafe { DestroyWindow(h) };
    }
}

fn repaint(hwnd: HWND) {
    FLY.with(|f| {
        if let Some(fly) = f.borrow_mut().as_mut() {
            paint(&mut fly.canvas, &fly.data, fly.mouse, fly.focus);
        }
    });
    unsafe { InvalidateRect(hwnd, null(), 0) };
}

/// Close, then run `t`'s command (outside the borrow: it may post or open windows).
fn activate(t: Target) {
    let cmd = FLY.with(|f| f.borrow().as_ref().map(|fly| t.command(&fly.data)));
    match cmd {
        Some(Some(c)) => finish(UiAction::Run(c)),
        Some(None) => open_menu(t),
        None => {}
    }
}

/// Close the flyout, then hand `action` to the callback.
fn finish(action: UiAction) {
    let picked = FLY.with(|f| f.borrow_mut().take().map(|fly| (fly.hwnd, fly.on_command)));
    if let Some((hwnd, cb)) = picked {
        CLOSED_AT.with(|c| *c.borrow_mut() = Some(Instant::now()));
        unsafe { DestroyWindow(hwnd) };
        cb(action);
    }
}

/// The popup menu of the band or a row, under it. Picking an item closes the flyout and runs
/// it; Esc or a click elsewhere leaves the flyout open.
fn open_menu(t: Target) {
    let Some((hwnd, items, anchor)) = FLY.with(|f| {
        f.borrow().as_ref().map(|fly| {
            let l = layout(&fly.canvas, &fly.data);
            (fly.hwnd, t.menu(&fly.data), t.rect(&l))
        })
    }) else {
        return;
    };
    if items.is_empty() {
        return;
    }
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        for (i, item) in items.iter().enumerate() {
            match item {
                MenuItem::Action(label, _) => {
                    AppendMenuW(menu, MF_STRING, i + 1, wide(label).as_ptr());
                }
                MenuItem::Info(text) => {
                    AppendMenuW(menu, MF_STRING | MF_GRAYED, i + 1, wide(text).as_ptr());
                }
                MenuItem::Separator => {
                    AppendMenuW(menu, MF_SEPARATOR, 0, null());
                }
            }
        }
        let mut pt = POINT { x: anchor.x + anchor.w / 6, y: anchor.bottom() };
        windows_sys::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
        let picked = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN, pt.x, pt.y, 0, hwnd, null());
        DestroyMenu(menu);
        if let Some(MenuItem::Action(_, a)) = (picked as usize).checked_sub(1).and_then(|i| items.get(i)) {
            finish(a.clone());
        } else {
            repaint(hwnd);
        }
    }
}

unsafe extern "system" fn fly_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let pt = || Pt::new((l & 0xFFFF) as u16 as i16 as i32, ((l >> 16) & 0xFFFF) as u16 as i16 as i32);
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            FLY.with(|f| {
                if let Some(fly) = f.borrow().as_ref() {
                    fly.canvas.blit(hdc, 0, 0);
                }
            });
            EndPaint(hwnd, &ps);
            return 0;
        }
        WM_ACTIVATE => {
            if (w & 0xFFFF) as u32 == WA_INACTIVE {
                close();
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            let p = pt();
            let over = FLY.with(|f| {
                let mut f = f.borrow_mut();
                let fly = f.as_mut()?;
                fly.mouse = Some(p);
                if !fly.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut tme);
                    fly.tracking = true;
                }
                Some(hit(&layout(&fly.canvas, &fly.data), p).is_some())
            });
            SetCursor(LoadCursorW(null_mut(), if over == Some(true) { IDC_HAND } else { IDC_ARROW }));
            repaint(hwnd);
            return 0;
        }
        WM_SETCURSOR => {
            // WM_MOUSEMOVE sets the cursor.
            if (l & 0xFFFF) as u32 == HTCLIENT {
                return 1;
            }
        }
        WM_MOUSELEAVE => {
            FLY.with(|f| {
                if let Some(fly) = f.borrow_mut().as_mut() {
                    fly.mouse = None;
                    fly.tracking = false;
                }
            });
            repaint(hwnd);
            return 0;
        }
        WM_LBUTTONUP => {
            let p = pt();
            let t = FLY.with(|f| f.borrow().as_ref().and_then(|fly| hit(&layout(&fly.canvas, &fly.data), p)));
            if let Some(t) = t {
                activate(t);
            }
            return 0;
        }
        WM_KEYDOWN => {
            let vk = w as u16;
            if vk == VK_ESCAPE {
                close();
            } else if vk == VK_TAB {
                let back = GetKeyState(VK_SHIFT as i32) < 0;
                FLY.with(|f| {
                    if let Some(fly) = f.borrow_mut().as_mut() {
                        let order = tab_order(&layout(&fly.canvas, &fly.data));
                        let n = order.len();
                        let i = fly.focus.and_then(|t| order.iter().position(|x| *x == t));
                        let next = match (i, back) {
                            (None, false) => 0,
                            (None, true) => n - 1,
                            (Some(i), false) => (i + 1) % n,
                            (Some(i), true) => (i + n - 1) % n,
                        };
                        fly.focus = Some(order[next]);
                    }
                });
                repaint(hwnd);
            } else if vk == VK_RETURN || vk == VK_SPACE {
                let t = FLY.with(|f| f.borrow().as_ref().and_then(|fly| fly.focus));
                if let Some(t) = t {
                    activate(t);
                }
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, w, l)
}

// ---------------------------------------------------------------- snapshots

/// Write `flyout-<state>.png` and `flyout-<state>@1.5x.png` into `dir`; returns the files.
pub fn snapshots(dir: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut out = Vec::new();
    for (scale, suffix) in [(Scale(1.0), ""), (Scale(1.5), "@1.5x")] {
        for (name, d) in samples() {
            let p = dir.join(format!("flyout-{}{}.png", name, suffix));
            render(&d, scale).save_png(&p)?;
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement() {
        let work = Rect::new(0, 0, 1920, 1032); // taskbar at the bottom, 48 px
        let icon = Rect::new(1700, 1040, 24, 24);
        assert_eq!(place(icon, work, 340, 500, 12), (1712 - 170, 1032 - 500 - 12));
        // Near the right edge it stays on screen.
        assert_eq!(place(Rect::new(1900, 1040, 16, 16), work, 340, 500, 12), (1920 - 340 - 12, 520));
        // Taskbar on top.
        let top = Rect::new(0, 48, 1920, 1032);
        assert_eq!(place(Rect::new(1700, 10, 24, 24), top, 340, 500, 12).1, 60);
        // Taskbar on the left.
        let left = Rect::new(60, 0, 1860, 1080);
        assert_eq!(place(Rect::new(10, 900, 24, 24), left, 340, 500, 12), (72, 1080 - 500 - 12));
        // Overflow area (inside the work area): above the icon.
        assert_eq!(place(Rect::new(1500, 900, 24, 24), work, 340, 300, 12), (1512 - 170, 900 - 300 - 12));
    }
}

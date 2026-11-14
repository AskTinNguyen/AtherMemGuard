//! The tray flyout (docs/UI.md section 3): a 340 px borderless popup above the tray icon that
//! answers "is memory fine?" at a glance. Left-click the icon to open it; it closes on focus
//! loss or Esc.
//!
//! - `FlyoutData` is what it shows, built from the engine's `Status` (pure).
//! - `layout` / `paint` / `hit` draw it into a `Canvas` (window and snapshots share them).
//! - `show`, `update`, `close` drive the one popup window on the tray thread. Clicks become
//!   `Command`s handed to the callback given to `show`; the flyout never acts itself.
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

use super::canvas::{Align, Canvas};
use super::commands::Command;
use super::geom::{Pt, Rect};
use super::kit::{usage_bar, Button, ButtonStyle, WState};
use super::theme::*;
use crate::config::Pressure;
use crate::engine::{Level, Status};
use crate::win::wide;

/// Width at 96 DPI.
pub const WIDTH: f32 = 340.0;
const PAD: f32 = 18.0;
/// Space between the flyout and the taskbar / screen edge.
pub const EDGE_GAP: f32 = 12.0;
const MAX_GROUPS: usize = 6;

const F_STATE: Font = Font::new(Face::Display, 30.0, REGULAR);
const F_METER: Font = Font::new(Face::Body, 12.0, REGULAR);
const F_METER_VALUE: Font = Font::new(Face::Body, 12.0, SEMIBOLD);
const F_UE: Font = Font::new(Face::Body, 14.0, SEMIBOLD);

// ---------------------------------------------------------------- data

#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    pub label: String,
    /// "45.1 of 128 GB"
    pub value: String,
    /// Share free, 0..1.
    pub frac: f32,
    pub color: Rgb,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupLine {
    pub name: String,
    pub frac: f32,
    pub color: Rgb,
    /// "4.1 GB"
    pub value: String,
}

/// Everything the flyout shows.
#[derive(Debug, Clone, PartialEq)]
pub struct FlyoutData {
    /// "EDITOR + BUILD", "IDLE"
    pub eyebrow: String,
    /// "MEMORY IS FINE", "MEMORY IS TIGHT", "MEMORY IS CRITICAL", "PAUSED"
    pub headline: String,
    pub headline_color: Rgb,
    pub ram: Meter,
    pub commit: Meter,
    /// "UnrealEditor · 43.8 GB" or "Unreal isn't running"
    pub ue_title: String,
    /// "Up 30 min · growing 2.1 GB an hour · 3 compile workers" (may be empty)
    pub ue_detail: String,
    pub groups: Vec<GroupLine>,
    /// One muted line above the buttons (at most two lines when painted).
    pub advice: Option<String>,
    pub paused: bool,
}

/// "editor+build" -> "Editor + build", "idle" -> "Idle".
pub fn mode_words(mode: &str) -> String {
    let s = mode.split('+').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" + ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "Idle".into(),
    }
}

/// The pressure in the UI's words.
pub fn pressure_words(level: Level) -> &'static str {
    match level {
        Level::Normal => "fine",
        Level::Elevated => "tight",
        Level::Critical => "critical",
    }
}

/// Colour of a free-memory meter: lime above the tight threshold, amber below it, red below
/// the critical one (percent free).
pub fn meter_color(free_pct: f64, tight_pct: f64, critical_pct: f64) -> Rgb {
    if free_pct < critical_pct {
        ERROR
    } else if free_pct < tight_pct {
        WARN
    } else {
        ACCENT
    }
}

fn gb_text(gb: f64) -> String {
    super::kit::format_gb(gb)
}

/// "Up 30 min", "Up 7 h 30 min".
pub fn uptime_words(min: u64) -> String {
    if min < 60 {
        format!("Up {} min", min)
    } else {
        format!("Up {} h {} min", min / 60, min % 60)
    }
}

impl FlyoutData {
    /// The flyout for `st`. `growth` is the editor's growth in GB an hour (from the memory
    /// history), `pause_note` the line shown while paused ("Other apps are back to normal
    /// until 15:42.").
    pub fn new(st: &Status, pressure: &Pressure, growth: Option<f64>, pause_note: Option<String>) -> FlyoutData {
        let sys = &st.system;
        let meter = |label: &str, free: f64, total: f64, pct: f64, tight: f64, critical: f64| Meter {
            label: label.into(),
            value: format!("{:.1} of {:.0} GB", free, total),
            frac: if total > 0.0 { (free / total).clamp(0.0, 1.0) as f32 } else { 0.0 },
            color: meter_color(pct, tight, critical),
        };
        let ram = meter("RAM free", sys.ram_free_gb, sys.ram_total_gb, sys.ram_free_pct, pressure.elevated_ram_free_pct, pressure.critical_ram_free_pct);
        let commit = meter(
            "Commit free",
            sys.commit_free_gb,
            sys.commit_limit_gb,
            sys.commit_free_pct,
            pressure.elevated_commit_free_pct,
            pressure.critical_commit_free_pct,
        );

        let workers = match st.ue.workers {
            0 => None,
            1 => Some("1 compile worker".to_string()),
            n => Some(format!("{} compile workers", n)),
        };
        let editor = st.ue.processes.iter().filter(|p| p.kind == "editor").max_by(|a, b| a.committed_gb.total_cmp(&b.committed_gb));
        let (ue_title, ue_detail) = match editor.or_else(|| st.ue.processes.first()) {
            Some(p) => {
                let mut parts = vec![uptime_words(p.uptime_min)];
                if p.kind == "editor" {
                    if let Some(g) = growth.filter(|g| *g >= 0.1) {
                        parts.push(format!("growing {:.1} GB an hour", g));
                    }
                }
                parts.extend(workers);
                (format!("{} · {}", p.name, gb_text(p.committed_gb)), parts.join(" · "))
            }
            None if st.activities.any() => (format!("Unreal {} running", mode_words(&st.mode).to_lowercase()), workers.unwrap_or_default()),
            None => ("Unreal isn't running".to_string(), String::new()),
        };

        let groups = st
            .groups
            .iter()
            .take(MAX_GROUPS)
            .map(|g| {
                let bar = usage_bar(g.ram_mb, g.policy.trim_above_mb);
                GroupLine { name: g.name.clone(), frac: bar.frac, color: bar.color, value: gb_text(g.ram_mb as f64 / 1024.0) }
            })
            .collect();

        let (headline, headline_color) = if st.paused {
            ("PAUSED".to_string(), TEXT)
        } else {
            match st.pressure {
                Level::Normal => ("MEMORY IS FINE".to_string(), TEXT),
                Level::Elevated => ("MEMORY IS TIGHT".to_string(), WARN),
                Level::Critical => ("MEMORY IS CRITICAL".to_string(), ERROR),
            }
        };
        let advice = if st.paused { pause_note.or_else(|| st.advice.first().cloned()) } else { st.advice.first().cloned() };
        FlyoutData {
            eyebrow: mode_words(&st.mode).to_uppercase(),
            headline,
            headline_color,
            ram,
            commit,
            ue_title,
            ue_detail,
            groups,
            advice,
            paused: st.paused,
        }
    }

    /// The three buttons: label, command, primary.
    pub fn buttons(&self) -> [(&'static str, Command); 3] {
        [
            ("Free memory", Command::FreeMemory),
            if self.paused { ("Resume", Command::Resume) } else { ("Pause 30 min", Command::Pause30) },
            ("Settings", Command::OpenSettings),
        ]
    }
}

// ---------------------------------------------------------------- layout and paint

/// Where everything goes, in device pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub eyebrow: Rect,
    pub headline: Rect,
    /// (label row, bar) for RAM and commit.
    pub meters: [(Rect, Rect); 2],
    /// The full-width Unreal band and its two lines.
    pub ue_band: Rect,
    pub ue_title: Rect,
    pub ue_detail: Option<Rect>,
    pub ue_detail_lines: Vec<String>,
    /// (name, bar, value) per group.
    pub groups: Vec<(Rect, Rect, Rect)>,
    /// The "Run a checkup" link (docs/UI.md section 6).
    pub checkup: Rect,
    pub advice: Option<Rect>,
    pub advice_lines: Vec<String>,
    pub actions_band: Rect,
    pub buttons: [Rect; 3],
}

pub const CHECKUP_LABEL: &str = "Run a checkup";

/// At most two lines, breaking between " · " parts rather than inside one when it can.
fn wrap_segments(c: &Canvas, s: &str, f: Font, width: i32) -> Vec<String> {
    if c.measure(s, f).0 <= width {
        return vec![s.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    for seg in s.split(" · ") {
        match lines.last_mut() {
            Some(cur) if c.measure(&format!("{} · {}", cur, seg), f).0 <= width => *cur = format!("{} · {}", cur, seg),
            _ => lines.push(seg.to_string()),
        }
    }
    if lines.len() > 2 || lines.iter().any(|l| c.measure(l, f).0 > width) {
        return c.wrap_lines(s, f, width, 2);
    }
    lines
}

pub fn layout(c: &Canvas, d: &FlyoutData) -> Layout {
    let w = c.s(WIDTH);
    let pad = c.s(PAD);
    let inner = w - 2 * pad;
    let mut y = pad;

    let eh = c.line_height(F_EYEBROW);
    let eyebrow = Rect::new(pad, y, inner, eh);
    y += eh + c.s(4.0);
    let hh = c.line_height(F_STATE);
    let headline = Rect::new(pad, y, inner, hh);
    y += hh + c.s(14.0) - c.s(10.0); // the first meter's top margin follows

    let meter = |y: &mut i32| {
        *y += c.s(10.0);
        let lh = c.line_height(F_METER).max(c.line_height(F_METER_VALUE));
        let label = Rect::new(pad, *y, inner, lh);
        *y += lh + c.s(4.0);
        let bar = Rect::new(pad, *y, inner, c.s(6.0));
        *y += c.s(6.0);
        (label, bar)
    };
    let m1 = meter(&mut y);
    let m2 = meter(&mut y);
    y += c.s(14.0);

    let ue_top = y;
    y += c.s(12.0);
    let th = c.line_height(F_UE);
    let ue_title = Rect::new(pad, y, inner, th);
    y += th;
    // Wraps to a second line when it is long ("Up 7 h 30 min · growing 9.4 GB an hour · …").
    let ue_detail_lines = if d.ue_detail.is_empty() { Vec::new() } else { wrap_segments(c, &d.ue_detail, F_HINT, inner) };
    let ue_detail = if ue_detail_lines.is_empty() {
        None
    } else {
        let dh = c.line_height(F_HINT) * ue_detail_lines.len() as i32;
        let r = Rect::new(pad, y, inner, dh);
        y += dh;
        Some(r)
    };
    y += c.s(12.0);
    let ue_band = Rect::new(0, ue_top, w, y - ue_top);
    y += c.s(14.0) - c.s(6.0);

    let gh = c.line_height(F_METER).max(c.s(16.0));
    let name_w = c.s(90.0);
    let value_w = c.s(64.0);
    let gap = c.s(8.0);
    let mut groups = Vec::new();
    for _ in &d.groups {
        y += c.s(6.0);
        let name = Rect::new(pad, y, name_w, gh);
        let value = Rect::new(pad + inner - value_w, y, value_w, gh);
        let bx = pad + name_w + gap;
        let bar = Rect::new(bx, y + (gh - c.s(6.0)) / 2, value.x - gap - bx, c.s(6.0));
        groups.push((name, bar, value));
        y += gh;
    }
    y += c.s(10.0);
    let (lw, lh) = c.measure(CHECKUP_LABEL, F_HINT);
    let checkup = Rect::new(pad, y, lw + c.s(2.0), lh + c.s(2.0));
    y += lh + c.s(2.0);

    let advice_lines = match &d.advice {
        Some(a) if !a.trim().is_empty() => c.wrap_lines(a.trim(), F_HINT, inner, 2),
        _ => Vec::new(),
    };
    let advice = if advice_lines.is_empty() {
        None
    } else {
        y += c.s(10.0);
        let h = advice_lines.len() as i32 * c.line_height(F_HINT);
        let r = Rect::new(pad, y, inner, h);
        y += h;
        Some(r)
    };
    y += c.s(14.0);

    let band_top = y;
    let bh = c.s(34.0);
    let bpad = c.s(12.0);
    let bgap = c.s(8.0);
    // The row is filled like the mockup's flex row, but the spare room is shared out equally
    // as padding so a longer label ("Pause 30 min") never touches its edges.
    let labels = d.buttons().map(|(label, _)| c.measure(label, F_BUTTON).0);
    let spare = (inner - 2 * bgap - labels.iter().sum::<i32>()).max(0);
    let by = band_top + bpad;
    let mut x = pad;
    let mut buttons = [Rect::default(); 3];
    for (i, lw) in labels.iter().enumerate() {
        let w = if i == 2 { pad + inner - x } else { lw + spare / 3 };
        buttons[i] = Rect::new(x, by, w, bh);
        x += w + bgap;
    }
    let height = by + bh + bpad;
    Layout {
        width: w,
        height,
        eyebrow,
        headline,
        meters: [m1, m2],
        ue_band,
        ue_title,
        ue_detail,
        ue_detail_lines,
        groups,
        checkup,
        advice,
        advice_lines,
        actions_band: Rect::new(0, band_top, w, height - band_top),
        buttons,
    }
}

/// Things that can be clicked or focused, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Checkup,
    Button(usize),
}

pub const TAB_ORDER: [Target; 4] = [Target::Button(0), Target::Button(1), Target::Button(2), Target::Checkup];

impl Target {
    pub fn command(self, d: &FlyoutData) -> Command {
        match self {
            Target::Checkup => Command::OpenDoctor,
            Target::Button(i) => d.buttons()[i.min(2)].1,
        }
    }
    fn rect(self, l: &Layout) -> Rect {
        match self {
            Target::Checkup => l.checkup,
            Target::Button(i) => l.buttons[i.min(2)],
        }
    }
}

pub fn hit(l: &Layout, p: Pt) -> Option<Target> {
    TAB_ORDER.iter().copied().find(|t| t.rect(l).contains(p))
}

fn paint_bar(c: &mut Canvas, r: Rect, frac: f32, color: Rgb) {
    let rad = r.h / 2;
    c.fill_round_rect(r, rad, RAISED);
    let fw = (r.w as f32 * frac.clamp(0.0, 1.0)).round() as i32;
    if fw > 0 {
        c.fill_round_rect(Rect::new(r.x, r.y, fw.max(r.h), r.h), rad, color);
    }
}

/// Paint the flyout filling the canvas (sized `layout().width` x `layout().height`).
pub fn paint(c: &mut Canvas, d: &FlyoutData, mouse: Option<Pt>, focus: Option<Target>) {
    let l = layout(c, d);
    c.fill_rect(c.bounds(), BG);
    c.spaced_text(l.eyebrow, &d.eyebrow, F_EYEBROW, MUTED, c.s(3.0), Align::Left);
    c.text(l.headline, &c.ellipsize(&d.headline, F_STATE, l.headline.w), F_STATE, d.headline_color, Align::Left);

    for (m, (label, bar)) in [&d.ram, &d.commit].into_iter().zip(l.meters.iter()) {
        c.text(*label, &m.label, F_METER, MUTED, Align::Left);
        c.text(*label, &m.value, F_METER_VALUE, TEXT, Align::Right);
        paint_bar(c, *bar, m.frac, m.color);
    }

    c.fill_rect(l.ue_band, SURFACE);
    c.hline(0, l.ue_band.y, l.width, BORDER);
    c.hline(0, l.ue_band.bottom() - c.px1(1.0), l.width, BORDER);
    c.text(l.ue_title, &d.ue_title, F_UE, TEXT, Align::Left);
    if let Some(r) = l.ue_detail {
        let lh = c.line_height(F_HINT);
        for (i, line) in l.ue_detail_lines.iter().enumerate() {
            c.text_top(Rect::new(r.x, r.y + i as i32 * lh, r.w, lh), line, F_HINT, MUTED, Align::Left);
        }
    }

    for (g, (name, bar, value)) in d.groups.iter().zip(l.groups.iter()) {
        c.text(*name, &g.name, F_METER, TEXT, Align::Left);
        paint_bar(c, *bar, g.frac, g.color);
        c.text(*value, &g.value, F_METER, MUTED, Align::Right);
    }

    let hover = |t: Target| mouse.is_some_and(|m| hit(&l, m) == Some(t));
    // A left-aligned link in the hint size (kit's Link button is centred and 13 px).
    let col = if hover(Target::Checkup) { TEXT } else { MUTED };
    c.text(l.checkup, CHECKUP_LABEL, F_HINT, col, Align::Left);
    let (tw, th) = c.measure(CHECKUP_LABEL, F_HINT);
    c.hline(l.checkup.x, l.checkup.y + th - c.s(1.0), tw, col);
    if focus == Some(Target::Checkup) {
        super::kit::focus_ring(c, l.checkup, c.s(3.0));
    }

    if let Some(r) = l.advice {
        let lh = c.line_height(F_HINT);
        for (i, line) in l.advice_lines.iter().enumerate() {
            c.text_top(Rect::new(r.x, r.y + i as i32 * lh, r.w, lh), line, F_HINT, MUTED, Align::Left);
        }
    }

    c.fill_rect(l.actions_band, SURFACE);
    c.hline(0, l.actions_band.y, l.width, BORDER);
    for (i, (label, _)) in d.buttons().iter().enumerate() {
        let style = if i == 0 { ButtonStyle::Primary } else { ButtonStyle::Normal };
        let st = WState { hover: hover(Target::Button(i)), focused: focus == Some(Target::Button(i)), ..Default::default() };
        Button::new(label, style).paint(c, l.buttons[i], &st);
    }
}

/// A canvas with the flyout painted on it and a hairline frame (snapshots).
pub fn render(d: &FlyoutData, scale: Scale) -> Canvas {
    let probe = Canvas::new(1, 1, scale);
    let l = layout(&probe, d);
    drop(probe);
    let mut c = Canvas::new(l.width, l.height, scale);
    paint(&mut c, d, None, None);
    let b = c.bounds();
    c.stroke_round_rect(b, 0, BORDER, c.px1(1.0));
    c
}

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
    on_command: Box<dyn Fn(Command)>,
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
pub fn show(data: FlyoutData, anchor: Option<Rect>, on_command: Box<dyn Fn(Command)>) {
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
    let picked = FLY.with(|f| f.borrow_mut().take().map(|fly| (fly.hwnd, t.command(&fly.data), fly.on_command)));
    if let Some((hwnd, cmd, cb)) = picked {
        CLOSED_AT.with(|c| *c.borrow_mut() = Some(Instant::now()));
        unsafe { DestroyWindow(hwnd) };
        cb(cmd);
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
                        let n = TAB_ORDER.len();
                        let i = fly.focus.and_then(|t| TAB_ORDER.iter().position(|x| *x == t));
                        let next = match (i, back) {
                            (None, false) => 0,
                            (None, true) => n - 1,
                            (Some(i), false) => (i + 1) % n,
                            (Some(i), true) => (i + n - 1) % n,
                        };
                        fly.focus = Some(TAB_ORDER[next]);
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

/// The four sample states (snapshot name, data), from the mockup's numbers.
pub fn samples() -> Vec<(&'static str, FlyoutData)> {
    use super::status_source::sample_status;
    let pressure = crate::config::Config::builtin().pressure;
    let normal = sample_status();

    let mut tight = sample_status();
    tight.pressure = Level::Elevated;
    tight.system.ram_free_gb = 11.3;
    tight.system.ram_free_pct = 8.8;
    tight.system.commit_free_gb = 21.0;
    tight.system.commit_free_pct = 14.6;
    tight.ue.processes[0].committed_gb = 58.2;
    tight.ue.processes[0].uptime_min = 245;

    let mut critical = sample_status();
    critical.pressure = Level::Critical;
    critical.system.ram_free_gb = 4.1;
    critical.system.ram_free_pct = 3.2;
    critical.system.commit_free_gb = 6.2;
    critical.system.commit_free_pct = 4.3;
    critical.ue.processes[0].committed_gb = 71.4;
    critical.ue.processes[0].uptime_min = 450;
    critical.advice = vec!["Memory is critical: 4.1 GB RAM free, 6.2 GB commit free. Don't start a build or cook until this clears.".into()];

    let mut paused = sample_status();
    paused.paused = true;

    vec![
        ("normal", FlyoutData::new(&normal, &pressure, Some(2.1), None)),
        ("tight", FlyoutData::new(&tight, &pressure, Some(6.8), None)),
        ("critical", FlyoutData::new(&critical, &pressure, Some(9.4), None)),
        ("paused", FlyoutData::new(&paused, &pressure, Some(2.1), Some("Other apps are back to normal until 15:42.".into()))),
    ]
}

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

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::status_source::sample_status;

    fn data() -> FlyoutData {
        samples().remove(0).1
    }

    #[test]
    fn words_and_colours() {
        assert_eq!(mode_words("editor+build"), "Editor + build");
        assert_eq!(mode_words("idle"), "Idle");
        assert_eq!(mode_words(""), "Idle");
        assert_eq!(pressure_words(Level::Elevated), "tight");
        assert_eq!(meter_color(35.0, 10.0, 4.0), ACCENT);
        assert_eq!(meter_color(8.0, 10.0, 4.0), WARN);
        assert_eq!(meter_color(3.0, 10.0, 4.0), ERROR);
        assert_eq!(uptime_words(30), "Up 30 min");
        assert_eq!(uptime_words(450), "Up 7 h 30 min");
    }

    #[test]
    fn data_from_status() {
        let d = data();
        assert_eq!(d.eyebrow, "EDITOR + BUILD");
        assert_eq!(d.headline, "MEMORY IS FINE");
        assert_eq!(d.ram.value, "45.1 of 128 GB");
        assert_eq!(d.commit.value, "27.6 of 144 GB");
        assert_eq!(d.ue_title, "UnrealEditor · 43.8 GB");
        assert_eq!(d.ue_detail, "Up 30 min · growing 2.1 GB an hour · 3 compile workers");
        assert_eq!(d.groups.len(), 4);
        assert_eq!(d.groups[0].value, "4.1 GB");
        assert_eq!(d.groups[3].value, "819 MB");
        assert_eq!(d.groups[2].color, MUTED, "Agents has no cap");
        assert_eq!(d.groups[3].color, WARN, "Background is over its cap");
        assert_eq!(d.buttons()[1], ("Pause 30 min", Command::Pause30));
        assert!(d.advice.is_none());

        let (_, crit) = samples().remove(2);
        assert_eq!((crit.headline.as_str(), crit.headline_color), ("MEMORY IS CRITICAL", ERROR));
        assert_eq!(crit.ram.color, ERROR);
        assert!(crit.advice.is_some());
        let (_, p) = samples().remove(3);
        assert_eq!(p.headline, "PAUSED");
        assert_eq!(p.buttons()[1], ("Resume", Command::Resume));
        assert_eq!(p.advice.as_deref(), Some("Other apps are back to normal until 15:42."));

        let mut idle = sample_status();
        idle.mode = "idle".into();
        idle.activities = Default::default();
        idle.ue = Default::default();
        let d = FlyoutData::new(&idle, &crate::config::Config::builtin().pressure, None, None);
        assert_eq!((d.eyebrow.as_str(), d.ue_title.as_str(), d.ue_detail.as_str()), ("IDLE", "Unreal isn't running", ""));
    }

    #[test]
    fn layout_fits_at_two_scales() {
        for (name, d) in samples() {
            for scale in [Scale(1.0), Scale(1.5)] {
                let c = Canvas::new(1, 1, scale);
                let l = layout(&c, &d);
                let bounds = Rect::new(0, 0, l.width, l.height);
                assert_eq!(l.width, scale.s(WIDTH));
                let mut rects = vec![l.eyebrow, l.headline, l.ue_band, l.ue_title, l.checkup, l.actions_band];
                rects.extend(l.meters.iter().flat_map(|(a, b)| [*a, *b]));
                rects.extend(l.groups.iter().flat_map(|(a, b, v)| [*a, *b, *v]));
                rects.extend(l.buttons);
                rects.extend(l.advice);
                rects.extend(l.ue_detail);
                for r in &rects {
                    assert!(bounds.encloses(r) && !r.is_empty(), "{} at {:?}: {:?} outside {:?}", name, scale, r, bounds);
                }
                // Buttons sit in the band, side by side, and their labels fit.
                for (i, b) in l.buttons.iter().enumerate() {
                    assert!(l.actions_band.encloses(b));
                    assert!(c.measure(d.buttons()[i].0, F_BUTTON).0 + c.s(8.0) <= b.w, "{} label {} clips", name, i);
                    if i > 0 {
                        assert!(l.buttons[i - 1].right() < b.x);
                    }
                }
                // Top to bottom, nothing overlaps.
                let order = [l.headline.bottom() <= l.meters[0].0.y, l.meters[0].1.bottom() <= l.meters[1].0.y, l.meters[1].1.bottom() <= l.ue_band.y];
                assert!(order.iter().all(|x| *x), "{} order", name);
                assert!(l.groups.last().map_or(l.ue_band.bottom(), |g| g.0.bottom()) <= l.checkup.y);
                assert!(l.advice.map_or(l.checkup.bottom(), |a| a.bottom()) <= l.actions_band.y);
                assert!(c.measure(&d.headline, F_STATE).0 <= l.headline.w, "{} headline clips", name);
                for line in l.ue_detail_lines.iter().chain(&l.advice_lines) {
                    assert!(c.measure(line, F_HINT).0 <= l.inner_width() && !line.ends_with('…'), "{} line clips: {}", name, line);
                }
                // Tab order reaches every target.
                for t in TAB_ORDER {
                    assert!(hit(&l, Pt::new(t.rect(&l).cx(), t.rect(&l).cy())) == Some(t));
                }
            }
            let l1 = layout(&Canvas::new(1, 1, Scale(1.0)), &d);
            let l15 = layout(&Canvas::new(1, 1, Scale(1.5)), &d);
            let ratio = l15.height as f32 / l1.height as f32;
            assert!((1.4..1.6).contains(&ratio), "{} height scales: {}", name, ratio);
        }
    }

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

    #[test]
    fn renders_with_the_theme() {
        let c = render(&data(), Scale(1.0));
        let l = layout(&c, &data());
        assert_eq!(c.pixel(l.width / 2, l.eyebrow.y - 4), BG);
        assert_eq!(c.pixel(4, l.ue_band.cy()), SURFACE);
        assert_eq!(c.pixel(l.buttons[0].x + 3, l.buttons[0].cy()), ACCENT);
    }
}

impl Layout {
    /// Content width inside the padding.
    pub fn inner_width(&self) -> i32 {
        self.eyebrow.w
    }
}

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
use super::kit::{Button, ButtonStyle, WState};
use super::theme::*;
use crate::config::Pressure;
use crate::engine::{Level, Status};
use crate::win::wide;

/// Width at 96 DPI.
pub const WIDTH: f32 = 340.0;
const PAD: f32 = 18.0;
/// Space between the flyout and the taskbar / screen edge.
pub const EDGE_GAP: f32 = 12.0;

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

/// One part of the "where your RAM is" bar: a share of total RAM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub frac: f32,
    pub color: Rgb,
}

/// One line of the RAM list. A `sub` line names one app inside "Other apps" (no swatch,
/// indented).
#[derive(Debug, Clone, PartialEq)]
pub struct RamRow {
    pub name: String,
    /// "4.1 GB"
    pub value: String,
    pub swatch: Rgb,
    pub sub: bool,
    /// The "Empty it now" link under "Windows file cache" (a sub row with no value).
    pub link: bool,
}

/// Everything the flyout shows.
#[derive(Debug, Clone, PartialEq)]
pub struct FlyoutData {
    /// "EDITOR + BUILD", "IDLE"
    pub eyebrow: String,
    /// "MEMORY IS FINE", "MEMORY IS TIGHT", "MEMORY IS CRITICAL", "PAUSED"
    pub headline: String,
    pub headline_color: Rgb,
    /// "92.6 of 96 GB"
    pub ram_in_use: String,
    /// "3.4 GB free", coloured like a free-memory meter.
    pub ram_free: String,
    pub ram_free_color: Rgb,
    /// The stacked bar, in the order of `rows` (Unreal first). The empty rest is free.
    pub segments: Vec<Segment>,
    /// "Unreal · 52.9 GB" or "Unreal isn't running"
    pub ue_title: String,
    /// Lines under the title, each wrapped on its own: what Unreal's RAM is, then
    /// "Up 30 min · growing 2.1 GB an hour · Windows promised it 47.0 GB".
    pub ue_detail: Vec<String>,
    /// Ranked, largest first; they add up to the RAM in use with Unreal.
    pub rows: Vec<RamRow>,
    /// When memory is critical: what to close and what it gets back. Shown in a red-edged box
    /// whose link opens the checkup.
    pub fix: Option<String>,
    pub commit: Meter,
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

/// Swatch colours of the RAM parts (docs/UI.md section 3): lime is Unreal, off-white is
/// what you could close yourself, olive is what MemGuard already holds back, the greys are
/// Windows' own.
pub const SW_UNREAL: Rgb = ACCENT;
pub const SW_OTHER: Rgb = TEXT;
pub const SW_GROUP: Rgb = Rgb(159, 167, 122);
pub const SW_CACHE: Rgb = Rgb(112, 136, 146);
pub const SW_WINDOWS: Rgb = Rgb(124, 125, 117);
pub const SW_SQUEEZED: Rgb = Rgb(90, 91, 84);
pub const SW_SHARED: Rgb = Rgb(68, 70, 63);

/// A group gets its own row from this size; smaller groups share one.
const GROUP_ROW_GB: f64 = 1.0;
/// The critical box mentions the file cache from this size.
const CACHE_IN_FIX_FROM_GB: f64 = 2.0;
/// "Empty it now" shows under "Windows file cache" from this size.
const EMPTY_CACHE_FROM_GB: f64 = 1.0;
pub const EMPTY_CACHE_LABEL: &str = "Empty it now (needs admin)";
/// How many other apps are named.
const NAMED_APPS: usize = 3;
/// Other apps are named from this size, or whenever memory is tight.
const NAME_APPS_FROM_GB: f64 = 1.0;

/// A friendlier name for a few executables whose names say little.
pub fn app_words(exe: &str) -> String {
    match exe.to_ascii_lowercase().as_str() {
        "vmmem" | "vmmemwsl" => "Linux VMs (WSL)".into(),
        "rider64" => "Rider".into(),
        "devenv" => "Visual Studio".into(),
        "code" => "VS Code".into(),
        "msedge" => "Edge".into(),
        "chrome" => "Chrome".into(),
        "firefox" => "Firefox".into(),
        "zenserver" => "Unreal Zen server".into(),
        _ => exe.to_string(),
    }
}

/// "A, B and C"
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

impl FlyoutData {
    /// The flyout for `st`. `growth` is the editor's growth in GB an hour (from the memory
    /// history), `pause_note` the line shown while paused ("Other apps are back to normal
    /// until 15:42.").
    pub fn new(st: &Status, pressure: &Pressure, growth: Option<f64>, pause_note: Option<String>) -> FlyoutData {
        let sys = &st.system;
        let br = &st.ram;
        let commit = Meter {
            label: "Commit free".into(),
            value: format!("{:.1} of {:.0} GB", sys.commit_free_gb, sys.commit_limit_gb),
            frac: if sys.commit_limit_gb > 0.0 { (sys.commit_free_gb / sys.commit_limit_gb).clamp(0.0, 1.0) as f32 } else { 0.0 },
            color: meter_color(sys.commit_free_pct, pressure.elevated_commit_free_pct, pressure.critical_commit_free_pct),
        };
        let in_use = if br.in_use_gb > 0.0 { br.in_use_gb } else { (sys.ram_total_gb - sys.ram_free_gb).max(0.0) };

        // Unreal.
        let workers = match st.ue.workers {
            0 => None,
            1 => Some("1 compile worker".to_string()),
            n => Some(format!("{} compile workers", n)),
        };
        let editor = st.ue.processes.iter().filter(|p| p.kind == "editor").max_by(|a, b| a.committed_gb.total_cmp(&b.committed_gb));
        let running = editor.is_some() || !st.ue.processes.is_empty() || st.activities.any();
        let ue_title = if running { format!("Unreal · {}", gb_text(br.unreal_gb)) } else { "Unreal isn't running".to_string() };
        let mut ue_detail = Vec::new();
        if running {
            let mut split = Vec::new();
            if br.unreal_editor_gb > 0.0 {
                split.push(format!("Editor {}", gb_text(br.unreal_editor_gb)));
            }
            if let Some(w) = &workers {
                split.push(format!("{} {}", w, gb_text(br.unreal_workers_gb)));
            }
            let rest = br.unreal_gb - br.unreal_editor_gb - br.unreal_workers_gb;
            if rest >= 0.05 && !split.is_empty() {
                split.push(format!("other {}", gb_text(rest)));
            }
            if !split.is_empty() {
                ue_detail.push(split.join(" · "));
            }
            let mut parts = Vec::new();
            if let Some(p) = editor.or_else(|| st.ue.processes.first()) {
                parts.push(uptime_words(p.uptime_min));
                if p.kind == "editor" {
                    if let Some(g) = growth.filter(|g| *g >= 0.1) {
                        parts.push(format!("growing {:.1} GB an hour", g));
                    }
                }
            }
            if st.ue.total_committed_gb > 0.0 {
                parts.push(format!("Windows promised it {}", gb_text(st.ue.total_committed_gb)));
            }
            if !parts.is_empty() {
                ue_detail.push(parts.join(" · "));
            }
        }

        // The other parts, as (name, GB, swatch, its sub-rows). Ranked by the size shown (0.1 GB
        // steps), so the order always matches the numbers; ties keep this order.
        let tight = st.pressure >= Level::Elevated;
        let mut parts: Vec<(String, f64, Rgb, Vec<RamRow>)> = Vec::new();
        let mut other_rows = Vec::new();
        if tight || br.other_apps_gb >= NAME_APPS_FROM_GB {
            let named: Vec<_> = br.top_apps.iter().filter(|a| a.ram_gb >= 0.1).take(NAMED_APPS).collect();
            for a in &named {
                other_rows.push(RamRow { name: app_words(&a.name), value: gb_text(a.ram_gb), swatch: SW_OTHER, sub: true, link: false });
            }
            let rest_n = br.other_apps_count.saturating_sub(named.len());
            let rest_gb = br.other_apps_gb - named.iter().map(|a| a.ram_gb).sum::<f64>();
            if !named.is_empty() && rest_n > 0 {
                let what = if rest_n == 1 { "1 smaller app".to_string() } else { format!("{} smaller apps", rest_n) };
                other_rows.push(RamRow { name: what, value: gb_text(rest_gb.max(0.0)), swatch: SW_OTHER, sub: true, link: false });
            }
        }
        parts.push(("Other apps".into(), br.other_apps_gb, SW_OTHER, other_rows));
        let (big, small): (Vec<_>, Vec<_>) = br.groups.iter().partition(|g| g.ram_gb >= GROUP_ROW_GB);
        for g in big {
            parts.push((g.name.clone(), g.ram_gb, SW_GROUP, Vec::new()));
        }
        if !small.is_empty() {
            let names: Vec<String> = small.iter().map(|g| g.name.clone()).collect();
            parts.push((names.join(", "), small.iter().map(|g| g.ram_gb).sum(), SW_GROUP, Vec::new()));
        }
        let empty_link = if br.file_cache_gb >= EMPTY_CACHE_FROM_GB {
            vec![RamRow { name: EMPTY_CACHE_LABEL.into(), value: String::new(), swatch: SW_CACHE, sub: true, link: true }]
        } else {
            Vec::new()
        };
        parts.push(("Windows file cache".into(), br.file_cache_gb, SW_CACHE, empty_link));
        parts.push(("Windows and drivers".into(), br.windows_gb, SW_WINDOWS, Vec::new()));
        parts.push(("Squeezed by Windows".into(), br.compressed_gb, SW_SQUEEZED, Vec::new()));
        parts.push(("Shared or not itemized".into(), br.unattributed_gb, SW_SHARED, Vec::new()));
        // Stable sort: equal sizes keep the order above.
        parts.sort_by_key(|p| std::cmp::Reverse((p.1 * 10.0).round() as i64));

        let total = sys.ram_total_gb.max(in_use);
        let share = |gb: f64| if total > 0.0 { (gb / total).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let mut segments = vec![Segment { frac: share(br.unreal_gb), color: SW_UNREAL }];
        let mut rows = Vec::new();
        for (name, gb, swatch, subs) in parts {
            segments.push(Segment { frac: share(gb), color: swatch });
            rows.push(RamRow { name, value: gb_text(gb), swatch, sub: false, link: false });
            rows.extend(subs);
        }
        segments.retain(|s| s.frac > 0.0);

        let (headline, headline_color) = if st.paused {
            ("PAUSED".to_string(), TEXT)
        } else {
            match st.pressure {
                Level::Normal => ("MEMORY IS FINE".to_string(), TEXT),
                Level::Elevated => ("MEMORY IS TIGHT".to_string(), WARN),
                Level::Critical => ("MEMORY IS CRITICAL".to_string(), ERROR),
            }
        };

        // Critical: say what closing the biggest other apps gets back. The engine's own
        // "Memory is critical" advice says the same numbers again, so it gives way.
        let fix = (!st.paused && st.pressure == Level::Critical).then(|| {
            let named: Vec<_> = br.top_apps.iter().filter(|a| a.ram_gb >= 0.5).take(NAMED_APPS).collect();
            let names: Vec<String> = named.iter().map(|a| app_words(&a.name)).collect();
            let back: f64 = named.iter().map(|a| a.ram_gb).sum();
            let cache = br.file_cache_gb >= CACHE_IN_FIX_FROM_GB;
            match (cache, named.is_empty()) {
                (false, true) => "Don't start a build or cook until this clears.".to_string(),
                (false, false) => format!("Closing {} gets about {:.1} GB back. Don't start a build or cook yet.", and_list(&names), back),
                (true, true) => format!("Emptying Windows' file cache gets about {:.1} GB back. Don't start a build or cook yet.", br.file_cache_gb),
                (true, false) => format!(
                    "Emptying Windows' file cache gets about {:.1} GB back, closing {} about {:.1} GB. Don't start a build or cook yet.",
                    br.file_cache_gb,
                    and_list(&names),
                    back
                ),
            }
        });
        let engine_advice = || st.advice.iter().find(|a| fix.is_none() || !a.starts_with("Memory is critical")).cloned();
        let advice = if st.paused { pause_note.or_else(engine_advice) } else { engine_advice() };
        FlyoutData {
            eyebrow: mode_words(&st.mode).to_uppercase(),
            headline,
            headline_color,
            ram_in_use: format!("{:.1} of {:.0} GB", in_use, sys.ram_total_gb),
            ram_free: format!("{} free", gb_text(sys.ram_free_gb)),
            ram_free_color: meter_color(sys.ram_free_pct, pressure.elevated_ram_free_pct, pressure.critical_ram_free_pct),
            segments,
            ue_title,
            ue_detail,
            rows,
            fix,
            commit,
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

    /// The link that opens the checkup: inside the red box when there is one.
    pub fn checkup_label(&self) -> &'static str {
        if self.fix.is_some() {
            FIX_LABEL
        } else {
            CHECKUP_LABEL
        }
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
    /// "RAM in use … · … free" and the stacked bar under it.
    pub ram_label: Rect,
    pub ram_bar: Rect,
    /// The full-width Unreal band and its lines.
    pub ue_band: Rect,
    pub ue_title: Rect,
    pub ue_detail: Option<Rect>,
    pub ue_detail_lines: Vec<String>,
    /// (swatch, name, value) per row; sub rows have an empty swatch.
    pub rows: Vec<(Rect, Rect, Rect)>,
    /// The "Empty it now" link, when the file cache row has one.
    pub empty_cache: Option<Rect>,
    /// The red-edged box and its wrapped text, when there is a fix.
    pub fix_box: Option<Rect>,
    pub fix_lines: Vec<String>,
    /// The checkup link ("Run a checkup", or "Review and close" inside the box).
    pub checkup: Rect,
    pub advice: Option<Rect>,
    pub advice_lines: Vec<String>,
    /// (label row, bar) of Commit free, and its hint lines.
    pub commit: (Rect, Rect),
    pub commit_hint: Rect,
    pub commit_hint_lines: Vec<String>,
    pub actions_band: Rect,
    pub buttons: [Rect; 3],
}

pub const CHECKUP_LABEL: &str = "Run a checkup";
pub const FIX_LABEL: &str = "Review and close";
pub const COMMIT_HINT: &str = "Room left for apps to ask for more memory: RAM plus the page file. When it runs out, apps crash even if RAM is free.";

const F_ROW: Font = Font::new(Face::Body, 12.0, REGULAR);
const F_ROW_STRONG: Font = Font::new(Face::Body, 12.0, SEMIBOLD);

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
    y += hh + c.s(14.0);

    let mh = c.line_height(F_METER).max(c.line_height(F_METER_VALUE));
    let ram_label = Rect::new(pad, y, inner, mh);
    y += mh + c.s(5.0);
    let ram_bar = Rect::new(pad, y, inner, c.s(10.0));
    y += c.s(10.0) + c.s(14.0);

    let ue_top = y;
    y += c.s(11.0);
    let th = c.line_height(F_UE);
    let ue_title = Rect::new(pad, y, inner, th);
    y += th;
    let ue_detail_lines: Vec<String> = d.ue_detail.iter().flat_map(|l| wrap_segments(c, l, F_HINT, inner)).collect();
    let ue_detail = if ue_detail_lines.is_empty() {
        None
    } else {
        let dh = c.line_height(F_HINT) * ue_detail_lines.len() as i32;
        let r = Rect::new(pad, y, inner, dh);
        y += dh;
        Some(r)
    };
    y += c.s(11.0);
    let ue_band = Rect::new(0, ue_top, w, y - ue_top);
    y += c.s(8.0);

    let rh = c.line_height(F_ROW).max(c.line_height(F_ROW_STRONG)).max(c.s(16.0));
    let value_w = c.s(64.0);
    let sw = c.s(8.0);
    let gap = c.s(8.0);
    let indent = c.s(16.0);
    let mut rows = Vec::new();
    let mut empty_cache = None;
    for r in &d.rows {
        y += c.s(if r.sub { 2.0 } else { 5.0 });
        let value = Rect::new(pad + inner - value_w, y, value_w, rh);
        let (swatch, x) = if r.sub {
            (Rect::new(pad + indent, y + (rh - sw) / 2, 0, 0), pad + indent)
        } else {
            (Rect::new(pad, y + (rh - sw) / 2, sw, sw), pad + sw + gap)
        };
        let name = if r.link { Rect::new(x, y, pad + inner - x, rh) } else { Rect::new(x, y, value.x - gap - x, rh) };
        if r.link {
            empty_cache = Some(Rect::new(x, y, (c.measure(&r.name, F_ROW).0 + c.s(2.0)).min(name.w), rh));
        }
        rows.push((swatch, name, value));
        y += rh;
    }

    // The fix box, or the plain checkup link.
    let lh = c.line_height(F_HINT);
    let (fix_box, fix_lines, checkup) = match &d.fix {
        Some(text) => {
            y += c.s(12.0);
            let bar = c.s(3.0);
            let (px, py) = (c.s(12.0), c.s(10.0));
            let tw = inner - bar - 2 * px;
            let lines = c.wrap_lines(text, F_ROW, tw, 4);
            let top = y;
            y += py + lines.len() as i32 * lh + c.s(4.0);
            let (lw, llh) = c.measure(FIX_LABEL, F_HINT);
            let link = Rect::new(pad + bar + px, y, lw + c.s(2.0), llh + c.s(2.0));
            y += llh + c.s(2.0) + py;
            (Some(Rect::new(pad, top, inner, y - top)), lines, link)
        }
        None => {
            y += c.s(10.0);
            let (lw, llh) = c.measure(CHECKUP_LABEL, F_HINT);
            let link = Rect::new(pad, y, lw + c.s(2.0), llh + c.s(2.0));
            y += llh + c.s(2.0);
            (None, Vec::new(), link)
        }
    };

    let advice_lines = match &d.advice {
        Some(a) if !a.trim().is_empty() => c.wrap_lines(a.trim(), F_HINT, inner, 2),
        _ => Vec::new(),
    };
    let advice = if advice_lines.is_empty() {
        None
    } else {
        y += c.s(10.0);
        let h = advice_lines.len() as i32 * lh;
        let r = Rect::new(pad, y, inner, h);
        y += h;
        Some(r)
    };

    // Commit free, under a hairline, with what it means.
    y += c.s(12.0) + c.s(10.0);
    let commit_label = Rect::new(pad, y, inner, mh);
    y += mh + c.s(4.0);
    let commit_bar = Rect::new(pad, y, inner, c.s(6.0));
    y += c.s(6.0) + c.s(5.0);
    let commit_hint_lines = c.wrap_lines(COMMIT_HINT, F_READ_SMALL, inner, 3);
    let sh = c.line_height(F_READ_SMALL);
    let commit_hint = Rect::new(pad, y, inner, sh * commit_hint_lines.len() as i32);
    y += commit_hint.h + c.s(14.0);

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
        ram_label,
        ram_bar,
        ue_band,
        ue_title,
        ue_detail,
        ue_detail_lines,
        rows,
        empty_cache,
        fix_box,
        fix_lines,
        checkup,
        advice,
        advice_lines,
        commit: (commit_label, commit_bar),
        commit_hint,
        commit_hint_lines,
        actions_band: Rect::new(0, band_top, w, height - band_top),
        buttons,
    }
}

/// Things that can be clicked or focused, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Checkup,
    EmptyCache,
    Button(usize),
}

/// The targets this layout has, in Tab order: the buttons, then the links top to bottom.
pub fn tab_order(l: &Layout) -> Vec<Target> {
    let mut t = vec![Target::Button(0), Target::Button(1), Target::Button(2)];
    if l.empty_cache.is_some() {
        t.push(Target::EmptyCache);
    }
    t.push(Target::Checkup);
    t
}

impl Target {
    pub fn command(self, d: &FlyoutData) -> Command {
        match self {
            Target::Checkup => Command::OpenDoctor,
            Target::EmptyCache => Command::FreeWindowsCache,
            Target::Button(i) => d.buttons()[i.min(2)].1,
        }
    }
    fn rect(self, l: &Layout) -> Rect {
        match self {
            Target::Checkup => l.checkup,
            Target::EmptyCache => l.empty_cache.unwrap_or_default(),
            Target::Button(i) => l.buttons[i.min(2)],
        }
    }
}

pub fn hit(l: &Layout, p: Pt) -> Option<Target> {
    tab_order(l).into_iter().find(|t| t.rect(l).contains(p))
}

fn paint_bar(c: &mut Canvas, r: Rect, frac: f32, color: Rgb) {
    let rad = r.h / 2;
    c.fill_round_rect(r, rad, RAISED);
    let fw = (r.w as f32 * frac.clamp(0.0, 1.0)).round() as i32;
    if fw > 0 {
        c.fill_round_rect(Rect::new(r.x, r.y, fw.max(r.h), r.h), rad, color);
    }
}

/// The stacked bar: each segment a share of the width, split by a one-pixel gap, the empty
/// track after them is free RAM. A critical bar ends in a red notch.
fn paint_stack(c: &mut Canvas, r: Rect, segs: &[Segment], critical: bool) {
    c.fill_round_rect(r, c.s(2.0), RAISED);
    let gap = c.px1(1.0);
    let mut x = r.x;
    let mut acc = 0.0f32;
    for s in segs {
        acc += s.frac;
        let end = r.x + (r.w as f32 * acc.min(1.0)).round() as i32;
        let w = (end - x).max(c.px1(2.0)).min(r.right() - x);
        if w > 0 {
            c.fill_rect(Rect::new(x, r.y, w, r.h), s.color);
        }
        x += w;
        if x < r.right() {
            c.fill_rect(Rect::new(x, r.y, gap, r.h), BG);
            x += gap;
        }
    }
    if critical {
        c.fill_rect(Rect::new(r.right() - c.s(3.0), r.y, c.s(3.0), r.h), ERROR);
    }
}

fn paint_lines(c: &mut Canvas, r: Rect, lines: &[String], f: Font, color: Rgb) {
    let lh = c.line_height(f);
    for (i, line) in lines.iter().enumerate() {
        c.text_top(Rect::new(r.x, r.y + i as i32 * lh, r.w, lh), line, f, color, Align::Left);
    }
}

/// Paint the flyout filling the canvas (sized `layout().width` x `layout().height`).
pub fn paint(c: &mut Canvas, d: &FlyoutData, mouse: Option<Pt>, focus: Option<Target>) {
    let l = layout(c, d);
    c.fill_rect(c.bounds(), BG);
    c.spaced_text(l.eyebrow, &d.eyebrow, F_EYEBROW, MUTED, c.s(3.0), Align::Left);
    c.text(l.headline, &c.ellipsize(&d.headline, F_STATE, l.headline.w), F_STATE, d.headline_color, Align::Left);

    // "RAM in use 92.6 of 96 GB" on the left, "3.4 GB free" on the right.
    const RAM_LABEL: &str = "RAM in use";
    c.text(l.ram_label, RAM_LABEL, F_METER, MUTED, Align::Left);
    let lw = c.measure(RAM_LABEL, F_METER).0 + c.s(6.0);
    let value = Rect::new(l.ram_label.x + lw, l.ram_label.y, l.ram_label.w - lw, l.ram_label.h);
    c.text(value, &d.ram_in_use, F_METER_VALUE, TEXT, Align::Left);
    c.text(l.ram_label, &d.ram_free, F_METER_VALUE, d.ram_free_color, Align::Right);
    paint_stack(c, l.ram_bar, &d.segments, d.ram_free_color == ERROR);

    c.fill_rect(l.ue_band, SURFACE);
    c.hline(0, l.ue_band.y, l.width, BORDER);
    c.hline(0, l.ue_band.bottom() - c.px1(1.0), l.width, BORDER);
    let sw = c.s(8.0);
    let running = !d.ue_detail.is_empty();
    let title = if running {
        c.fill_round_rect(Rect::new(l.ue_title.x, l.ue_title.cy() - sw / 2, sw, sw), c.s(2.0), SW_UNREAL);
        Rect::new(l.ue_title.x + sw + c.s(8.0), l.ue_title.y, l.ue_title.w - sw - c.s(8.0), l.ue_title.h)
    } else {
        l.ue_title
    };
    c.text(title, &d.ue_title, F_UE, TEXT, Align::Left);
    if let Some(r) = l.ue_detail {
        paint_lines(c, r, &l.ue_detail_lines, F_HINT, MUTED);
    }

    let hover = |t: Target| mouse.is_some_and(|m| hit(&l, m) == Some(t));
    for (row, (swatch, name, value)) in d.rows.iter().zip(l.rows.iter()) {
        if row.link {
            let col = if hover(Target::EmptyCache) { ACCENT_SOFT } else { ACCENT };
            c.text(*name, &c.ellipsize(&row.name, F_ROW, name.w), F_ROW, col, Align::Left);
            let (tw, th) = c.measure(&row.name, F_ROW);
            c.hline(name.x, name.y + (name.h + th) / 2 - c.s(2.0), tw.min(name.w), col);
            if focus == Some(Target::EmptyCache) {
                super::kit::focus_ring(c, l.empty_cache.unwrap_or(*name), c.s(3.0));
            }
        } else if row.sub {
            c.text(*name, &c.ellipsize(&row.name, F_ROW, name.w), F_ROW, TEXT_DIM, Align::Left);
            c.text(*value, &row.value, F_ROW, MUTED, Align::Right);
        } else {
            c.fill_round_rect(*swatch, c.s(2.0), row.swatch);
            c.text(*name, &c.ellipsize(&row.name, F_ROW, name.w), F_ROW, TEXT, Align::Left);
            c.text(*value, &row.value, F_ROW_STRONG, TEXT, Align::Right);
        }
    }

    if let Some(b) = l.fix_box {
        c.fill_round_rect(b, c.s(6.0), SURFACE);
        c.fill_rect(Rect::new(b.x, b.y, c.s(3.0), b.h), ERROR);
        let text = Rect::new(l.checkup.x, b.y + c.s(10.0), b.right() - c.s(12.0) - l.checkup.x, 0);
        paint_lines(c, Rect { h: c.line_height(F_ROW) * l.fix_lines.len() as i32, ..text }, &l.fix_lines, F_ROW, TEXT);
    }

    // A left-aligned link in the hint size (kit's Link button is centred and 13 px).
    let label = d.checkup_label();
    let col = match (d.fix.is_some(), hover(Target::Checkup)) {
        (true, false) => ACCENT,
        (true, true) => ACCENT_SOFT,
        (false, false) => MUTED,
        (false, true) => TEXT,
    };
    c.text(l.checkup, label, F_HINT, col, Align::Left);
    let (tw, th) = c.measure(label, F_HINT);
    c.hline(l.checkup.x, l.checkup.y + th - c.s(1.0), tw, col);
    if focus == Some(Target::Checkup) {
        super::kit::focus_ring(c, l.checkup, c.s(3.0));
    }

    if let Some(r) = l.advice {
        paint_lines(c, r, &l.advice_lines, F_HINT, MUTED);
    }

    let (label, bar) = l.commit;
    c.hline(0, label.y - c.s(10.0), l.width, HAIRLINE);
    c.text(label, &d.commit.label, F_METER, MUTED, Align::Left);
    c.text(label, &d.commit.value, F_METER_VALUE, TEXT, Align::Right);
    paint_bar(c, bar, d.commit.frac, d.commit.color);
    paint_lines(c, l.commit_hint, &l.commit_hint_lines, F_READ_SMALL, MUTED);

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

/// The four sample states (snapshot name, data). Normal and paused use the mockup's numbers;
/// critical is a 96 GB PC with 3.4 GB free where the editor has 47 GB committed.
pub fn samples() -> Vec<(&'static str, FlyoutData)> {
    use super::status_source::sample_status;
    use crate::engine::{AppRam, GroupRam, RamBreakdown};
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
    tight.ue.total_committed_gb = 62.3;
    tight.ram.in_use_gb = 116.4;
    tight.ram.unreal_gb = 49.8;
    tight.ram.unreal_editor_gb = 45.9;
    tight.ram.file_cache_gb = 35.3;
    tight.ram.unattributed_gb = 6.7;

    let mut critical = sample_status();
    critical.mode = "editor".into();
    critical.activities = crate::engine::Activities { editor: true, build: false, cook: false };
    critical.pressure = Level::Critical;
    critical.system.ram_total_gb = 96.0;
    critical.system.ram_free_gb = 3.4;
    critical.system.ram_free_pct = 3.5;
    critical.system.commit_limit_gb = 128.0;
    critical.system.commit_free_gb = 45.0;
    critical.system.commit_free_pct = 35.2;
    critical.ue.processes[0].committed_gb = 47.0;
    critical.ue.processes[0].uptime_min = 5;
    critical.ue.workers = 0;
    critical.ue.workers_committed_gb = 0.0;
    critical.ue.total_committed_gb = 47.0;
    let app = |name: &str, processes: usize, ram_gb: f64| AppRam { name: name.into(), processes, ram_gb };
    let group = |name: &str, ram_gb: f64| GroupRam { name: name.into(), ram_gb };
    critical.ram = RamBreakdown {
        in_use_gb: 92.6,
        unreal_gb: 40.2,
        unreal_editor_gb: 38.5,
        unreal_workers_gb: 0.0,
        groups: vec![group("Browsers", 0.1), group("Chat", 0.1), group("Agents", 0.8), group("Background", 0.0)],
        other_apps_gb: 6.1,
        other_apps_count: 64,
        top_apps: vec![app("rider64", 1, 2.9), app("vmmem", 1, 1.6), app("devenv", 2, 0.8), app("Discord", 6, 0.2)],
        file_cache_gb: 31.5,
        windows_gb: 7.4,
        compressed_gb: 2.1,
        unattributed_gb: 4.3,
    };
    critical.groups.iter_mut().for_each(|g| g.ram_mb /= 40);
    critical.advice = vec!["Memory is critical: 3.4 GB RAM free, 45.0 GB commit free. Don't start a build or cook until this clears.".into()];

    let mut paused = sample_status();
    paused.paused = true;

    vec![
        ("normal", FlyoutData::new(&normal, &pressure, Some(2.1), None)),
        ("tight", FlyoutData::new(&tight, &pressure, Some(6.8), None)),
        ("critical", FlyoutData::new(&critical, &pressure, None, None)),
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

    fn gb(v: &str) -> f64 {
        if let Some(mb) = v.strip_suffix(" MB") {
            mb.parse::<f64>().unwrap() / 1024.0
        } else {
            v.strip_suffix(" GB").unwrap().parse().unwrap()
        }
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
        assert_eq!(app_words("vmmemWSL"), "Linux VMs (WSL)");
        assert_eq!(app_words("Notion"), "Notion");
        assert_eq!(and_list(&["A".into(), "B".into(), "C".into()]), "A, B and C");
        assert_eq!(and_list(&["A".into()]), "A");
    }

    #[test]
    fn data_from_status() {
        let d = data();
        assert_eq!(d.eyebrow, "EDITOR + BUILD");
        assert_eq!(d.headline, "MEMORY IS FINE");
        assert_eq!(d.ram_in_use, "82.6 of 128 GB");
        assert_eq!((d.ram_free.as_str(), d.ram_free_color), ("45.1 GB free", ACCENT));
        assert_eq!(d.commit.value, "27.6 of 144 GB");
        assert_eq!(d.ue_title, "Unreal · 34.0 GB");
        assert_eq!(d.ue_detail, vec!["Editor 30.1 GB · 3 compile workers 2.4 GB · other 1.5 GB", "Up 30 min · growing 2.1 GB an hour · Windows promised it 47.9 GB"]);
        // Ranked, largest first; groups of 1 GB and up get a row, the rest share one.
        let names: Vec<&str> = d.rows.iter().filter(|r| !r.sub).map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Windows file cache", "Other apps", "Agents", "Windows and drivers", "Browsers", "Shared or not itemized", "Squeezed by Windows", "Chat, Background"]);
        // Other apps is over 1 GB, so its biggest three are named, then the rest.
        let other = d.rows.iter().position(|r| r.name == "Other apps").unwrap();
        let subs: Vec<(&str, &str)> = d.rows[other + 1..other + 5].iter().map(|r| (r.name.as_str(), r.value.as_str())).collect();
        assert_eq!(subs, [("Rider", "2.8 GB"), ("Linux VMs (WSL)", "1.9 GB"), ("Visual Studio", "717 MB"), ("58 smaller apps", "819 MB")]);
        assert!(d.rows[other + 1..other + 5].iter().all(|r| r.sub));
        // The parts add up to the RAM in use, and the bar to its share of total RAM.
        let sum: f64 = d.rows.iter().filter(|r| !r.sub).map(|r| gb(&r.value)).sum::<f64>() + 34.0;
        assert!((sum - 82.6).abs() < 0.15, "{}", sum);
        let bar: f32 = d.segments.iter().map(|s| s.frac).sum();
        assert!((bar - 82.6 / 127.7).abs() < 0.01, "{}", bar);
        assert_eq!(d.segments[0].color, SW_UNREAL);
        assert!(d.fix.is_none() && d.advice.is_none());
        // The file cache row is followed by its "Empty it now" link.
        let cache = d.rows.iter().position(|r| r.name == "Windows file cache").unwrap();
        assert!(d.rows[cache + 1].link && d.rows[cache + 1].sub && d.rows[cache + 1].value.is_empty());
        assert_eq!(d.rows.iter().filter(|r| r.link).count(), 1);
        assert_eq!(d.checkup_label(), CHECKUP_LABEL);
        assert_eq!(d.buttons()[1], ("Pause 30 min", Command::Pause30));

        let (_, crit) = samples().remove(2);
        assert_eq!((crit.headline.as_str(), crit.headline_color), ("MEMORY IS CRITICAL", ERROR));
        assert_eq!((crit.ram_in_use.as_str(), crit.ram_free.as_str(), crit.ram_free_color), ("92.6 of 96 GB", "3.4 GB free", ERROR));
        assert_eq!(crit.ue_detail, vec!["Editor 38.5 GB · other 1.7 GB", "Up 5 min · Windows promised it 47.0 GB"]);
        assert_eq!(
            crit.fix.as_deref(),
            Some("Emptying Windows' file cache gets about 31.5 GB back, closing Rider, Linux VMs (WSL) and Visual Studio about 5.3 GB. Don't start a build or cook yet.")
        );
        assert_eq!(crit.checkup_label(), FIX_LABEL);
        assert!(crit.advice.is_none(), "the box replaces the engine's critical line");
        assert_eq!(crit.rows[0].name, "Windows file cache");

        let (_, p) = samples().remove(3);
        assert_eq!(p.headline, "PAUSED");
        assert_eq!(p.buttons()[1], ("Resume", Command::Resume));
        assert_eq!(p.advice.as_deref(), Some("Other apps are back to normal until 15:42."));
        assert!(p.fix.is_none());

        let mut idle = sample_status();
        idle.mode = "idle".into();
        idle.activities = Default::default();
        idle.ue = Default::default();
        idle.ram.unreal_gb = 0.0;
        idle.ram.other_apps_gb = 0.4;
        let d = FlyoutData::new(&idle, &crate::config::Config::builtin().pressure, None, None);
        assert_eq!((d.eyebrow.as_str(), d.ue_title.as_str()), ("IDLE", "Unreal isn't running"));
        assert!(d.ue_detail.is_empty());
        assert!(d.rows.iter().all(|r| !r.sub || r.link), "small other apps aren't itemized when memory is fine");
    }

    #[test]
    fn critical_without_big_apps_still_says_wait() {
        let mut st = sample_status();
        st.pressure = Level::Critical;
        st.ram.top_apps.iter_mut().for_each(|a| a.ram_gb = 0.2);
        let pressure = crate::config::Config::builtin().pressure;
        let d = FlyoutData::new(&st, &pressure, None, None);
        assert_eq!(d.fix.as_deref(), Some("Emptying Windows' file cache gets about 22.4 GB back. Don't start a build or cook yet."));
        // A small cache is neither in the box nor offered.
        st.ram.file_cache_gb = 0.8;
        let d = FlyoutData::new(&st, &pressure, None, None);
        assert_eq!(d.fix.as_deref(), Some("Don't start a build or cook until this clears."));
        assert!(!d.rows.iter().any(|r| r.link));
        let l = layout(&Canvas::new(1, 1, Scale(1.0)), &d);
        assert!(l.empty_cache.is_none() && !tab_order(&l).contains(&Target::EmptyCache));
    }

    #[test]
    fn layout_fits_at_two_scales() {
        for (name, d) in samples() {
            for scale in [Scale(1.0), Scale(1.5)] {
                let c = Canvas::new(1, 1, scale);
                let l = layout(&c, &d);
                let bounds = Rect::new(0, 0, l.width, l.height);
                assert_eq!(l.width, scale.s(WIDTH));
                let mut rects = vec![l.eyebrow, l.headline, l.ram_label, l.ram_bar, l.ue_band, l.ue_title, l.checkup, l.commit.0, l.commit.1, l.commit_hint, l.actions_band];
                rects.extend(l.rows.iter().flat_map(|(_, n, v)| [*n, *v]));
                rects.extend(l.buttons);
                rects.extend(l.advice);
                rects.extend(l.ue_detail);
                rects.extend(l.fix_box);
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
                assert!(l.headline.bottom() <= l.ram_label.y && l.ram_label.bottom() <= l.ram_bar.y && l.ram_bar.bottom() <= l.ue_band.y, "{} order", name);
                let mut y = l.ue_band.bottom();
                for (_, n, _) in &l.rows {
                    assert!(n.y >= y, "{} rows overlap", name);
                    y = n.bottom();
                }
                match l.fix_box {
                    Some(b) => {
                        assert!(y <= b.y && b.encloses(&l.checkup), "{} fix box", name);
                        y = b.bottom();
                    }
                    None => {
                        assert!(y <= l.checkup.y);
                        y = l.checkup.bottom();
                    }
                }
                assert!(l.advice.map_or(y, |a| a.bottom()) <= l.commit.0.y);
                assert!(l.commit_hint.bottom() <= l.actions_band.y);
                assert!(c.measure(&d.headline, F_STATE).0 <= l.headline.w, "{} headline clips", name);
                for line in l.ue_detail_lines.iter().chain(&l.advice_lines) {
                    assert!(c.measure(line, F_HINT).0 <= l.inner_width() && !line.ends_with('…'), "{} line clips: {}", name, line);
                }
                for line in &l.commit_hint_lines {
                    assert!(c.measure(line, F_READ_SMALL).0 <= l.inner_width() && !line.ends_with('…'), "{} hint clips: {}", name, line);
                }
                assert!(!l.fix_lines.iter().any(|x| x.ends_with('…')), "{} fix clips", name);
                // Tab order reaches every target.
                assert_eq!(l.empty_cache.is_some(), d.rows.iter().any(|r| r.link), "{}", name);
                for t in tab_order(&l) {
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
        assert_eq!(c.pixel(l.ram_bar.x + 2, l.ram_bar.cy()), SW_UNREAL);
        // The link is a target that empties the cache.
        let r = l.empty_cache.unwrap();
        assert_eq!(hit(&l, Pt::new(r.cx(), r.cy())), Some(Target::EmptyCache));
        assert_eq!(Target::EmptyCache.command(&data()), Command::FreeWindowsCache);
    }
}

impl Layout {
    /// Content width inside the padding.
    pub fn inner_width(&self) -> i32 {
        self.eyebrow.w
    }
}

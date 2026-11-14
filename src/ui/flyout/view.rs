//! Where everything in the flyout goes and how it is painted, into a `Canvas` (the window
//! and the snapshots share it), and what a point or Tab lands on.

use super::super::canvas::{Align, Canvas};
use super::super::commands::Command;
use super::super::geom::{Pt, Rect};
use super::super::kit::{Button, ButtonStyle, WState};
use super::super::theme::*;
use super::model::*;

/// Width at 96 DPI.
pub const WIDTH: f32 = 340.0;
const PAD: f32 = 18.0;

const F_STATE: Font = Font::new(Face::Display, 30.0, REGULAR);
const F_METER: Font = Font::new(Face::Body, 12.0, REGULAR);
const F_METER_VALUE: Font = Font::new(Face::Body, 12.0, SEMIBOLD);
const F_UE: Font = Font::new(Face::Body, 14.0, SEMIBOLD);


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
    /// (swatch, name, value) per row; indented rows have an empty swatch.
    pub rows: Vec<(Rect, Rect, Rect)>,
    /// What a click on row `i` hits: the full width (split halfway between rows), a link's
    /// text, or nothing.
    pub row_hits: Vec<Option<Rect>>,
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
    /// The disk line under the commit hint, when the tray has polled the drives.
    pub disk: Option<Rect>,
    pub actions_band: Rect,
    pub buttons: [Rect; 3],
}


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
    // Each row's top edge with half the space above it, so the click areas meet halfway.
    let mut tops = Vec::new();
    for r in &d.rows {
        let space = c.s(if r.kind.is_main() { 5.0 } else { 2.0 });
        tops.push(y + space / 2);
        y += space;
        let value = Rect::new(pad + inner - value_w, y, value_w, rh);
        let (swatch, x) = if r.kind.is_main() {
            (Rect::new(pad, y + (rh - sw) / 2, sw, sw), pad + sw + gap)
        } else {
            (Rect::new(pad + indent, y + (rh - sw) / 2, 0, 0), pad + indent)
        };
        let name_w = if matches!(r.kind, RowKind::Link(_)) { pad + inner - x } else { value.x - gap - x };
        rows.push((swatch, Rect::new(x, y, name_w, rh), value));
        y += rh;
    }
    let row_hits = d
        .rows
        .iter()
        .zip(&rows)
        .enumerate()
        .map(|(i, (r, (_, name, _)))| match r.kind {
            RowKind::Rest => None,
            RowKind::Link(_) => Some(Rect::new(name.x, name.y, (c.measure(&r.name, F_ROW).0 + c.s(2.0)).min(name.w), name.h)),
            _ => {
                let bottom = tops.get(i + 1).copied().unwrap_or(name.bottom() + c.s(2.5));
                Some(Rect::new(c.s(6.0), tops[i], w - c.s(12.0), bottom - tops[i]))
            }
        })
        .collect();

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
        Some(a) if !a.trim().is_empty() => c.wrap_lines(a.trim(), F_HINT, inner, 4),
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
    y += commit_hint.h;
    let disk = d.disk.as_ref().map(|_| {
        y += c.s(8.0);
        let r = Rect::new(pad, y, inner, c.line_height(F_HINT));
        y += r.h;
        r
    });
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
        ram_label,
        ram_bar,
        ue_band,
        ue_title,
        ue_detail,
        ue_detail_lines,
        rows,
        row_hits,
        fix_box,
        fix_lines,
        checkup,
        advice,
        advice_lines,
        commit: (commit_label, commit_bar),
        commit_hint,
        commit_hint_lines,
        disk,
        actions_band: Rect::new(0, band_top, w, height - band_top),
        buttons,
    }
}

/// Things that can be clicked or focused, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Checkup,
    Button(usize),
    /// The Unreal band: opens its menu.
    Unreal,
    /// RAM row `i`: opens its menu, or runs its link.
    Row(usize),
}

/// The targets this layout has, in Tab order: the buttons, then the links top to bottom.
pub fn tab_order(l: &Layout) -> Vec<Target> {
    let mut t = vec![Target::Button(0), Target::Button(1), Target::Button(2), Target::Unreal];
    t.extend(l.row_hits.iter().enumerate().filter(|(_, r)| r.is_some()).map(|(i, _)| Target::Row(i)));
    t.push(Target::Checkup);
    t
}

impl Target {
    /// What clicking it runs; None for the band and rows with a menu, which open it instead.
    pub fn command(self, d: &FlyoutData) -> Option<Command> {
        match self {
            Target::Checkup => Some(Command::OpenDoctor),
            Target::Button(i) => Some(d.buttons()[i.min(2)].1),
            Target::Row(i) => match d.rows.get(i).map(|r| &r.kind) {
                Some(RowKind::Link(c)) => Some(*c),
                _ => None,
            },
            Target::Unreal => None,
        }
    }

    /// The menu it opens: the Unreal band's, or row `i`'s.
    pub fn menu(self, d: &FlyoutData) -> Vec<MenuItem> {
        match self {
            Target::Unreal => d.unreal_menu(),
            Target::Row(i) => d.rows.get(i).map(|r| d.row_menu(&r.kind)).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
    pub fn rect(self, l: &Layout) -> Rect {
        match self {
            Target::Checkup => l.checkup,
            Target::Button(i) => l.buttons[i.min(2)],
            Target::Unreal => l.ue_band,
            Target::Row(i) => l.row_hits.get(i).copied().flatten().unwrap_or_default(),
        }
    }
}

/// What is under `p` (the targets don't overlap).
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

    let hover = |t: Target| mouse.is_some_and(|m| hit(&l, m) == Some(t));
    c.fill_rect(l.ue_band, if hover(Target::Unreal) { RAISED } else { SURFACE });
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

    if focus == Some(Target::Unreal) {
        super::super::kit::focus_ring(c, Rect::new(l.ue_band.x + c.s(3.0), l.ue_band.y + c.s(3.0), l.ue_band.w - c.s(6.0), l.ue_band.h - c.s(6.0)), c.s(4.0));
    }
    for (i, (row, (swatch, name, value))) in d.rows.iter().zip(l.rows.iter()).enumerate() {
        let hovered = hover(Target::Row(i));
        let area = l.row_hits[i];
        if let (true, Some(a), false) = (hovered, area, matches!(row.kind, RowKind::Link(_))) {
            c.fill_round_rect(a, c.s(4.0), SELECTED);
        }
        if let (true, Some(a)) = (focus == Some(Target::Row(i)), area) {
            super::super::kit::focus_ring(c, a, c.s(4.0));
        }
        let text = c.ellipsize(&row.name, F_ROW, name.w);
        match &row.kind {
            RowKind::Link(_) => {
                let col = if hovered { ACCENT_SOFT } else { ACCENT };
                c.text(*name, &text, F_ROW, col, Align::Left);
                let (tw, th) = c.measure(&row.name, F_ROW);
                c.hline(name.x, name.y + (name.h + th) / 2 - c.s(2.0), tw.min(name.w), col);
            }
            k if k.is_main() => {
                c.fill_round_rect(*swatch, c.s(2.0), row.swatch);
                c.text(*name, &text, F_ROW, TEXT, Align::Left);
                c.text(*value, &row.value, F_ROW_STRONG, TEXT, Align::Right);
            }
            _ => {
                c.text(*name, &text, F_ROW, TEXT_DIM, Align::Left);
                c.text(*value, &row.value, F_ROW, MUTED, Align::Right);
            }
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
        super::super::kit::focus_ring(c, l.checkup, c.s(3.0));
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
    if let (Some(r), Some(text)) = (l.disk, &d.disk) {
        paint_lines(c, r, std::slice::from_ref(text), F_HINT, d.disk_color);
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

impl Layout {
    /// Content width inside the padding.
    pub fn inner_width(&self) -> i32 {
        self.eyebrow.w
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breakdown::RamPart;
    use crate::engine::Level;
    use crate::ui::commands::UiAction;
    use crate::ui::status_source::sample_status;

    fn data() -> FlyoutData {
        samples().remove(0).1
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
        assert!(!d.rows.iter().any(|r| matches!(r.kind, RowKind::Link(_))));
        let l = layout(&Canvas::new(1, 1, Scale(1.0)), &d);
        assert!(tab_order(&l).iter().all(|t| t.command(&d) != Some(Command::FreeWindowsCache)));
    }

    #[test]
    fn rows_open_menus_that_act_on_them() {
        let d = data();
        let actions = |items: Vec<MenuItem>| -> Vec<UiAction> {
            items.into_iter().filter_map(|i| if let MenuItem::Action(_, a) = i { Some(a) } else { None }).collect()
        };
        let row = |name: &str| d.rows.iter().find(|r| r.name == name).unwrap().kind.clone();

        assert!(actions(d.unreal_menu()).contains(&UiAction::Run(Command::EndUnrealLeftovers)));
        assert_eq!(
            actions(d.row_menu(&row("Agents"))),
            [UiAction::FreeGroup("Agents".into()), UiAction::Run(Command::OpenSettings)]
        );
        // Small groups share a row, and its menu frees each.
        let small = actions(d.row_menu(&row("Chat, Background")));
        assert_eq!(&small[..2], [UiAction::FreeGroup("Chat".into()), UiAction::FreeGroup("Background".into())]);
        // Other apps: close each of the biggest, then the checkup.
        let other = actions(d.row_menu(&RowKind::Part(RamPart::OtherApps)));
        assert_eq!(other[0], UiAction::CloseApp("rider64".into()));
        assert_eq!(other.last(), Some(&UiAction::Run(Command::OpenDoctor)));
        assert!(d.row_menu(&RowKind::Part(RamPart::OtherApps)).iter().any(|i| matches!(i, MenuItem::Action(l, _) if l == "Close Rider (2.8 GB)")));
        // A named app: close it, or hold it back with any group.
        let rider = actions(d.row_menu(&row("Rider")));
        assert_eq!(rider[0], UiAction::CloseApp("rider64".into()));
        assert!(rider.contains(&UiAction::AddToGroup { app: "rider64".into(), group: "Browsers".into() }));
        assert_eq!(rider.len(), 1 + d.group_names.len());
        assert_eq!(actions(d.row_menu(&row("Windows file cache"))), [UiAction::Run(Command::FreeWindowsCache)]);
        assert_eq!(actions(d.row_menu(&row("Shared or not itemized"))), [UiAction::Run(Command::FreeWindowsCache)]);
        assert_eq!(actions(d.row_menu(&row("Windows and drivers"))), [UiAction::Run(Command::ShowDriverReport)]);
        assert!(matches!(d.row_menu(&row("Squeezed by Windows"))[0], MenuItem::Info(_)));
        assert!(d.row_menu(&row("58 smaller apps")).is_empty());

        // Every row with a menu is a target, and a click on it is the row.
        let l = layout(&Canvas::new(1, 1, Scale(1.0)), &d);
        for (i, r) in d.rows.iter().enumerate() {
            assert_eq!(l.row_hits[i].is_some(), r.kind != RowKind::Rest, "{}", r.name);
            if let Some(h) = l.row_hits[i] {
                assert_eq!(hit(&l, Pt::new(h.cx(), h.cy())), Some(Target::Row(i)), "{}", r.name);
                assert!(tab_order(&l).contains(&Target::Row(i)));
                let link = matches!(r.kind, RowKind::Link(_));
                assert_eq!(Target::Row(i).command(&d).is_some(), link, "{}", r.name);
                assert_eq!(Target::Row(i).menu(&d), d.row_menu(&r.kind));
            }
        }
        assert_eq!(hit(&l, Pt::new(l.ue_title.cx(), l.ue_title.cy())), Some(Target::Unreal));
        // Click areas don't overlap: each row's bottom is the next row's top.
        let areas: Vec<Rect> = l.row_hits.iter().flatten().copied().collect();
        for w in areas.windows(2) {
            assert!(w[0].bottom() <= w[1].y, "{:?} overlaps {:?}", w[0], w[1]);
        }
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
                rects.extend(l.disk);
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
    fn renders_with_the_theme() {
        let c = render(&data(), Scale(1.0));
        let l = layout(&c, &data());
        assert_eq!(c.pixel(l.width / 2, l.eyebrow.y - 4), BG);
        assert_eq!(c.pixel(4, l.ue_band.cy()), SURFACE);
        assert_eq!(c.pixel(l.buttons[0].x + 3, l.buttons[0].cy()), ACCENT);
        assert_eq!(c.pixel(l.ram_bar.x + 2, l.ram_bar.cy()), SW_UNREAL);
        // The link is a target that empties the cache.
        let link = data().rows.iter().position(|r| matches!(r.kind, RowKind::Link(_))).unwrap();
        let r = l.row_hits[link].unwrap();
        assert_eq!(hit(&l, Pt::new(r.cx(), r.cy())), Some(Target::Row(link)));
        assert_eq!(Target::Row(link).command(&data()), Some(Command::FreeWindowsCache));
    }
}

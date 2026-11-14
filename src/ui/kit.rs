//! The widget kit: plain structs that paint themselves into a `Canvas` inside a rectangle
//! the surface chose, and answer hit tests. Surfaces keep their own retained list of
//! (rect, widget, action) and pass a `WState` (hover / pressed / focused) when painting.
//! Sizes are the mockup's 96-DPI values, scaled with `Canvas::s`.

pub use super::canvas::{Align, Canvas};
use super::geom::{Pt, Rect};
use super::theme::*;

/// Standard control height (pills, number fields, buttons) at 96 DPI.
pub const CTRL_H: f32 = 34.0;

/// Interaction state of one widget while painting.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WState {
    pub hover: bool,
    pub pressed: bool,
    pub focused: bool,
    pub disabled: bool,
    /// The mouse position when it is over the widget (chips show their x on hover).
    pub mouse: Option<Pt>,
}

/// The 1 px lime focus ring, drawn just outside `r`.
pub fn focus_ring(c: &mut Canvas, r: Rect, radius: i32) {
    let g = c.s(3.0);
    let w = c.px1(1.0);
    c.stroke_round_rect(r.inset(-g, -g), radius + g, ACCENT, w);
}

/// The lime "changed by you" dot, 6 px, centred vertically on `cy`, starting at `x`. Returns its width.
pub fn layer_dot(c: &mut Canvas, x: i32, cy: i32) -> i32 {
    let d = c.s(6.0);
    c.fill_ellipse(Rect::new(x, cy - d / 2, d, d), ACCENT);
    d
}

/// The outlined "TEAM" tag starting at `x`, centred on `cy`. Returns its width.
pub fn team_tag(c: &mut Canvas, x: i32, cy: i32) -> i32 {
    let extra = c.s(1.6);
    let (tw, _) = c.measure_spaced("TEAM", F_TAG, extra);
    let w = tw + c.s(12.0);
    let h = c.s(16.0);
    let r = Rect::new(x, cy - h / 2, w, h);
    c.stroke_round_rect(r, c.s(3.0), BORDER, c.px1(1.0));
    c.spaced_text(r.offset(c.s(1.0), 0), "TEAM", F_TAG, MUTED, extra, Align::Center);
    w
}

// ---------------------------------------------------------------- Toggle

#[derive(Debug, Clone, PartialEq)]
pub struct Toggle {
    pub on: bool,
}

impl Toggle {
    pub fn size(c: &Canvas) -> (i32, i32) {
        (c.s(44.0), c.s(24.0))
    }
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let rad = r.h / 2;
        if self.on {
            c.fill_round_rect(r, rad, if st.hover { ACCENT_SOFT } else { ACCENT });
        } else {
            c.fill_round_rect_bordered(r, rad, if st.hover { SELECTED } else { RAISED }, BORDER);
        }
        let k = c.s(16.0);
        let inset = c.s(4.0);
        let kx = if self.on { r.right() - inset - k } else { r.x + inset };
        c.fill_ellipse(Rect::new(kx, r.cy() - k / 2, k, k), if self.on { ON_ACCENT } else { MUTED });
        if st.focused {
            focus_ring(c, r, rad);
        }
    }
}

// ---------------------------------------------------------------- Segmented

#[derive(Debug, Clone, PartialEq)]
pub struct Segmented {
    pub options: Vec<String>,
    pub selected: usize,
}

impl Segmented {
    fn segments(&self, c: &Canvas, r: Rect) -> Vec<Rect> {
        let pad = c.s(3.0);
        let inner = r.inset(pad, pad);
        let n = self.options.len().max(1) as i32;
        (0..n)
            .map(|i| {
                let x0 = inner.x + inner.w * i / n;
                let x1 = inner.x + inner.w * (i + 1) / n;
                Rect::new(x0, inner.y, x1 - x0, inner.h)
            })
            .collect()
    }
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        c.fill_round_rect_bordered(r, c.s(RADIUS), RAISED, BORDER);
        let segs = self.segments(c, r);
        for (i, (seg, label)) in segs.iter().zip(&self.options).enumerate() {
            let sel = i == self.selected;
            let hovered = !sel && st.mouse.map(|m| seg.contains(m)).unwrap_or(false);
            if sel {
                c.fill_round_rect(*seg, c.s(4.0), ACCENT);
            } else if hovered {
                c.fill_round_rect(*seg, c.s(4.0), SELECTED);
            }
            c.text(*seg, label, F_BUTTON, if sel { ON_ACCENT } else if hovered { TEXT } else { MUTED }, Align::Center);
        }
        if st.focused {
            focus_ring(c, r, c.s(RADIUS));
        }
    }
    /// The option under `p`.
    pub fn hit(&self, c: &Canvas, r: Rect, p: Pt) -> Option<usize> {
        self.segments(c, r).iter().position(|s| s.contains(p))
    }
}

// ---------------------------------------------------------------- Pill (opens a popup menu)

#[derive(Debug, Clone, PartialEq)]
pub struct Pill {
    pub text: String,
    /// Draw the ▾ chevron (choices). Hotkey fields have none.
    pub chevron: bool,
    pub muted: bool,
    pub accent_border: bool,
}

impl Pill {
    pub fn new(text: &str) -> Pill {
        Pill { text: text.to_string(), chevron: true, muted: false, accent_border: false }
    }
    /// Width that fits the text, at least `min` (96-DPI px).
    pub fn width(&self, c: &Canvas, min: f32) -> i32 {
        let tw = c.measure(&self.text, F_VALUE).0;
        let w = tw + c.s(24.0) + if self.chevron { c.s(22.0) } else { 0 };
        w.max(c.s(min))
    }
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let fill = if st.hover || st.pressed { SELECTED } else { RAISED };
        c.fill_round_rect_bordered(r, c.s(RADIUS), fill, if self.accent_border { ACCENT } else { BORDER });
        let pad = c.s(12.0);
        let chev_w = if self.chevron { c.s(18.0) } else { 0 };
        let tr = Rect::new(r.x + pad, r.y, r.w - 2 * pad - chev_w, r.h);
        let align = if self.chevron { Align::Left } else { Align::Center };
        c.text(tr, &self.text, F_VALUE, if self.muted { MUTED } else if self.accent_border { ACCENT_SOFT } else { TEXT }, align);
        if self.chevron {
            let ar = Rect::new(r.right() - pad - chev_w, r.y, chev_w, r.h);
            c.icon(ar, glyph::CHEVRON_DOWN, Font::new(Face::Icon, 10.0, REGULAR), MUTED);
        }
        if st.focused {
            focus_ring(c, r, c.s(RADIUS));
        }
    }
}

// ---------------------------------------------------------------- NumberField

#[derive(Debug, Clone, PartialEq)]
pub struct NumberField {
    pub text: String,
    /// "Off" and other placeholders are muted.
    pub muted: bool,
    /// While the temporary EDIT is over it: lime border, no text.
    pub editing: bool,
}

impl NumberField {
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let border = if self.editing { ACCENT } else { BORDER };
        c.fill_round_rect_bordered(r, c.s(RADIUS), if st.hover && !self.editing { SELECTED } else { RAISED }, border);
        if !self.editing {
            let tr = r.inset(c.s(12.0), 0);
            c.text(tr, &self.text, F_VALUE, if self.muted { MUTED } else { TEXT }, Align::Right);
        }
        if st.focused {
            focus_ring(c, r, c.s(RADIUS));
        }
    }
    /// Where the temporary EDIT goes while editing.
    pub fn edit_rect(c: &Canvas, r: Rect) -> Rect {
        let h = c.s(20.0);
        Rect::new(r.x + c.s(10.0), r.cy() - h / 2, r.w - c.s(20.0), h)
    }
}

// ---------------------------------------------------------------- Slider

/// Soft-cap slider: 0 to `max` with a lime fill and a white knob.
#[derive(Debug, Clone, PartialEq)]
pub struct Slider {
    /// Knob position 0..1.
    pub frac: f32,
}

impl Slider {
    fn track(c: &Canvas, r: Rect) -> (i32, i32) {
        let k = c.s(8.0);
        (r.x + k, r.right() - k)
    }
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let (x0, x1) = Self::track(c, r);
        let th = c.s(4.0);
        let ty = r.cy() - th / 2;
        c.fill_round_rect(Rect::new(r.x, ty, r.w, th), th / 2, RAISED);
        let kx = x0 + ((x1 - x0) as f32 * self.frac.clamp(0.0, 1.0)).round() as i32;
        if kx > r.x {
            c.fill_round_rect(Rect::new(r.x, ty, kx - r.x, th), th / 2, ACCENT);
        }
        let k = c.s(16.0);
        let knob = Rect::new(kx - k / 2, r.cy() - k / 2, k, k);
        c.fill_ellipse(knob, if st.pressed || st.hover { ACCENT_SOFT } else { TEXT });
        if st.focused {
            focus_ring(c, knob, k / 2);
        }
    }
    /// The knob position for the mouse at `x`.
    pub fn frac_at(c: &Canvas, r: Rect, x: i32) -> f32 {
        let (x0, x1) = Self::track(c, r);
        if x1 <= x0 {
            return 0.0;
        }
        ((x - x0) as f32 / (x1 - x0) as f32).clamp(0.0, 1.0)
    }
}

/// Soft caps go from 0 ("Off") to this share of RAM.
pub const SLIDER_MAX_PCT: f64 = 25.0;

/// Percent of RAM for a knob position, snapped to 0.1 %.
pub fn pct_from_frac(frac: f32) -> f64 {
    snap_pct(frac.clamp(0.0, 1.0) as f64 * SLIDER_MAX_PCT)
}

/// Knob position for a percentage (clamped to the slider's range).
pub fn frac_from_pct(pct: f64) -> f32 {
    (pct / SLIDER_MAX_PCT).clamp(0.0, 1.0) as f32
}

/// Round to the slider's 0.1 % step, within 0..=25 %.
pub fn snap_pct(pct: f64) -> f64 {
    ((pct * 10.0).round() / 10.0).clamp(0.0, SLIDER_MAX_PCT)
}

/// GB that `pct` percent of `total_gb` means.
pub fn pct_to_gb(pct: f64, total_gb: f64) -> f64 {
    pct / 100.0 * total_gb
}

/// Percent of `total_gb` that `gb` is.
pub fn gb_to_pct(gb: f64, total_gb: f64) -> f64 {
    if total_gb <= 0.0 {
        0.0
    } else {
        gb / total_gb * 100.0
    }
}

/// "6.1 GB", or "256 MB" under 1 GB.
pub fn format_gb(gb: f64) -> String {
    if gb < 0.995 {
        format!("{} MB", (gb * 1024.0).round() as i64)
    } else {
        format!("{:.1} GB", gb)
    }
}

/// "4.8%" with one decimal.
pub fn format_pct(pct: f64) -> String {
    format!("{:.1}%", pct)
}

// ---------------------------------------------------------------- Chips

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Item(usize),
    /// "+N": the rest of a long list; clicking shows all.
    More(usize),
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipHit {
    Item(usize),
    Remove(usize),
    More,
    Add,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chips {
    pub items: Vec<String>,
    /// "+ Add app…" / "+ Add…".
    pub add_label: String,
    /// When not expanded, at most this many lines; the rest folds into "+N".
    pub max_lines: usize,
    pub expanded: bool,
}

impl Chips {
    fn chip_w(c: &Canvas, text: &str) -> i32 {
        c.measure(text, F_CHIP).0 + c.s(20.0) + 2 * c.px1(1.0)
    }
    pub fn chip_h(c: &Canvas) -> i32 {
        c.s(24.0)
    }

    /// Chip rectangles for a row `width` wide, right-aligned lines, starting at (0, 0).
    pub fn layout(&self, c: &Canvas, width: i32) -> Vec<(Rect, ChipKind)> {
        let gap = c.s(6.0);
        let h = Self::chip_h(c);
        let add = (Self::chip_w(c, &self.add_label), ChipKind::Add);
        let mut widths: Vec<(i32, ChipKind)> = self.items.iter().enumerate().map(|(i, s)| (Self::chip_w(c, s), ChipKind::Item(i))).collect();
        // Fold into "+N" when the list would need more lines than allowed.
        let lines_needed = |ws: &[(i32, ChipKind)]| {
            let (mut lines, mut x) = (1usize, 0);
            for (w, _) in ws {
                if x > 0 && x + gap + w > width {
                    lines += 1;
                    x = *w;
                } else {
                    x += if x > 0 { gap } else { 0 } + w;
                }
            }
            lines
        };
        let mut all = widths.clone();
        all.push(add);
        if !self.expanded && lines_needed(&all) > self.max_lines {
            let mut keep = widths.len();
            loop {
                let rest = self.items.len() - keep;
                let mut trial: Vec<(i32, ChipKind)> = widths[..keep].to_vec();
                trial.push((Self::chip_w(c, &format!("+{}", rest)), ChipKind::More(rest)));
                trial.push(add);
                if keep == 0 || lines_needed(&trial) <= self.max_lines {
                    widths = trial;
                    break;
                }
                keep -= 1;
            }
        } else {
            widths = all;
        }
        // Break into lines, then right-align each.
        let mut lines: Vec<Vec<(i32, ChipKind)>> = vec![Vec::new()];
        let mut x = 0;
        for (w, k) in widths {
            let cur = lines.last_mut().unwrap();
            if !cur.is_empty() && x + gap + w > width {
                lines.push(vec![(w, k)]);
                x = w;
            } else {
                x += if cur.is_empty() { 0 } else { gap } + w;
                cur.push((w, k));
            }
        }
        let mut out = Vec::new();
        for (li, line) in lines.iter().enumerate() {
            let total: i32 = line.iter().map(|(w, _)| *w).sum::<i32>() + gap * (line.len() as i32 - 1).max(0);
            let mut x = width - total;
            let y = li as i32 * (h + gap);
            for (w, k) in line {
                out.push((Rect::new(x, y, *w, h), *k));
                x += w + gap;
            }
        }
        out
    }

    /// Height for a row `width` wide.
    pub fn height(&self, c: &Canvas, width: i32) -> i32 {
        self.layout(c, width).iter().map(|(r, _)| r.bottom()).max().unwrap_or(0)
    }

    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let rad = c.s(12.0);
        for (cr, kind) in self.layout(c, r.w) {
            let cr = cr.offset(r.x, r.y);
            let hovered = st.mouse.map(|m| cr.contains(m)).unwrap_or(false);
            match kind {
                ChipKind::Item(i) => {
                    c.fill_round_rect_bordered(cr, rad, if hovered { SELECTED } else { RAISED }, BORDER);
                    if hovered {
                        let xr = Rect::new(cr.right() - c.s(20.0), cr.y, c.s(16.0), cr.h);
                        c.text(Rect::new(cr.x + c.s(8.0), cr.y, cr.w - c.s(26.0), cr.h), &self.items[i], F_CHIP, TEXT, Align::Left);
                        c.icon(xr, glyph::CLOSE, Font::new(Face::Icon, 8.0, REGULAR), TEXT);
                    } else {
                        c.text(cr, &self.items[i], F_CHIP, TEXT, Align::Center);
                    }
                }
                ChipKind::More(n) => {
                    c.fill_round_rect_bordered(cr, rad, if hovered { SELECTED } else { RAISED }, BORDER);
                    c.text(cr, &format!("+{}", n), F_CHIP, MUTED, Align::Center);
                }
                ChipKind::Add => {
                    c.fill_round_rect(cr, rad, if hovered { SELECTED } else { SURFACE });
                    dashed_round_rect(c, cr, rad, ACCENT);
                    c.text(cr, &self.add_label, F_CHIP, ACCENT, Align::Center);
                }
            }
        }
        if st.focused {
            if let Some((ar, _)) = self.layout(c, r.w).into_iter().find(|(_, k)| *k == ChipKind::Add) {
                focus_ring(c, ar.offset(r.x, r.y), rad);
            }
        }
    }

    pub fn hit(&self, c: &Canvas, r: Rect, p: Pt) -> Option<ChipHit> {
        for (cr, kind) in self.layout(c, r.w) {
            let cr = cr.offset(r.x, r.y);
            if !cr.contains(p) {
                continue;
            }
            return Some(match kind {
                ChipKind::Item(i) if p.x >= cr.right() - c.s(22.0) => ChipHit::Remove(i),
                ChipKind::Item(i) => ChipHit::Item(i),
                ChipKind::More(_) => ChipHit::More,
                ChipKind::Add => ChipHit::Add,
            });
        }
        None
    }
}

/// A dashed rounded outline (the "+ Add" chip).
fn dashed_round_rect(c: &mut Canvas, r: Rect, rad: i32, col: Rgb) {
    // Short dashes along the straight edges, solid arcs at the ends: reads as dashed at chip size.
    let w = c.px1(1.0);
    let dash = c.s(4.0).max(2);
    let gap = c.s(3.0).max(2);
    let mut x = r.x + rad;
    while x < r.right() - rad {
        let len = dash.min(r.right() - rad - x);
        c.fill_rect(Rect::new(x, r.y, len, w), col);
        c.fill_rect(Rect::new(x, r.bottom() - w, len, w), col);
        x += dash + gap;
    }
    // Ends: draw the full outline clipped to the end caps.
    c.push_clip(Rect::new(r.x, r.y, rad, r.h));
    c.stroke_round_rect(r, rad, col, w);
    c.pop_clip();
    c.push_clip(Rect::new(r.right() - rad, r.y, rad, r.h));
    c.stroke_round_rect(r, rad, col, w);
    c.pop_clip();
}

// ---------------------------------------------------------------- Button

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Normal,
    Link,
    /// Normal look with a red label (destructive confirmations).
    Danger,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub label: String,
    pub style: ButtonStyle,
    /// Toast-sized: 12 px text, 5 x 12 padding.
    pub small: bool,
}

impl Button {
    pub fn new(label: &str, style: ButtonStyle) -> Button {
        Button { label: label.to_string(), style, small: false }
    }
    pub fn small(label: &str, style: ButtonStyle) -> Button {
        Button { label: label.to_string(), style, small: true }
    }
    fn font(&self) -> Font {
        if self.small {
            F_SMALL_BUTTON
        } else {
            F_BUTTON
        }
    }
    pub fn size(&self, c: &Canvas) -> (i32, i32) {
        let tw = c.measure(&self.label, self.font()).0;
        if self.small {
            (tw + c.s(24.0) + 2, c.s(26.0))
        } else {
            (tw + c.s(32.0) + 2, c.s(CTRL_H))
        }
    }
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let rad = c.s(RADIUS);
        let f = self.font();
        match self.style {
            ButtonStyle::Primary => {
                c.fill_round_rect(r, rad, if st.hover || st.pressed { ACCENT_SOFT } else { ACCENT });
                c.text(r, &self.label, f, ON_ACCENT, Align::Center);
            }
            ButtonStyle::Normal | ButtonStyle::Danger => {
                let fill = if st.disabled { SURFACE } else if st.pressed { BORDER } else if st.hover { SELECTED } else { RAISED };
                c.fill_round_rect_bordered(r, rad, fill, BORDER);
                let col = if st.disabled {
                    MUTED
                } else if self.style == ButtonStyle::Danger {
                    ERROR
                } else {
                    TEXT
                };
                c.text(r, &self.label, f, col, Align::Center);
            }
            ButtonStyle::Link => {
                let col = if st.hover { TEXT } else { MUTED };
                c.text(r, &self.label, f, col, Align::Center);
                let (tw, th) = c.measure(&self.label, f);
                c.hline(r.cx() - tw / 2, r.cy() + th / 2 - c.s(2.0), tw, col);
            }
        }
        if st.focused {
            focus_ring(c, r, rad);
        }
    }
}

// ---------------------------------------------------------------- HotkeyField

#[derive(Debug, Clone, PartialEq)]
pub struct HotkeyField {
    /// "Ctrl+Alt+M" or empty.
    pub keys: String,
    pub recording: bool,
    /// Modifiers held so far while recording ("Ctrl + Alt").
    pub partial: String,
}

impl HotkeyField {
    /// "Ctrl+Alt+M" shown as "Ctrl + Alt + M".
    pub fn display(keys: &str) -> String {
        keys.split('+').map(|k| k.trim()).filter(|k| !k.is_empty()).collect::<Vec<_>>().join(" + ")
    }
    pub fn paint(&self, c: &mut Canvas, r: Rect, st: &WState) {
        let text = if self.recording {
            if self.partial.is_empty() {
                "Press a shortcut…".to_string()
            } else {
                format!("{} + …", self.partial)
            }
        } else if self.keys.is_empty() {
            "Not set".to_string()
        } else {
            Self::display(&self.keys)
        };
        let pill = Pill { text, chevron: false, muted: !self.recording && self.keys.is_empty(), accent_border: self.recording };
        pill.paint(c, r, st);
    }
}

// ---------------------------------------------------------------- Bar

/// A meter: `frac` filled in `color`, with an optional white marker at `marker`.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    pub frac: f32,
    pub color: Rgb,
    pub marker: Option<f32>,
}

impl Bar {
    pub fn paint(&self, c: &mut Canvas, r: Rect, _st: &WState) {
        let rad = r.h / 2;
        c.fill_round_rect(r, rad, RAISED);
        let fw = (r.w as f32 * self.frac.clamp(0.0, 1.0)).round() as i32;
        if fw > 0 {
            c.fill_round_rect(Rect::new(r.x, r.y, fw.max(r.h), r.h), rad, self.color);
        }
        if let Some(m) = self.marker {
            let mx = r.x + (r.w as f32 * m.clamp(0.0, 1.0)).round() as i32;
            let w = c.px1(2.0);
            c.fill_rect(Rect::new(mx - w / 2, r.y - c.s(4.0), w, r.h + c.s(8.0)), TEXT);
        }
    }
}

/// A group's usage bar per docs/UI.md: RAM now as a share of `max(cap x 1.5, RAM now)`, lime,
/// amber over the cap, a marker at the cap; with no cap the bar is muted and has no marker.
pub fn usage_bar(ram_mb: u64, cap_mb: u64) -> Bar {
    if cap_mb == 0 {
        return Bar { frac: if ram_mb > 0 { 1.0 } else { 0.0 }, color: MUTED, marker: None };
    }
    let scale = (cap_mb as f64 * 1.5).max(ram_mb as f64);
    Bar {
        frac: (ram_mb as f64 / scale) as f32,
        color: if ram_mb > cap_mb { WARN } else { ACCENT },
        marker: Some((cap_mb as f64 / scale) as f32),
    }
}

// ---------------------------------------------------------------- Scrolling

/// Vertical scroll state of a viewport, with the thin lime thumb.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scroll {
    pub y: i32,
    pub content: i32,
    pub view: Rect,
}

impl Scroll {
    pub fn max(&self) -> i32 {
        (self.content - self.view.h).max(0)
    }
    pub fn clamp(&mut self) {
        self.y = self.y.clamp(0, self.max());
    }
    /// Scroll by a wheel delta (120 per notch), `step` pixels per notch.
    pub fn wheel(&mut self, delta: i32, step: i32) {
        self.y -= delta * step / 120;
        self.clamp();
    }
    /// Make `r` (content coordinates already offset by -y) visible.
    pub fn reveal(&mut self, r: Rect, margin: i32) {
        if r.y < self.view.y + margin {
            self.y -= self.view.y + margin - r.y;
        } else if r.bottom() > self.view.bottom() - margin {
            self.y += r.bottom() - (self.view.bottom() - margin);
        }
        self.clamp();
    }
    pub fn thumb(&self, c: &Canvas) -> Option<Rect> {
        if self.content <= self.view.h || self.view.h <= 0 {
            return None;
        }
        let w = c.s(4.0).max(3);
        let track = self.view.inset(0, c.s(4.0));
        let h = ((track.h as i64 * self.view.h as i64 / self.content as i64) as i32).max(c.s(32.0));
        let y = track.y + ((track.h - h) as i64 * self.y as i64 / self.max().max(1) as i64) as i32;
        Some(Rect::new(self.view.right() - w - c.s(3.0), y, w, h))
    }
    pub fn paint(&self, c: &mut Canvas, active: bool) {
        if let Some(t) = self.thumb(c) {
            c.fill_round_rect(t, t.w / 2, if active { ACCENT } else { ACCENT.over(BG, 0.55) });
        }
    }
    /// The scroll position for dragging the thumb so that its top is at `thumb_top`.
    pub fn y_for_thumb(&self, c: &Canvas, thumb_top: i32) -> i32 {
        let Some(t) = self.thumb(c) else { return 0 };
        let track = self.view.inset(0, c.s(4.0));
        let span = (track.h - t.h).max(1);
        ((thumb_top - track.y).clamp(0, span) as i64 * self.max() as i64 / span as i64) as i32
    }
}

// ---------------------------------------------------------------- Dialog (confirmations)

/// A centred confirmation card drawn over a dimmed window.
#[derive(Debug, Clone, PartialEq)]
pub struct Dialog {
    pub title: String,
    pub body: String,
    pub ok: Button,
    pub cancel: Button,
}

impl Dialog {
    /// Card, OK and Cancel rectangles inside a window of `area`.
    pub fn layout(&self, c: &Canvas, area: Rect) -> (Rect, Rect, Rect) {
        let w = c.s(420.0).min(area.w - c.s(32.0));
        let pad = c.s(24.0);
        let inner = w - 2 * pad;
        let title_h = c.wrapped_height(&self.title.to_uppercase(), F_DIALOG_TITLE, inner, 2);
        let body_h = c.wrapped_height(&self.body, F_VALUE, inner, 6);
        let (ow, bh) = self.ok.size(c);
        let (cw, _) = self.cancel.size(c);
        let h = pad + title_h + c.s(10.0) + body_h + c.s(22.0) + bh + pad;
        let card = area.centered(w, h);
        let by = card.bottom() - pad - bh;
        let cancel = Rect::new(card.right() - pad - cw, by, cw, bh);
        let ok = Rect::new(cancel.x - c.s(8.0) - ow, by, ow, bh);
        (card, ok, cancel)
    }
    pub fn paint(&self, c: &mut Canvas, area: Rect, hover: Option<Pt>, focus_ok: bool) {
        c.fill_round_rect_alpha(area, 0, BG, 190);
        let (card, ok, cancel) = self.layout(c, area);
        c.fill_round_rect_bordered(card, c.s(CARD_RADIUS), SURFACE, BORDER);
        let pad = c.s(24.0);
        let inner = Rect::new(card.x + pad, card.y + pad, card.w - 2 * pad, card.h);
        let th = c.text_wrapped(inner, &self.title.to_uppercase(), F_DIALOG_TITLE, TEXT, 2);
        c.text_wrapped(Rect::new(inner.x, inner.y + th + c.s(10.0), inner.w, inner.h), &self.body, F_VALUE, TEXT_DIM, 6);
        let hv = |r: Rect| hover.map(|p| r.contains(p)).unwrap_or(false);
        self.ok.paint(c, ok, &WState { hover: hv(ok), focused: focus_ok, ..Default::default() });
        self.cancel.paint(c, cancel, &WState { hover: hv(cancel), focused: !focus_ok, ..Default::default() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Scale;

    #[test]
    fn slider_percent_and_gb_math() {
        assert_eq!(pct_from_frac(0.0), 0.0);
        assert_eq!(pct_from_frac(1.0), 25.0);
        assert_eq!(pct_from_frac(0.192), 4.8);
        assert_eq!(pct_from_frac(0.5), 12.5);
        assert!((frac_from_pct(4.8) - 0.192).abs() < 1e-6);
        assert_eq!(frac_from_pct(40.0), 1.0);
        assert_eq!(snap_pct(0.04), 0.0);
        assert_eq!(snap_pct(0.05), 0.1);
        assert_eq!(snap_pct(2.449), 2.4);
        // Round trip through every 0.1 % step.
        for i in 0..=250 {
            let pct = i as f64 / 10.0;
            assert_eq!(pct_from_frac(frac_from_pct(pct)), pct, "{pct}");
        }
        assert!((pct_to_gb(4.8, 128.0) - 6.144).abs() < 1e-9);
        assert!((gb_to_pct(6.144, 128.0) - 4.8).abs() < 1e-9);
        assert_eq!(gb_to_pct(1.0, 0.0), 0.0);
        assert_eq!(format_gb(pct_to_gb(4.8, 127.7)), "6.1 GB");
        assert_eq!(format_gb(pct_to_gb(0.2, 128.0)), "262 MB");
        assert_eq!(format_gb(0.25), "256 MB");
        assert_eq!(format_pct(2.4), "2.4%");
    }

    #[test]
    fn slider_pixels_map_to_fractions() {
        for scale in [1.0, 1.5] {
            let c = Canvas::new(10, 10, Scale(scale));
            let r = Rect::new(100, 0, c.s(300.0), c.s(24.0));
            assert_eq!(Slider::frac_at(&c, r, r.x), 0.0);
            assert_eq!(Slider::frac_at(&c, r, r.right()), 1.0);
            let mid = Slider::frac_at(&c, r, r.cx());
            assert!((mid - 0.5).abs() < 0.01);
        }
    }

    #[test]
    fn usage_bar_rules() {
        let b = usage_bar(4198, 3174);
        assert_eq!(b.color, WARN);
        assert_eq!(b.frac, 4198.0 / (3174.0 * 1.5));
        let under = usage_bar(1126, 1946);
        assert_eq!(under.color, ACCENT);
        assert!((under.marker.unwrap() - 2.0 / 3.0).abs() < 1e-6);
        let none = usage_bar(6554, 0);
        assert_eq!((none.color, none.marker), (MUTED, None));
    }

    #[test]
    fn chips_fold_and_fit() {
        for scale in [1.0, 1.5] {
            let c = Canvas::new(10, 10, Scale(scale));
            let width = c.s(420.0);
            let items: Vec<String> = (0..22).map(|i| format!("process{}", i)).collect();
            let chips = Chips { items, add_label: "+ Add…".into(), max_lines: 1, expanded: false };
            let lay = chips.layout(&c, width);
            assert!(lay.iter().any(|(_, k)| matches!(k, ChipKind::More(_))));
            assert!(lay.iter().all(|(r, _)| r.y == 0 && r.x >= 0 && r.right() <= width));
            for (i, (a, _)) in lay.iter().enumerate() {
                for (b, _) in &lay[i + 1..] {
                    assert!(!a.intersects(b));
                }
            }
            let open = Chips { expanded: true, ..chips };
            let lay = open.layout(&c, width);
            assert!(lay.iter().all(|(_, k)| !matches!(k, ChipKind::More(_))));
            assert!(open.height(&c, width) > Chips::chip_h(&c));
        }
    }

    #[test]
    fn scroll_thumb_and_reveal() {
        let c = Canvas::new(10, 10, Scale(1.0));
        let mut s = Scroll { y: 0, content: 2000, view: Rect::new(0, 60, 900, 600) };
        let t = s.thumb(&c).unwrap();
        s.y = s.max();
        let t2 = s.thumb(&c).unwrap();
        assert!(t2.y > t.y && t2.bottom() <= s.view.bottom());
        assert_eq!(s.y_for_thumb(&c, t2.y), s.max());
        s.wheel(120, 66);
        assert_eq!(s.y, s.max() - 66);
        s.y = 0;
        s.reveal(Rect::new(0, 700, 10, 40), 10);
        assert_eq!(s.y, 700 + 40 - (660 - 10));
    }
}

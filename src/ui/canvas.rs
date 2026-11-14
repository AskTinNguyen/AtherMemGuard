//! `Canvas`: the double-buffered paint target every surface draws into.
//!
//! One 32-bit top-down DIB section, drawn on by GDI (text, for ClearType) and by GDI+
//! (anti-aliased shapes) through a GDI+ bitmap that shares the same pixels. A window blits
//! it to the screen; snapshots save it as PNG. Lengths passed in are device pixels; use
//! `canvas.s(n)` to scale 96-DPI lengths.

use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{LPARAM, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::{
    CombineModeReplace, DashStyleDash, FillModeAlternate, GdipAddPathArc, GdipAddPathPolygon, GdipClosePathFigure,
    GdipCreateBitmapFromScan0, GdipCreatePath, GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics,
    GdipDeletePath, GdipDeletePen, GdipDisposeImage, GdipDrawLine, GdipDrawPath, GdipFillEllipse, GdipFillPath,
    GdipGetImageGraphicsContext, GdipResetClip, GdipSaveImageToFile, GdipSetClipRectI, GdipSetPenDashStyle,
    GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdiplusStartup, GdiplusStartupInput, GpBitmap, GpBrush, GpGraphics,
    GpImage, GpPath, GpPen, GpSolidFill, PixelOffsetModeHalf, PointF, SmoothingModeAntiAlias8x8, UnitPixel,
};

use super::geom::{Pt, Rect};
use super::theme::{Face, Font, Rgb, Scale};
use crate::win::wide;

const PIXEL_FORMAT_32BPP_RGB: i32 = 0x0002_2009;
const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;

/// Start GDI+ once per process; it is never shut down.
pub(crate) fn gdiplus() {
    static TOKEN: OnceLock<usize> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut token = 0usize;
        let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 };
        unsafe { GdiplusStartup(&mut token, &input, null_mut()) };
        token
    });
}

unsafe extern "system" fn font_found(_: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, found: LPARAM) -> i32 {
    *(found as *mut bool) = true;
    0
}

fn font_installed(face: &str) -> bool {
    unsafe {
        let mut lf: LOGFONTW = zeroed();
        lf.lfCharSet = DEFAULT_CHARSET as u8;
        let w = wide(face);
        let n = (w.len() - 1).min(31);
        lf.lfFaceName[..n].copy_from_slice(&w[..n]);
        let mut found = false;
        let dc = GetDC(null_mut());
        EnumFontFamiliesExW(dc, &lf, Some(font_found), &mut found as *mut bool as LPARAM, 0);
        ReleaseDC(null_mut(), dc);
        found
    }
}

/// The installed face name and weight for a role.
fn resolve_face(face: Face, weight: u16) -> (&'static str, i32) {
    static BODY: OnceLock<&'static str> = OnceLock::new();
    static DISPLAY: OnceLock<bool> = OnceLock::new();
    static EYEBROW: OnceLock<bool> = OnceLock::new();
    match face {
        Face::Body => (*BODY.get_or_init(|| if font_installed("Segoe UI Variable Text") { "Segoe UI Variable Text" } else { "Segoe UI" }), weight as i32),
        Face::Display => {
            if *DISPLAY.get_or_init(|| font_installed("Bahnschrift SemiBold Condensed")) {
                ("Bahnschrift SemiBold Condensed", 400)
            } else {
                ("Bahnschrift", 700)
            }
        }
        Face::Eyebrow => {
            if *EYEBROW.get_or_init(|| font_installed("Bahnschrift SemiBold")) {
                ("Bahnschrift SemiBold", 400)
            } else {
                ("Bahnschrift", 700)
            }
        }
        Face::Icon => ("Segoe Fluent Icons", 400),
    }
}

/// A GDI font for a role at `scale`, for native controls (the temporary EDIT). The caller deletes it.
pub(crate) fn create_hfont(f: Font, scale: Scale) -> HFONT {
    let (face, weight) = resolve_face(f.face, f.weight);
    let w = wide(face);
    unsafe { CreateFontW(-scale.s(f.px).max(1), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0, w.as_ptr()) }
}

/// Horizontal alignment for single-line text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

pub struct Canvas {
    dc: HDC,
    bmp: HBITMAP,
    old_bmp: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
    gp_bitmap: *mut GpBitmap,
    gfx: *mut GpGraphics,
    pub scale: Scale,
    fonts: RefCell<Vec<((Face, i32, u16), HFONT)>>,
    fade: Option<(Rgb, f32)>,
    clips: Vec<Rect>,
}

impl Canvas {
    /// An off-screen canvas of `w` x `h` device pixels at `scale`.
    pub fn new(w: i32, h: i32, scale: Scale) -> Canvas {
        gdiplus();
        let mut c = Canvas {
            dc: unsafe { CreateCompatibleDC(null_mut()) },
            bmp: null_mut(),
            old_bmp: null_mut(),
            bits: null_mut(),
            w: 0,
            h: 0,
            gp_bitmap: null_mut(),
            gfx: null_mut(),
            scale,
            fonts: RefCell::new(Vec::new()),
            fade: None,
            clips: Vec::new(),
        };
        c.resize(w, h);
        c
    }

    pub fn width(&self) -> i32 {
        self.w
    }
    pub fn height(&self) -> i32 {
        self.h
    }
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    /// `S(n)`: device pixels for a 96-DPI length.
    pub fn s(&self, v: f32) -> i32 {
        self.scale.s(v)
    }
    /// Like `s`, at least one pixel.
    pub fn px1(&self, v: f32) -> i32 {
        self.scale.px1(v)
    }

    /// Change the size (contents are lost) and keep the fonts.
    pub fn resize(&mut self, w: i32, h: i32) {
        let (w, h) = (w.max(1), h.max(1));
        if w == self.w && h == self.h && !self.bmp.is_null() {
            return;
        }
        unsafe {
            self.free_bitmap();
            let mut bi: BITMAPINFO = zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            let mut bits: *mut core::ffi::c_void = null_mut();
            self.bmp = CreateDIBSection(self.dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            self.bits = bits as *mut u32;
            self.old_bmp = SelectObject(self.dc, self.bmp);
            self.w = w;
            self.h = h;
            SetBkMode(self.dc, TRANSPARENT as i32);
            if !self.bits.is_null() {
                GdipCreateBitmapFromScan0(w, h, w * 4, PIXEL_FORMAT_32BPP_RGB, self.bits as *const u8, &mut self.gp_bitmap);
                if !self.gp_bitmap.is_null() {
                    GdipGetImageGraphicsContext(self.gp_bitmap as *mut GpImage, &mut self.gfx);
                }
                if !self.gfx.is_null() {
                    GdipSetSmoothingMode(self.gfx, SmoothingModeAntiAlias8x8);
                    GdipSetPixelOffsetMode(self.gfx, PixelOffsetModeHalf);
                }
            }
        }
        self.clips.clear();
    }

    /// Change the DPI scale; fonts are recreated on demand.
    pub fn set_scale(&mut self, scale: Scale) {
        if scale != self.scale {
            self.scale = scale;
            self.free_fonts();
        }
    }

    unsafe fn free_bitmap(&mut self) {
        if !self.gfx.is_null() {
            GdipDeleteGraphics(self.gfx);
            self.gfx = null_mut();
        }
        if !self.gp_bitmap.is_null() {
            GdipDisposeImage(self.gp_bitmap as *mut GpImage);
            self.gp_bitmap = null_mut();
        }
        if !self.bmp.is_null() {
            SelectObject(self.dc, self.old_bmp);
            DeleteObject(self.bmp);
            self.bmp = null_mut();
            self.bits = null_mut();
        }
    }

    fn free_fonts(&mut self) {
        for (_, f) in self.fonts.borrow_mut().drain(..) {
            unsafe { DeleteObject(f) };
        }
    }

    /// Draw everything after this at `opacity` over `under` (inherited values at 50%).
    pub fn set_fade(&mut self, fade: Option<(Rgb, f32)>) {
        self.fade = fade;
    }

    fn col(&self, c: Rgb) -> Rgb {
        match self.fade {
            Some((under, a)) => c.over(under, a),
            None => c,
        }
    }

    // ------------------------------------------------------------ clipping

    /// Restrict drawing to `r` (intersected with the current clip) until `pop_clip`.
    pub fn push_clip(&mut self, r: Rect) {
        let r = match self.clips.last() {
            Some(c) => {
                let l = r.x.max(c.x);
                let t = r.y.max(c.y);
                Rect::ltrb(l, t, r.right().min(c.right()).max(l), r.bottom().min(c.bottom()).max(t))
            }
            None => r,
        };
        self.clips.push(r);
        self.apply_clip();
    }

    pub fn pop_clip(&mut self) {
        self.clips.pop();
        self.apply_clip();
    }

    fn apply_clip(&self) {
        unsafe {
            match self.clips.last() {
                Some(r) => {
                    let rgn = CreateRectRgn(r.x, r.y, r.right(), r.bottom());
                    SelectClipRgn(self.dc, rgn);
                    DeleteObject(rgn);
                    if !self.gfx.is_null() {
                        GdipSetClipRectI(self.gfx, r.x, r.y, r.w, r.h, CombineModeReplace);
                    }
                }
                None => {
                    SelectClipRgn(self.dc, null_mut());
                    if !self.gfx.is_null() {
                        GdipResetClip(self.gfx);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ shapes

    /// Solid rectangle, pixel exact.
    pub fn fill_rect(&mut self, r: Rect, c: Rgb) {
        if r.is_empty() {
            return;
        }
        let c = self.col(c);
        unsafe {
            SetBkColor(self.dc, c.colorref());
            let rc = r.to_win();
            ExtTextOutW(self.dc, 0, 0, ETO_OPAQUE, &rc, null(), 0, null());
        }
    }

    fn with_brush(&self, c: Rgb, alpha: u8, f: impl FnOnce(*mut GpGraphics, *mut GpBrush)) {
        if self.gfx.is_null() {
            return;
        }
        unsafe {
            GdiFlush();
            let mut brush: *mut GpSolidFill = null_mut();
            GdipCreateSolidFill(self.col(c).argb(alpha), &mut brush);
            if !brush.is_null() {
                f(self.gfx, brush as *mut GpBrush);
                GdipDeleteBrush(brush as *mut GpBrush);
            }
        }
    }

    fn round_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> *mut GpPath {
        let mut path: *mut GpPath = null_mut();
        unsafe {
            GdipCreatePath(FillModeAlternate, &mut path);
            if path.is_null() {
                return path;
            }
            let d = (radius * 2.0).min(w).min(h);
            if d <= 0.5 {
                GdipAddPathArc(path, x, y, 0.0, 0.0, 180.0, 90.0);
                let pts = [PointF { X: x, Y: y }, PointF { X: x + w, Y: y }, PointF { X: x + w, Y: y + h }, PointF { X: x, Y: y + h }];
                GdipAddPathPolygon(path, pts.as_ptr(), 4);
            } else {
                GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
                GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
                GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
                GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
                GdipClosePathFigure(path);
            }
        }
        path
    }

    /// Anti-aliased filled rounded rectangle; `radius` in device pixels.
    pub fn fill_round_rect(&mut self, r: Rect, radius: i32, c: Rgb) {
        self.fill_round_rect_alpha(r, radius, c, 255);
    }

    pub fn fill_round_rect_alpha(&mut self, r: Rect, radius: i32, c: Rgb, alpha: u8) {
        if r.is_empty() {
            return;
        }
        if radius <= 0 && alpha == 255 {
            return self.fill_rect(r, c);
        }
        self.with_brush(c, alpha, |g, b| unsafe {
            let path = Self::round_path(r.x as f32, r.y as f32, r.w as f32, r.h as f32, radius as f32);
            if !path.is_null() {
                GdipFillPath(g, b, path);
                GdipDeletePath(path);
            }
        });
    }

    /// Rounded rectangle outline, `width` device pixels wide, drawn inside `r`.
    pub fn stroke_round_rect(&mut self, r: Rect, radius: i32, c: Rgb, width: i32) {
        if r.is_empty() || self.gfx.is_null() {
            return;
        }
        let wf = width.max(1) as f32;
        unsafe {
            GdiFlush();
            let mut pen: *mut GpPen = null_mut();
            GdipCreatePen1(self.col(c).argb(255), wf, UnitPixel, &mut pen);
            if pen.is_null() {
                return;
            }
            let h = wf / 2.0;
            let path = Self::round_path(r.x as f32 + h, r.y as f32 + h, r.w as f32 - wf, r.h as f32 - wf, (radius as f32 - h).max(0.0));
            if !path.is_null() {
                GdipDrawPath(self.gfx, pen, path);
                GdipDeletePath(path);
            }
            GdipDeletePen(pen);
        }
    }

    /// Filled rounded rectangle with a one-pixel (scaled) border of another colour.
    pub fn fill_round_rect_bordered(&mut self, r: Rect, radius: i32, fill: Rgb, border: Rgb) {
        let bw = self.px1(1.0);
        self.fill_round_rect(r, radius, border);
        self.fill_round_rect(r.inset(bw, bw), (radius - bw).max(0), fill);
    }

    pub fn fill_ellipse(&mut self, r: Rect, c: Rgb) {
        if r.is_empty() {
            return;
        }
        self.with_brush(c, 255, |g, b| unsafe {
            GdipFillEllipse(g, b, r.x as f32, r.y as f32, r.w as f32, r.h as f32);
        });
    }

    /// Anti-aliased line from `a` to `b`. `dash` draws it dashed.
    pub fn line(&mut self, a: Pt, b: Pt, c: Rgb, width: f32, dash: bool) {
        if self.gfx.is_null() {
            return;
        }
        unsafe {
            GdiFlush();
            let mut pen: *mut GpPen = null_mut();
            GdipCreatePen1(self.col(c).argb(255), width, UnitPixel, &mut pen);
            if pen.is_null() {
                return;
            }
            if dash {
                GdipSetPenDashStyle(pen, DashStyleDash);
            }
            GdipDrawLine(self.gfx, pen, a.x as f32, a.y as f32, b.x as f32, b.y as f32);
            GdipDeletePen(pen);
        }
    }

    /// Horizontal hairline across `r`'s top edge, one scaled pixel high.
    pub fn hline(&mut self, x: i32, y: i32, w: i32, c: Rgb) {
        let h = self.px1(1.0);
        self.fill_rect(Rect::new(x, y, w, h), c);
    }

    // ------------------------------------------------------------ text

    fn font(&self, f: Font) -> HFONT {
        let px = self.scale.s(f.px).max(1);
        let key = (f.face, px, f.weight);
        if let Some((_, h)) = self.fonts.borrow().iter().find(|(k, _)| *k == key) {
            return *h;
        }
        let h = create_hfont(f, self.scale);
        self.fonts.borrow_mut().push((key, h));
        h
    }

    /// Width and height in device pixels of one line of `s`.
    pub fn measure(&self, s: &str, f: Font) -> (i32, i32) {
        self.measure_spaced(s, f, 0)
    }

    /// Like `measure`, with `extra` pixels added after every character.
    pub fn measure_spaced(&self, s: &str, f: Font, extra: i32) -> (i32, i32) {
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let old = SelectObject(self.dc, self.font(f));
            let prev = SetTextCharacterExtra(self.dc, extra);
            let mut sz = SIZE { cx: 0, cy: 0 };
            GetTextExtentPoint32W(self.dc, w.as_ptr(), w.len() as i32, &mut sz);
            let mut tm: TEXTMETRICW = zeroed();
            GetTextMetricsW(self.dc, &mut tm);
            SetTextCharacterExtra(self.dc, prev);
            SelectObject(self.dc, old);
            (sz.cx, tm.tmHeight)
        }
    }

    /// Line height of a font in device pixels.
    pub fn line_height(&self, f: Font) -> i32 {
        self.measure("Ag", f).1
    }

    unsafe fn draw(&self, r: Rect, s: &str, f: Font, c: Rgb, flags: u32, extra: i32) {
        let w: Vec<u16> = s.encode_utf16().collect();
        let old = SelectObject(self.dc, self.font(f));
        let prev = SetTextCharacterExtra(self.dc, extra);
        SetTextColor(self.dc, self.col(c).colorref());
        let mut rc: RECT = r.to_win();
        DrawTextW(self.dc, w.as_ptr(), w.len() as i32, &mut rc, flags | DT_NOPREFIX);
        SetTextCharacterExtra(self.dc, prev);
        SelectObject(self.dc, old);
    }

    /// One line of text, vertically centred in `r`, cut with an ellipsis when too long.
    pub fn text(&mut self, r: Rect, s: &str, f: Font, c: Rgb, align: Align) {
        let h = match align {
            Align::Left => DT_LEFT,
            Align::Center => DT_CENTER,
            Align::Right => DT_RIGHT,
        };
        unsafe { self.draw(r, s, f, c, h | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS, 0) };
    }

    /// One line of text at the top of `r` (no vertical centring).
    pub fn text_top(&mut self, r: Rect, s: &str, f: Font, c: Rgb, align: Align) {
        let h = match align {
            Align::Left => DT_LEFT,
            Align::Center => DT_CENTER,
            Align::Right => DT_RIGHT,
        };
        unsafe { self.draw(r, s, f, c, h | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS, 0) };
    }

    /// Letter-spaced single line (eyebrows, section headers), vertically centred. Positioned
    /// by measuring (DrawText's own fitting ignores the character extra and cuts letters).
    pub fn spaced_text(&mut self, r: Rect, s: &str, f: Font, c: Rgb, extra: i32, align: Align) {
        let (w, _) = self.measure_spaced(s, f, extra);
        let w = w - extra; // no trailing space after the last letter
        let x = match align {
            Align::Left => r.x,
            Align::Center => r.x + (r.w - w) / 2,
            Align::Right => r.right() - w,
        };
        let rr = Rect::new(x, r.y, w + extra + 2, r.h);
        unsafe { self.draw(rr, s, f, c, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP, extra) };
    }

    /// Break `s` into lines no wider than `width`, at most `max_lines`; the last line gets
    /// an ellipsis when text is left over. Words longer than a line are cut by DrawText.
    pub fn wrap_lines(&self, s: &str, f: Font, width: i32, max_lines: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for para in s.split('\n') {
            let mut line = String::new();
            for word in para.split(' ').filter(|w| !w.is_empty()) {
                let candidate = if line.is_empty() { word.to_string() } else { format!("{} {}", line, word) };
                if line.is_empty() || self.measure(&candidate, f).0 <= width {
                    line = candidate;
                } else {
                    lines.push(std::mem::take(&mut line));
                    line = word.to_string();
                }
            }
            lines.push(line);
        }
        if lines.len() > max_lines {
            let rest = lines[max_lines - 1..].join(" ");
            lines.truncate(max_lines - 1);
            lines.push(self.ellipsize(&rest, f, width));
        }
        lines
    }

    /// `s` cut to fit `width`, ending in "…" when cut.
    pub fn ellipsize(&self, s: &str, f: Font, width: i32) -> String {
        if self.measure(s, f).0 <= width {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let t: String = chars[..mid].iter().collect::<String>() + "…";
            if self.measure(&t, f).0 <= width {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
    }

    /// Draw wrapped lines from the top of `r`; returns the height used.
    pub fn text_wrapped(&mut self, r: Rect, s: &str, f: Font, c: Rgb, max_lines: usize) -> i32 {
        let lh = self.line_height(f);
        let lines = self.wrap_lines(s, f, r.w, max_lines);
        let mut y = r.y;
        for line in &lines {
            unsafe { self.draw(Rect::new(r.x, y, r.w, lh), line, f, c, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS, 0) };
            y += lh;
        }
        y - r.y
    }

    /// Height `text_wrapped` would use.
    pub fn wrapped_height(&self, s: &str, f: Font, width: i32, max_lines: usize) -> i32 {
        self.wrap_lines(s, f, width, max_lines).len() as i32 * self.line_height(f)
    }

    /// A Segoe Fluent Icons glyph centred in `r`.
    pub fn icon(&mut self, r: Rect, glyph: char, f: Font, c: Rgb) {
        let mut buf = [0u16; 2];
        let s = glyph.encode_utf16(&mut buf);
        let s = String::from_utf16_lossy(s);
        unsafe { self.draw(r, &s, f, c, DT_CENTER | DT_VCENTER | DT_SINGLELINE, 0) };
    }

    // ------------------------------------------------------------ logo

    /// The Ather mark on its tile, filling `r` (square).
    pub fn logo(&mut self, r: Rect) {
        let size = r.w.min(r.h).max(1) as usize;
        let src = crate::logo::render(size, crate::engine::Level::Normal);
        unsafe { GdiFlush() };
        if self.bits.is_null() {
            return;
        }
        let clip = self.clips.last().copied().unwrap_or(self.bounds());
        for y in 0..size as i32 {
            for x in 0..size as i32 {
                let (dx, dy) = (r.x + x, r.y + y);
                if !clip.contains(Pt::new(dx, dy)) || dx < 0 || dy < 0 || dx >= self.w || dy >= self.h {
                    continue;
                }
                let i = (y as usize * size + x as usize) * 4;
                let a = src[i + 3] as u32;
                if a == 0 {
                    continue;
                }
                let px = unsafe { &mut *self.bits.add((dy * self.w + dx) as usize) };
                let (db, dg, dr) = (*px & 0xFF, (*px >> 8) & 0xFF, (*px >> 16) & 0xFF);
                let blend = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
                let (b, g, rr) = (src[i] as u32, src[i + 1] as u32, src[i + 2] as u32);
                *px = 0xFF00_0000 | blend(rr, dr) << 16 | blend(g, dg) << 8 | blend(b, db);
            }
        }
    }

    // ------------------------------------------------------------ output

    /// Copy the canvas to a device context at (x, y).
    pub fn blit(&self, hdc: HDC, x: i32, y: i32) {
        unsafe {
            GdiFlush();
            BitBlt(hdc, x, y, self.w, self.h, self.dc, 0, 0, SRCCOPY);
        }
    }

    /// The pixel at (x, y) as RGB (tests).
    pub fn pixel(&self, x: i32, y: i32) -> Rgb {
        unsafe { GdiFlush() };
        if self.bits.is_null() || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return Rgb(0, 0, 0);
        }
        let p = unsafe { *self.bits.add((y * self.w + x) as usize) };
        Rgb((p >> 16) as u8, (p >> 8) as u8, p as u8)
    }

    /// Save as an opaque PNG.
    pub fn save_png(&self, path: &Path) -> Result<(), String> {
        unsafe { GdiFlush() };
        if self.bits.is_null() {
            return Err("nothing to save".into());
        }
        let n = (self.w * self.h) as usize;
        let mut px: Vec<u32> = unsafe { std::slice::from_raw_parts(self.bits, n) }.to_vec();
        for p in &mut px {
            *p |= 0xFF00_0000;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        }
        // {557CF406-1A04-11D3-9A73-0000F81EF32E}: the PNG encoder.
        let png = windows_sys::core::GUID { data1: 0x557C_F406, data2: 0x1A04, data3: 0x11D3, data4: [0x9A, 0x73, 0x00, 0x00, 0xF8, 0x1E, 0xF3, 0x2E] };
        unsafe {
            let mut bmp: *mut GpBitmap = null_mut();
            GdipCreateBitmapFromScan0(self.w, self.h, self.w * 4, PIXEL_FORMAT_32BPP_ARGB, px.as_ptr() as *const u8, &mut bmp);
            if bmp.is_null() {
                return Err("GDI+ could not create the image".into());
            }
            let name = wide(&path.to_string_lossy());
            let status = GdipSaveImageToFile(bmp as *mut GpImage, name.as_ptr(), &png, null());
            GdipDisposeImage(bmp as *mut GpImage);
            if status != 0 {
                return Err(format!("{}: GDI+ error {}", path.display(), status));
            }
        }
        Ok(())
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        self.free_fonts();
        unsafe {
            self.free_bitmap();
            DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{ACCENT, BG, F_BODY, F_HINT};

    #[test]
    fn draws_and_measures() {
        let mut c = Canvas::new(200, 100, Scale(1.0));
        c.fill_rect(c.bounds(), BG);
        c.fill_round_rect(Rect::new(10, 10, 80, 40), 6, ACCENT);
        assert_eq!(c.pixel(50, 30), ACCENT);
        assert_eq!(c.pixel(150, 80), BG);
        // Corners are rounded: the very corner pixel is not lime.
        assert_ne!(c.pixel(10, 10), ACCENT);
        let (w, h) = c.measure("Hello", F_BODY);
        assert!(w > 20 && h > 10);
        let big = Canvas::new(10, 10, Scale(1.5));
        assert!(big.measure("Hello", F_BODY).0 > w);
    }

    #[test]
    fn wraps_and_ellipsizes() {
        let c = Canvas::new(10, 10, Scale(1.0));
        let text = "Balanced keeps browsers and chat usable. Aggressive halves every limit and slows other apps while Unreal runs.";
        let lines = c.wrap_lines(text, F_HINT, 200, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
        for l in &lines {
            assert!(c.measure(l, F_HINT).0 <= 200, "{l}");
        }
        assert_eq!(c.wrap_lines("short", F_HINT, 200, 2), vec!["short".to_string()]);
    }
}

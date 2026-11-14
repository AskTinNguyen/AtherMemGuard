//! The Ather "A5" mark, drawn in code so the tray icon and the exe icon need no art files.
//! Geometry and colours follow AtherScreenshot's logo.cpp: a warm near-black rounded tile,
//! a grey "A", and a lime light bar and "5".

use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{COLORREF, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush, DeleteDC,
    DeleteObject, Ellipse, GdiFlush, GetGlyphOutlineW, GetStockObject, Polygon, Rectangle, RoundRect, SelectObject,
    SetBkMode, SetTextAlign, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GGO_METRICS,
    GLYPHMETRICS, HDC, MAT2, NULL_PEN, PS_SOLID, TA_BASELINE, TA_LEFT, TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::engine::Level;
use crate::win::wide;

// Ather brand colours (AtherScreenshot src/common.h, namespace theme).
pub const BG: (u8, u8, u8) = (14, 14, 13);
pub const TILE_EDGE: (u8, u8, u8) = (44, 46, 40);
pub const LOGO_GRAY: (u8, u8, u8) = (201, 204, 205);
pub const ACCENT: (u8, u8, u8) = (212, 255, 0);
pub const WARNING: (u8, u8, u8) = (255, 184, 48);
pub const ERROR: (u8, u8, u8) = (255, 120, 110);

/// Transparent while drawing; never used by the logo itself.
const KEY: (u8, u8, u8) = (255, 0, 255);
const SUPERSAMPLE: usize = 4;

// Logo geometry in its own 462 x 478 space.
const A: [(f32, f32); 7] = [(90.0, 690.0), (270.0, 296.0), (360.0, 296.0), (540.0, 690.0), (460.0, 690.0), (313.0, 368.0), (165.0, 690.0)];
const BAR: (f32, f32, f32, f32) = (307.0, 483.0, 13.0, 127.0);
const FIVE: (f32, f32, f32, f32) = (424.0, 212.0, 128.0, 170.0);
const MIN_X: f32 = 90.0;
const MAX_X: f32 = 552.0;
const MIN_Y: f32 = 212.0;
const MAX_Y: f32 = 690.0;

fn rgb(c: (u8, u8, u8)) -> COLORREF {
    c.0 as u32 | (c.1 as u32) << 8 | (c.2 as u32) << 16
}

unsafe fn fill_shape(dc: HDC, colour: (u8, u8, u8), draw: impl FnOnce()) {
    let brush = CreateSolidBrush(rgb(colour));
    let old_brush = SelectObject(dc, brush);
    let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
    draw();
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    DeleteObject(brush);
}

/// Render the mark on its tile at `size` pixels. Returns top-down BGRA with straight alpha.
/// `level` adds a status dot: amber for elevated pressure, red for critical.
pub fn render(size: usize, level: Level) -> Vec<u8> {
    let n = size * SUPERSAMPLE;
    let mut out = vec![0u8; size * size * 4];
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = n as i32;
        bi.bmiHeader.biHeight = -(n as i32); // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            return out;
        }
        let old_bitmap = SelectObject(dc, dib);
        let ni = n as i32;
        let nf = n as f32;

        fill_shape(dc, KEY, || {
            Rectangle(dc, 0, 0, ni + 1, ni + 1);
        });

        // Tile: rounded square, radius 22% of the width.
        let diameter = (nf * 0.44) as i32;
        fill_shape(dc, if size >= 32 { TILE_EDGE } else { BG }, || {
            RoundRect(dc, 0, 0, ni + 1, ni + 1, diameter, diameter);
        });
        if size >= 32 {
            let inset = (nf / size as f32).round() as i32; // one output pixel of edge
            fill_shape(dc, BG, || {
                RoundRect(dc, inset, inset, ni + 1 - inset, ni + 1 - inset, diameter - inset * 2, diameter - inset * 2);
            });
        }

        // The mark, inset 14% on each side and centred.
        let area = nf * 0.72;
        let scale = area / (MAX_X - MIN_X).max(MAX_Y - MIN_Y);
        let off_x = (nf - (MAX_X - MIN_X) * scale) / 2.0 - MIN_X * scale;
        let off_y = (nf - (MAX_Y - MIN_Y) * scale) / 2.0 - MIN_Y * scale;
        let px = |x: f32| (x * scale + off_x).round() as i32;
        let py = |y: f32| (y * scale + off_y).round() as i32;

        let points: Vec<POINT> = A.iter().map(|&(x, y)| POINT { x: px(x), y: py(y) }).collect();
        fill_shape(dc, LOGO_GRAY, || {
            Polygon(dc, points.as_ptr(), points.len() as i32);
        });
        if size >= 20 {
            fill_shape(dc, ACCENT, || {
                Rectangle(dc, px(BAR.0), py(BAR.1), px(BAR.0 + BAR.2) + 1, py(BAR.1 + BAR.3) + 1);
            });
        }

        // The "5": Bahnschrift Bold, scaled so its ink fills the FIVE box.
        let face = wide("Bahnschrift");
        let identity = MAT2 {
            eM11: windows_sys::Win32::Graphics::Gdi::FIXED { fract: 0, value: 1 },
            eM12: windows_sys::Win32::Graphics::Gdi::FIXED { fract: 0, value: 0 },
            eM21: windows_sys::Win32::Graphics::Gdi::FIXED { fract: 0, value: 0 },
            eM22: windows_sys::Win32::Graphics::Gdi::FIXED { fract: 0, value: 1 },
        };
        let make_font = |height: i32| CreateFontW(-height, 0, 0, 0, 700, 0, 0, 0, 0, 0, 0, 3 /* NONANTIALIASED_QUALITY */, 0, face.as_ptr());
        let measure = |dc: HDC| {
            let mut gm: GLYPHMETRICS = zeroed();
            GetGlyphOutlineW(dc, '5' as u32, GGO_METRICS, &mut gm, 0, null_mut(), &identity);
            gm
        };
        let trial = make_font(1000);
        let old_font = SelectObject(dc, trial);
        let trial_ink = measure(dc).gmBlackBoxY.max(1) as f32;
        let target = FIVE.3 * scale;
        let font = make_font((1000.0 * target / trial_ink).round().max(4.0) as i32);
        SelectObject(dc, font);
        DeleteObject(trial);
        let gm = measure(dc);
        let x = px(FIVE.0) + ((FIVE.2 * scale) as i32 - gm.gmBlackBoxX as i32) / 2 - gm.gmptGlyphOrigin.x;
        let baseline = py(FIVE.1) + gm.gmptGlyphOrigin.y;
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, rgb(ACCENT));
        SetTextAlign(dc, TA_BASELINE | TA_LEFT);
        let five = wide("5");
        TextOutW(dc, x, baseline, five.as_ptr(), 1);
        SelectObject(dc, old_font);
        DeleteObject(font);

        // Status dot, bottom right, ringed in the tile colour so it reads at 16 px.
        let dot = match level {
            Level::Normal => None,
            Level::Elevated => Some(WARNING),
            Level::Critical => Some(ERROR),
        };
        if let Some(colour) = dot {
            let d = (nf * 0.46) as i32;
            let ring = ((nf * 0.05) as i32).max(1);
            let brush = CreateSolidBrush(rgb(colour));
            let pen = CreatePen(PS_SOLID, ring, rgb(BG));
            let old_brush = SelectObject(dc, brush);
            let old_pen = SelectObject(dc, pen);
            Ellipse(dc, ni - d, ni - d, ni, ni);
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            DeleteObject(pen);
            DeleteObject(brush);
        }

        GdiFlush();
        let src = std::slice::from_raw_parts(bits as *const u8, n * n * 4);
        for y in 0..size {
            for x in 0..size {
                let (mut b, mut g, mut r, mut covered) = (0u32, 0u32, 0u32, 0u32);
                for sy in 0..SUPERSAMPLE {
                    for sx in 0..SUPERSAMPLE {
                        let i = ((y * SUPERSAMPLE + sy) * n + x * SUPERSAMPLE + sx) * 4;
                        let (pb, pg, pr) = (src[i], src[i + 1], src[i + 2]);
                        if (pr, pg, pb) != KEY {
                            b += pb as u32;
                            g += pg as u32;
                            r += pr as u32;
                            covered += 1;
                        }
                    }
                }
                if covered > 0 {
                    let o = (y * size + x) * 4;
                    out[o] = (b / covered) as u8;
                    out[o + 1] = (g / covered) as u8;
                    out[o + 2] = (r / covered) as u8;
                    out[o + 3] = (covered * 255 / (SUPERSAMPLE * SUPERSAMPLE) as u32) as u8;
                }
            }
        }
        SelectObject(dc, old_bitmap);
        DeleteObject(dib);
        DeleteDC(dc);
    }
    out
}

/// Opacity of the mark while Ather MemGuard is paused (docs/UI.md section 3).
pub const PAUSED_OPACITY: f32 = 0.45;

/// The paused look: the mark at 45% opacity; the status dot (where `level` has one) stays
/// fully opaque so pressure still reads.
pub fn render_paused(size: usize, level: Level) -> Vec<u8> {
    let plain = render(size, Level::Normal);
    let dotted = if level == Level::Normal { plain.clone() } else { render(size, level) };
    paused_pixels(&plain, &dotted)
}

/// Pure part of `render_paused`: pixels that differ between the plain and the dotted mark
/// belong to the dot and are kept; everything else is faded.
fn paused_pixels(plain: &[u8], dotted: &[u8]) -> Vec<u8> {
    let mut out = plain.to_vec();
    for (i, px) in out.chunks_exact_mut(4).enumerate() {
        let d = &dotted[i * 4..i * 4 + 4];
        if d != &plain[i * 4..i * 4 + 4] {
            px.copy_from_slice(d);
        } else {
            px[3] = (px[3] as f32 * PAUSED_OPACITY).round() as u8;
        }
    }
    out
}

/// An icon handle for the tray. The caller destroys it with DestroyIcon.
pub fn create_icon(size: usize, level: Level) -> HICON {
    icon_from_pixels(size, &render(size, level))
}

/// The tray icon with the paused look (`render_paused`).
pub fn create_paused_icon(size: usize, level: Level) -> HICON {
    icon_from_pixels(size, &render_paused(size, level))
}

fn icon_from_pixels(size: usize, pixels: &[u8]) -> HICON {
    unsafe {
        let colour = CreateBitmap(size as i32, size as i32, 1, 32, pixels.as_ptr() as *const _);
        let mask_bits = vec![0u8; size.div_ceil(16) * 2 * size];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask_bits.as_ptr() as *const _);
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: colour };
        let icon = CreateIconIndirect(&info);
        DeleteObject(colour);
        DeleteObject(mask);
        icon
    }
}

/// Write a multi-size .ico (the exe icon). Used once at build time: `memguard write-icon res\app.ico`.
pub fn write_ico(path: &std::path::Path) -> std::io::Result<()> {
    const SIZES: [usize; 8] = [16, 20, 24, 32, 40, 48, 64, 128];
    let mut images = Vec::new();
    for &size in &SIZES {
        let pixels = render(size, Level::Normal);
        let mut img = Vec::with_capacity(40 + size * size * 4 + size * size / 8);
        img.extend_from_slice(&40u32.to_le_bytes());
        img.extend_from_slice(&(size as i32).to_le_bytes());
        img.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // colour + mask
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        img.extend_from_slice(&[0u8; 24]);
        for y in (0..size).rev() {
            img.extend_from_slice(&pixels[y * size * 4..(y + 1) * size * 4]);
        }
        img.extend(std::iter::repeat(0u8).take(size.div_ceil(32) * 4 * size)); // AND mask: alpha decides
        images.push(img);
    }
    let mut file = Vec::new();
    file.extend_from_slice(&[0, 0, 1, 0]);
    file.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len();
    for (i, &size) in SIZES.iter().enumerate() {
        file.extend_from_slice(&[size as u8, size as u8, 0, 0]);
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&(images[i].len() as u32).to_le_bytes());
        file.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += images[i].len();
    }
    for img in images {
        file.extend_from_slice(&img);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_fades_the_mark_and_keeps_the_dot() {
        let plain = vec![10, 20, 30, 200, 1, 2, 3, 0];
        let dotted = vec![10, 20, 30, 200, 9, 9, 9, 255];
        let out = paused_pixels(&plain, &dotted);
        assert_eq!(&out[..4], &[10, 20, 30, 90]);
        assert_eq!(&out[4..], &[9, 9, 9, 255]);
    }
}

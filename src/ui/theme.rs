//! The Ather look: warm near-black, off-white type, one lime accent. Dark only.
//! Colours and font roles from docs/UI.md "Theme"; sizes are 96-DPI pixels and every
//! surface scales them with its window's DPI (see `Scale`).

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// GDI COLORREF (0x00BBGGRR).
    pub fn colorref(self) -> u32 {
        self.0 as u32 | (self.1 as u32) << 8 | (self.2 as u32) << 16
    }
    /// GDI+ ARGB with the given alpha.
    pub fn argb(self, alpha: u8) -> u32 {
        (alpha as u32) << 24 | (self.0 as u32) << 16 | (self.1 as u32) << 8 | self.2 as u32
    }
    /// `self` drawn at `opacity` (0..1) over `under`.
    pub fn over(self, under: Rgb, opacity: f32) -> Rgb {
        let t = opacity.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 * t + b as f32 * (1.0 - t)).round() as u8;
        Rgb(mix(self.0, under.0), mix(self.1, under.1), mix(self.2, under.2))
    }
}

pub const BG: Rgb = Rgb(14, 14, 13);
pub const SURFACE: Rgb = Rgb(26, 27, 25);
pub const RAISED: Rgb = Rgb(37, 38, 35);
pub const SELECTED: Rgb = Rgb(42, 45, 33);
pub const BORDER: Rgb = Rgb(50, 52, 46);
/// Row separators inside sections (mockup `#23241f`).
pub const HAIRLINE: Rgb = Rgb(35, 36, 31);
pub const TEXT: Rgb = Rgb(242, 239, 232);
pub const TEXT_DIM: Rgb = Rgb(208, 206, 199);
pub const MUTED: Rgb = Rgb(150, 150, 142);
pub const ACCENT: Rgb = Rgb(212, 255, 0);
pub const ACCENT_SOFT: Rgb = Rgb(226, 255, 110);
pub const ON_ACCENT: Rgb = Rgb(14, 14, 13);
pub const WARN: Rgb = Rgb(255, 184, 48);
pub const ERROR: Rgb = Rgb(255, 120, 110);

/// Control corner radius.
pub const RADIUS: f32 = 6.0;
/// Card and panel corner radius.
pub const CARD_RADIUS: f32 = 8.0;

/// Which typeface a piece of text uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    /// "Segoe UI Variable Text", falling back to "Segoe UI".
    Body,
    /// "Bahnschrift SemiBold Condensed" for UPPERCASE headlines.
    Display,
    /// "Bahnschrift SemiBold" for letter-spaced eyebrows and section headers.
    Eyebrow,
    /// "Segoe Fluent Icons" glyphs.
    Icon,
}

/// A font role: face, 96-DPI pixel size and weight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Font {
    pub face: Face,
    pub px: f32,
    pub weight: u16,
}

impl Font {
    pub const fn new(face: Face, px: f32, weight: u16) -> Font {
        Font { face, px, weight }
    }
}

pub const SEMIBOLD: u16 = 600;
pub const REGULAR: u16 = 400;

// The roles every surface uses, named after where they appear in the mockup.
pub const F_BODY: Font = Font::new(Face::Body, 14.0, REGULAR);
pub const F_LABEL: Font = Font::new(Face::Body, 14.0, SEMIBOLD);
pub const F_HINT: Font = Font::new(Face::Body, 12.0, REGULAR);
pub const F_VALUE: Font = Font::new(Face::Body, 13.0, REGULAR);
pub const F_BUTTON: Font = Font::new(Face::Body, 13.0, SEMIBOLD);
pub const F_SMALL_BUTTON: Font = Font::new(Face::Body, 12.0, SEMIBOLD);
pub const F_SEARCH: Font = Font::new(Face::Body, 17.0, REGULAR);
pub const F_GROUP: Font = Font::new(Face::Body, 15.0, SEMIBOLD);
pub const F_READ_SMALL: Font = Font::new(Face::Body, 11.0, REGULAR);
pub const F_CHIP: Font = Font::new(Face::Body, 12.0, REGULAR);
pub const F_HEADLINE: Font = Font::new(Face::Display, 44.0, REGULAR);
pub const F_STRIP: Font = Font::new(Face::Display, 20.0, REGULAR);
pub const F_TOAST_TITLE: Font = Font::new(Face::Display, 17.0, REGULAR);
pub const F_DIALOG_TITLE: Font = Font::new(Face::Display, 24.0, REGULAR);
pub const F_EYEBROW: Font = Font::new(Face::Eyebrow, 11.0, REGULAR);
pub const F_SECTION: Font = Font::new(Face::Eyebrow, 12.0, REGULAR);
pub const F_CAPTION: Font = Font::new(Face::Eyebrow, 11.0, REGULAR);
pub const F_TAG: Font = Font::new(Face::Eyebrow, 10.0, REGULAR);
pub const F_ICON: Font = Font::new(Face::Icon, 12.0, REGULAR);
pub const F_ICON_SEARCH: Font = Font::new(Face::Icon, 17.0, REGULAR);

/// Segoe Fluent Icons code points used by the UI.
pub mod glyph {
    pub const SEARCH: char = '\u{E721}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const CHEVRON_RIGHT: char = '\u{E76C}';
    pub const CLOSE: char = '\u{E711}';
    pub const ADD: char = '\u{E710}';
}

/// The DPI scale of a surface: `s(n)` turns a 96-DPI length into device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale(pub f32);

impl Scale {
    pub fn from_dpi(dpi: u32) -> Scale {
        Scale(if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 })
    }
    /// `S(n)`: device pixels for a 96-DPI length, rounded.
    pub fn s(self, v: f32) -> i32 {
        (v * self.0).round() as i32
    }
    /// Like `s`, never below one pixel (hairlines, rings).
    pub fn px1(self, v: f32) -> i32 {
        self.s(v).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_conversions() {
        assert_eq!(ACCENT.colorref(), 0x0000FFD4);
        assert_eq!(ACCENT.argb(255), 0xFFD4FF00);
        assert_eq!(TEXT.over(BG, 1.0), TEXT);
        assert_eq!(TEXT.over(BG, 0.0), BG);
        assert_eq!(Rgb(200, 100, 0).over(Rgb(0, 0, 0), 0.5), Rgb(100, 50, 0));
    }

    #[test]
    fn scale_rounds() {
        assert_eq!(Scale(1.0).s(66.0), 66);
        assert_eq!(Scale(1.5).s(66.0), 99);
        assert_eq!(Scale(1.25).s(3.0), 4);
        assert_eq!(Scale(1.0).px1(0.4), 1);
        assert_eq!(Scale::from_dpi(144), Scale(1.5));
    }
}

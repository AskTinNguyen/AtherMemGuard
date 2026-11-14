//! Integer device-pixel geometry shared by layout, painting and hit testing.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

impl Pt {
    pub const fn new(x: i32, y: i32) -> Pt {
        Pt { x, y }
    }
}

/// A rectangle: left, top, width, height. Empty when `w <= 0 || h <= 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn ltrb(l: i32, t: i32, r: i32, b: i32) -> Rect {
        Rect { x: l, y: t, w: r - l, h: b - t }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn cx(&self) -> i32 {
        self.x + self.w / 2
    }
    pub fn cy(&self) -> i32 {
        self.y + self.h / 2
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn contains(&self, p: Pt) -> bool {
        !self.is_empty() && p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
    /// True when the two rectangles share at least one pixel.
    pub fn intersects(&self, o: &Rect) -> bool {
        !self.is_empty() && !o.is_empty() && self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    /// True when `o` lies completely inside `self`.
    pub fn encloses(&self, o: &Rect) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }
    pub fn inset(&self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, w: self.w - 2 * dx, h: self.h - 2 * dy }
    }
    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..*self }
    }
    /// A `w` x `h` rectangle centred vertically at the right edge of `self`.
    pub fn right_part(&self, w: i32, h: i32) -> Rect {
        Rect { x: self.right() - w, y: self.cy() - h / 2, w, h }
    }
    /// A `w` x `h` rectangle centred in `self`.
    pub fn centered(&self, w: i32, h: i32) -> Rect {
        Rect { x: self.cx() - w / 2, y: self.cy() - h / 2, w, h }
    }
    pub fn to_win(self) -> windows_sys::Win32::Foundation::RECT {
        windows_sys::Win32::Foundation::RECT { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_basics() {
        let r = Rect::new(10, 20, 100, 50);
        assert_eq!(r.right(), 110);
        assert_eq!(r.bottom(), 70);
        assert!(r.contains(Pt::new(10, 20)));
        assert!(!r.contains(Pt::new(110, 20)));
        assert!(r.intersects(&Rect::new(109, 69, 5, 5)));
        assert!(!r.intersects(&Rect::new(110, 20, 5, 5)));
        assert!(r.encloses(&r.inset(2, 2)));
        assert_eq!(r.right_part(20, 10), Rect::new(90, 40, 20, 10));
        assert_eq!(Rect::ltrb(1, 2, 4, 6), Rect::new(1, 2, 3, 4));
    }
}

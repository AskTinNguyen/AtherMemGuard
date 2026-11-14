//! `ui-state.toml` in the data folder: remembered window sizes and positions.
//!
//! ```toml
//! [settings]
//! x = 120
//! y = 80
//! w = 900
//! h = 760
//! ```
//! Device pixels of the window's normal (restored) rectangle. Written atomically; never
//! anywhere but `paths::data_dir()`.

use std::path::PathBuf;

use super::geom::Rect;

pub fn file() -> PathBuf {
    crate::paths::data_dir().join("ui-state.toml")
}

/// The rectangle stored for `kind` in `text`.
pub fn parse(text: &str, kind: &str) -> Option<Rect> {
    let doc: toml::Table = text.parse().ok()?;
    let t = doc.get(kind)?.as_table()?;
    let n = |k: &str| t.get(k).and_then(|v| v.as_integer()).map(|v| v as i32);
    Some(Rect::new(n("x")?, n("y")?, n("w")?, n("h")?))
}

/// `text` with the rectangle for `kind` set (other kinds kept).
pub fn with_rect(text: &str, kind: &str, r: Rect) -> String {
    let mut doc: toml::Table = text.parse().unwrap_or_default();
    let mut t = toml::Table::new();
    t.insert("x".into(), toml::Value::Integer(r.x as i64));
    t.insert("y".into(), toml::Value::Integer(r.y as i64));
    t.insert("w".into(), toml::Value::Integer(r.w as i64));
    t.insert("h".into(), toml::Value::Integer(r.h as i64));
    doc.insert(kind.to_string(), toml::Value::Table(t));
    toml::to_string(&doc).unwrap_or_default()
}

pub fn load(kind: &str) -> Option<Rect> {
    parse(&std::fs::read_to_string(file()).ok()?, kind)
}

pub fn save(kind: &str, r: Rect) -> Result<(), String> {
    let path = file();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let text = with_rect(&old, kind, r);
    std::fs::create_dir_all(crate::paths::data_dir()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_other_windows() {
        let text = with_rect("", "settings", Rect::new(10, 20, 900, 760));
        let text = with_rect(&text, "history", Rect::new(-1500, 5, 960, 600));
        assert_eq!(parse(&text, "settings"), Some(Rect::new(10, 20, 900, 760)));
        assert_eq!(parse(&text, "history"), Some(Rect::new(-1500, 5, 960, 600)));
        assert_eq!(parse(&text, "setup"), None);
        assert_eq!(parse("not toml [", "settings"), None);
    }
}

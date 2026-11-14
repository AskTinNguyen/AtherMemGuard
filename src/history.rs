//! Memory history: an in-memory ring of the last 24 hours for the chart and the editor
//! growth alert, and one CSV file per day in `history\` in the data folder.
//!
//! CSV columns: time, mode, pressure, ram_free_gb, commit_free_gb, ue_committed_gb,
//! editor_committed_gb, workers, then one RAM column (GB) per app group, named after the
//! group. When the set of groups changes during a day, a new header line is written and
//! the rows after it follow that header.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::engine::{Level, Status};

const DAY_SECS: i64 = 86_400;
/// Window for the editor growth slope.
const GROWTH_WINDOW_SECS: i64 = 30 * 60;
/// Less history than this for an editor and there is no slope.
const GROWTH_MIN_SPAN_SECS: i64 = 10 * 60;
const FIXED_COLUMNS: [&str; 8] =
    ["time", "mode", "pressure", "ram_free_gb", "commit_free_gb", "ue_committed_gb", "editor_committed_gb", "workers"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupRam {
    pub name: String,
    pub ram_gb: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    /// Seconds since 1970-01-01 UTC.
    pub unix: i64,
    /// Local time, `YYYY-MM-DD HH:MM:SS`.
    pub time: String,
    pub mode: String,
    pub pressure: Level,
    pub ram_free_gb: f64,
    pub commit_free_gb: f64,
    pub ue_committed_gb: f64,
    pub editor_committed_gb: f64,
    pub workers: usize,
    pub groups: Vec<GroupRam>,
    /// Committed GB per editor process (pid, GB). In memory only, not in the CSV.
    #[serde(skip)]
    pub editors: Vec<(u32, f64)>,
}

impl Sample {
    pub fn from_status(st: &Status, unix: i64) -> Sample {
        let editors: Vec<(u32, f64)> =
            st.ue.processes.iter().filter(|p| p.kind == "editor").map(|p| (p.pid, p.committed_gb)).collect();
        Sample {
            unix,
            time: st.time.clone(),
            mode: st.mode.clone(),
            pressure: st.pressure,
            ram_free_gb: st.system.ram_free_gb,
            commit_free_gb: st.system.commit_free_gb,
            ue_committed_gb: st.ue.total_committed_gb,
            editor_committed_gb: round2(editors.iter().map(|e| e.1).sum()),
            workers: st.ue.workers,
            groups: st.groups.iter().map(|g| GroupRam { name: g.name.clone(), ram_gb: round2(g.ram_mb as f64 / 1024.0) }).collect(),
            editors,
        }
    }

    fn date(&self) -> &str {
        self.time.get(..10).unwrap_or("unknown")
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0 + 0.0 // no "-0.00"
}

pub struct History {
    /// `None` = in memory only.
    dir: Option<PathBuf>,
    keep_days: u32,
    ring: VecDeque<Sample>,
    /// The file and group columns the last header we know of was for.
    header: Option<(String, Vec<String>)>,
    last_cleanup: Option<String>,
}

/// `history\` in the data folder.
pub fn history_dir() -> PathBuf {
    crate::paths::data_dir().join("history")
}

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl History {
    /// A history that writes CSV files to `dir`. Starts empty.
    pub fn new(dir: PathBuf, keep_days: u32) -> History {
        History { dir: Some(dir), keep_days: keep_days.max(1), ring: VecDeque::new(), header: None, last_cleanup: None }
    }

    /// A history that keeps samples in memory only.
    pub fn in_memory() -> History {
        History { dir: None, keep_days: 1, ring: VecDeque::new(), header: None, last_cleanup: None }
    }

    /// The history in the data folder, with the last 24 hours read back from disk.
    pub fn open_default(keep_days: u32) -> History {
        let mut h = History::new(history_dir(), keep_days);
        for s in load_since_from(&history_dir(), Duration::from_secs(DAY_SECS as u64), now_unix(), local_offset_secs()) {
            h.ring.push_back(s);
        }
        h
    }

    pub fn set_keep_days(&mut self, days: u32) {
        self.keep_days = days.max(1);
    }

    /// Add the status of one tick: to the ring and to today's CSV file.
    pub fn record(&mut self, st: &Status) {
        self.push(Sample::from_status(st, now_unix()));
    }

    /// Add the status of one tick to the ring only (history files turned off). The editor
    /// growth alert still works.
    pub fn record_memory(&mut self, st: &Status) {
        self.push_inner(Sample::from_status(st, now_unix()), false);
    }

    /// Add one sample: to the ring and, unless in memory only, to its day's CSV file.
    pub fn push(&mut self, s: Sample) {
        self.push_inner(s, true);
    }

    fn push_inner(&mut self, s: Sample, write: bool) {
        if let Some(dir) = self.dir.clone().filter(|_| write) {
            if let Err(e) = self.append_csv(&dir, &s) {
                crate::log::write(&format!("history: {}", e));
            }
            let today = s.date().to_string();
            if self.last_cleanup.as_deref() != Some(today.as_str()) {
                self.last_cleanup = Some(today.clone());
                cleanup(&dir, &today, self.keep_days);
            }
        }
        let cutoff = s.unix - DAY_SECS;
        self.ring.push_back(s);
        while self.ring.front().is_some_and(|f| f.unix < cutoff) {
            self.ring.pop_front();
        }
    }

    fn append_csv(&mut self, dir: &Path, s: &Sample) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        let path = dir.join(format!("{}.csv", s.date()));
        let groups: Vec<String> = s.groups.iter().map(|g| g.name.clone()).collect();
        let key = (s.date().to_string(), groups.clone());
        let mut text = String::new();
        if self.header.as_ref() != Some(&key) {
            // After a restart the file may already end with the right header.
            if last_header_groups(&path).as_ref() != Some(&groups) {
                text.push_str(&header_line(&groups));
            }
            self.header = Some(key);
        }
        text.push_str(&row_line(s));
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// Samples from the last `since` (at most 24 hours: the ring), oldest first.
    pub fn series(&self, since: Duration) -> Vec<Sample> {
        self.series_at(now_unix(), since)
    }

    fn series_at(&self, now: i64, since: Duration) -> Vec<Sample> {
        let from = now - since.as_secs() as i64;
        self.ring.iter().filter(|s| s.unix >= from).cloned().collect()
    }

    /// The most recent sample.
    pub fn latest(&self) -> Option<&Sample> {
        self.ring.back()
    }

    /// Every sample in one day's file. `date` is `YYYY-MM-DD` (local date).
    pub fn load_day(&self, date: &str) -> Result<Vec<Sample>, String> {
        let dir = self.dir.clone().unwrap_or_else(history_dir);
        load_day_from(&dir, date, local_offset_secs())
    }

    /// Least-squares slope of this editor's committed memory over the last 30 minutes, in GB
    /// per hour. `None` with under 10 minutes of samples for it.
    pub fn editor_growth_gb_per_hour(&self, pid: u32) -> Option<f64> {
        let end = self.ring.back()?.unix;
        let points: Vec<(f64, f64)> = self
            .ring
            .iter()
            .filter(|s| s.unix >= end - GROWTH_WINDOW_SECS)
            .filter_map(|s| s.editors.iter().find(|e| e.0 == pid).map(|e| (s.unix as f64, e.1)))
            .collect();
        let (first, last) = (points.first()?.0, points.last()?.0);
        if last - first < GROWTH_MIN_SPAN_SECS as f64 {
            return None;
        }
        slope_per_hour(&points)
    }
}

/// Least-squares slope of y over x (seconds), per hour.
fn slope_per_hour(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let x0 = points[0].0;
    let mx = points.iter().map(|p| p.0 - x0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for p in points {
        let dx = p.0 - x0 - mx;
        sxy += dx * (p.1 - my);
        sxx += dx * dx;
    }
    if sxx == 0.0 {
        return None;
    }
    Some(sxy / sxx * 3600.0)
}

// ---------------------------------------------------------------- CSV

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn header_line(groups: &[String]) -> String {
    let mut cols: Vec<String> = FIXED_COLUMNS.iter().map(|s| s.to_string()).collect();
    cols.extend(groups.iter().map(|g| csv_field(g)));
    cols.join(",") + "\n"
}

fn row_line(s: &Sample) -> String {
    let mut cols = vec![
        csv_field(&s.time),
        csv_field(&s.mode),
        s.pressure.label().to_string(),
        format!("{:.2}", s.ram_free_gb),
        format!("{:.2}", s.commit_free_gb),
        format!("{:.2}", s.ue_committed_gb),
        format!("{:.2}", s.editor_committed_gb),
        s.workers.to_string(),
    ];
    cols.extend(s.groups.iter().map(|g| format!("{:.2}", g.ram_gb)));
    cols.join(",") + "\n"
}

fn is_header(line: &str) -> bool {
    line.starts_with("time,")
}

fn last_header_groups(path: &Path) -> Option<Vec<String>> {
    let text = fs::read_to_string(path).ok()?;
    let line = text.lines().rev().find(|l| is_header(l))?;
    Some(split_csv(line).into_iter().skip(FIXED_COLUMNS.len()).collect())
}

fn parse_level(s: &str) -> Option<Level> {
    match s {
        "normal" => Some(Level::Normal),
        "elevated" => Some(Level::Elevated),
        "critical" => Some(Level::Critical),
        _ => None,
    }
}

/// Parse one day's CSV text. `offset` is local time minus UTC, in seconds.
fn parse_csv(text: &str, offset: i64) -> Vec<Sample> {
    let mut groups: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty()) {
        let f = split_csv(line);
        if is_header(line) {
            groups = f.into_iter().skip(FIXED_COLUMNS.len()).collect();
            continue;
        }
        if f.len() < FIXED_COLUMNS.len() {
            continue;
        }
        let num = |i: usize| f[i].parse::<f64>().ok();
        let (Some(local), Some(pressure), Some(ram), Some(commit), Some(ue), Some(ed), Ok(workers)) = (
            parse_local_secs(&f[0]),
            parse_level(&f[2]),
            num(3),
            num(4),
            num(5),
            num(6),
            f[7].parse::<usize>(),
        ) else {
            continue;
        };
        let group_ram = groups
            .iter()
            .zip(f.iter().skip(FIXED_COLUMNS.len()))
            .filter_map(|(name, v)| v.parse::<f64>().ok().map(|ram_gb| GroupRam { name: name.clone(), ram_gb }))
            .collect();
        out.push(Sample {
            unix: local - offset,
            time: f[0].clone(),
            mode: f[1].clone(),
            pressure,
            ram_free_gb: ram,
            commit_free_gb: commit,
            ue_committed_gb: ue,
            editor_committed_gb: ed,
            workers,
            groups: group_ram,
            editors: Vec::new(),
        });
    }
    out
}

fn load_day_from(dir: &Path, date: &str, offset: i64) -> Result<Vec<Sample>, String> {
    if parse_date(date).is_none() {
        return Err(format!("\"{}\" is not a date like 2026-11-14", date));
    }
    let path = dir.join(format!("{}.csv", date));
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse_csv(&text, offset)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {}", path.display(), e)),
    }
}

fn load_since_from(dir: &Path, since: Duration, now: i64, offset: i64) -> Vec<Sample> {
    let from = now - since.as_secs() as i64;
    let first_day = (from + offset).div_euclid(DAY_SECS);
    let last_day = (now + offset).div_euclid(DAY_SECS);
    let mut out = Vec::new();
    for day in first_day..=last_day {
        if let Ok(samples) = load_day_from(dir, &date_string(day), offset) {
            out.extend(samples.into_iter().filter(|s| s.unix >= from && s.unix <= now));
        }
    }
    out.sort_by_key(|s| s.unix);
    out
}

/// Samples from the last `since`, read from the CSV files in the data folder (any span the
/// files cover). Used by `memguard history`, which runs without the engine.
pub fn load_since(since: Duration) -> Vec<Sample> {
    load_since_from(&history_dir(), since, now_unix(), local_offset_secs())
}

/// Delete day files that are `keep_days` days old or older (today's file is age 0).
fn cleanup(dir: &Path, today: &str, keep_days: u32) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let names: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    for name in expired_files(&names, today, keep_days) {
        if fs::remove_file(dir.join(&name)).is_ok() {
            crate::log::write(&format!("history: deleted {} (older than {} days)", name, keep_days));
        }
    }
}

fn expired_files(names: &[String], today: &str, keep_days: u32) -> Vec<String> {
    let Some(today) = parse_date(today) else {
        return Vec::new();
    };
    names
        .iter()
        .filter(|n| {
            n.strip_suffix(".csv").and_then(parse_date).is_some_and(|d| today - d >= keep_days.max(1) as i64)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------- dates without a crate

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn date_string(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// `YYYY-MM-DD` -> days since 1970-01-01.
fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// `YYYY-MM-DD HH:MM:SS` read as if it were UTC, in seconds since 1970.
fn parse_local_secs(s: &str) -> Option<i64> {
    let day = parse_date(s.get(..10)?)?;
    let t = s.get(11..19)?;
    let parts: Vec<i64> = t.split(':').map(|p| p.parse().ok()).collect::<Option<Vec<_>>>()?;
    if parts.len() != 3 {
        return None;
    }
    Some(day * DAY_SECS + parts[0] * 3600 + parts[1] * 60 + parts[2])
}

/// Local time minus UTC, in seconds, rounded to the minute.
fn local_offset_secs() -> i64 {
    match parse_local_secs(&crate::win::local_time_string()) {
        Some(local) => ((local - now_unix()) as f64 / 60.0).round() as i64 * 60,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample(unix: i64, editors: &[(u32, f64)]) -> Sample {
        let day = unix.div_euclid(DAY_SECS);
        let secs = unix.rem_euclid(DAY_SECS);
        Sample {
            unix,
            time: format!("{} {:02}:{:02}:{:02}", date_string(day), secs / 3600, secs / 60 % 60, secs % 60),
            mode: "editor+build".into(),
            pressure: Level::Elevated,
            ram_free_gb: 12.5,
            commit_free_gb: 30.25,
            ue_committed_gb: 40.0,
            editor_committed_gb: editors.iter().map(|e| e.1).sum::<f64>() + 0.0,
            workers: 3,
            groups: vec![
                GroupRam { name: "Browsers".into(), ram_gb: 2.5 },
                GroupRam { name: "Odd, \"name\"".into(), ram_gb: 0.75 },
            ],
            editors: editors.to_vec(),
        }
    }

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("memguard-hist-test-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&d);
        d
    }

    const T0: i64 = 1_794_000_000; // 2026-11-07

    #[test]
    fn dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(date_string(days_from_civil(2024, 2, 29)), "2024-02-29");
        assert_eq!(parse_date("2026-11-14").map(date_string).as_deref(), Some("2026-11-14"));
        assert_eq!(parse_local_secs("1970-01-02 01:00:05"), Some(DAY_SECS + 3605));
        assert!(parse_date("2026-13-01").is_none());
    }

    #[test]
    fn growth_slope_on_synthetic_series() {
        let mut h = History::in_memory();
        // Editor 7 grows 6 GB/h, editor 9 is flat, sampled every 10 s for 40 minutes.
        for i in 0..=240 {
            let t = T0 + i * 10;
            let hours = (i * 10) as f64 / 3600.0;
            h.push(sample(t, &[(7, 20.0 + 6.0 * hours), (9, 30.0)]));
        }
        let g = h.editor_growth_gb_per_hour(7).unwrap();
        assert!((g - 6.0).abs() < 1e-6, "{}", g);
        assert!(h.editor_growth_gb_per_hour(9).unwrap().abs() < 1e-9);
        assert!(h.editor_growth_gb_per_hour(1234).is_none());
    }

    #[test]
    fn growth_needs_ten_minutes() {
        let mut h = History::in_memory();
        for i in 0..=59 {
            h.push(sample(T0 + i * 10, &[(7, 20.0 + i as f64)]));
        }
        assert!(h.editor_growth_gb_per_hour(7).is_none()); // 590 s
        h.push(sample(T0 + 600, &[(7, 80.0)]));
        assert!(h.editor_growth_gb_per_hour(7).is_some());
        // Only the last 30 minutes count: an old ramp followed by a flat hour is flat.
        let mut h = History::in_memory();
        for i in 0..360 {
            let gb = if i < 180 { i as f64 } else { 180.0 };
            h.push(sample(T0 + i * 20, &[(7, gb)]));
        }
        assert!(h.editor_growth_gb_per_hour(7).unwrap().abs() < 1e-9);
    }

    #[test]
    fn noisy_slope_is_close() {
        let pts: Vec<(f64, f64)> =
            (0..100).map(|i| (i as f64 * 18.0, 10.0 + 2.0 * i as f64 * 18.0 / 3600.0 + if i % 2 == 0 { 0.05 } else { -0.05 })).collect();
        assert!((slope_per_hour(&pts).unwrap() - 2.0).abs() < 0.05);
    }

    #[test]
    fn ring_keeps_24_hours_and_series_filters() {
        let mut h = History::in_memory();
        for i in 0..=(26 * 6) {
            h.push(sample(T0 + i * 600, &[]));
        }
        let now = T0 + 26 * 3600;
        assert!(h.ring.front().unwrap().unix >= now - DAY_SECS);
        assert_eq!(h.series_at(now, Duration::from_secs(3600)).len(), 7);
        assert_eq!(h.latest().unwrap().unix, now);
    }

    #[test]
    fn csv_round_trip_and_header_changes() {
        let dir = temp_dir();
        let mut h = History::new(dir.clone(), 7);
        let a = sample(T0, &[(7, 20.0)]);
        let mut b = sample(T0 + 10, &[]);
        b.groups.push(GroupRam { name: "Chat".into(), ram_gb: 1.0 });
        h.push(a.clone());
        h.push(b.clone());
        // A restart with the same groups doesn't repeat the header.
        let mut h2 = History::new(dir.clone(), 7);
        h2.push(sample(T0 + 20, &[]));
        let date = a.date().to_string();
        let text = fs::read_to_string(dir.join(format!("{}.csv", date))).unwrap();
        assert_eq!(text.lines().filter(|l| is_header(l)).count(), 3, "{}", text);
        assert!(!text.contains("-0.00"));
        assert!(text.starts_with("time,mode,pressure,ram_free_gb,commit_free_gb,ue_committed_gb,editor_committed_gb,workers,Browsers,\"Odd, \"\"name\"\"\"\n"));

        let loaded = load_day_from(&dir, &date, 0).unwrap();
        assert_eq!(loaded.len(), 3);
        let mut a2 = a.clone();
        a2.editors.clear();
        assert_eq!(loaded[0], a2);
        assert_eq!(loaded[1].groups.len(), 3);
        assert_eq!(loaded[1].groups[2], GroupRam { name: "Chat".into(), ram_gb: 1.0 });
        assert_eq!(loaded[2].groups.len(), 2);

        // A local offset shifts unix time, not the local time text.
        let shifted = load_day_from(&dir, &date, 3600).unwrap();
        assert_eq!(shifted[0].unix, T0 - 3600);
        assert_eq!(load_since_from(&dir, Duration::from_secs(15), T0 + 20, 0).len(), 2);
        assert!(load_day_from(&dir, "nope", 0).is_err());
        assert!(load_day_from(&dir, "1999-01-01", 0).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention() {
        let names: Vec<String> =
            ["2026-11-14.csv", "2026-11-08.csv", "2026-11-07.csv", "2026-10-01.csv", "notes.txt", "2026-11-01.csv.bak"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(expired_files(&names, "2026-11-14", 7), ["2026-11-07.csv", "2026-10-01.csv"]);
        assert_eq!(expired_files(&names, "2026-11-14", 1), ["2026-11-08.csv", "2026-11-07.csv", "2026-10-01.csv"]);

        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        for n in &names {
            fs::write(dir.join(n), "").unwrap();
        }
        let mut h = History::new(dir.clone(), 7);
        let mut s = sample(T0, &[]);
        s.time = "2026-11-14 12:00:00".into();
        h.push(s);
        let mut left: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["2026-11-01.csv.bak", "2026-11-08.csv", "2026-11-14.csv", "notes.txt"]);
        let _ = fs::remove_dir_all(&dir);
    }
}

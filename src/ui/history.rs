//! The memory history window (docs/UI.md section 5): range pills, the chart (Unreal committed,
//! RAM free, commit free, pressure bands, memory-freed dots, editor-start markers, gridlines,
//! axis labels, hover crosshair with readout), legend, cards, footer and the empty state.
//!
//! Data comes through a `HistorySource`: `RealHistorySource` reads the daily CSV files
//! (`history::load_since`), `FakeHistory` produces the mockup's curve deterministically.
//! The chart maths (downsampling, marker inference, axis ticks, bands) are pure functions
//! below the window and are unit-tested.

use std::path::PathBuf;
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB};

use super::canvas::{Align, Canvas};
use super::geom::{Pt, Rect};
use super::kit::{focus_ring, Segmented, WState};
use super::setup::{paint_link, wrap_hard};
use super::theme::*;
use super::window::{self, Cursor, Event, Host, MouseButton, Surface, WindowSpec};
use crate::engine::Level;
use crate::history::{GroupRam, Sample};

pub const KIND: &str = "history";
pub const TITLE: &str = "Ather MemGuard — Memory history";
const TIMER_REFRESH: usize = 1;
/// Never more points than this on the chart.
pub const MAX_POINTS: usize = 1500;
const F_CHART_HEADLINE: Font = Font::new(Face::Display, 34.0, REGULAR);
const EMPTY_TEXT: &str = "No history yet. It starts filling in as soon as Ather MemGuard has been running for a minute.";

// ---------------------------------------------------------------- source

/// Where the window gets its samples. Called on the UI thread.
pub trait HistorySource {
    /// Samples from the last `since`, oldest first.
    fn samples(&self, since: Duration) -> Vec<Sample>;
    fn interval_sec(&self) -> u64;
    fn keep_days(&self) -> u32;
    fn ram_total_gb(&self) -> f64;
    /// The folder with the CSV files ("Open the CSV folder").
    fn folder(&self) -> PathBuf;
    /// Addition: the right edge of the chart ("now"). Fakes override it to stay stable.
    fn now_unix(&self) -> i64 {
        crate::history::now_unix()
    }
}

/// Reads the CSV files in the data folder. Settings come from the caller's `Config` so the
/// window never loads (or writes) the settings file itself.
pub struct RealHistorySource {
    pub interval_sec: u64,
    pub keep_days: u32,
}

impl RealHistorySource {
    pub fn from_config(cfg: &crate::config::Config) -> RealHistorySource {
        RealHistorySource { interval_sec: cfg.interval_sec, keep_days: cfg.history.keep_days }
    }
}

impl HistorySource for RealHistorySource {
    fn samples(&self, since: Duration) -> Vec<Sample> {
        crate::history::load_since(since)
    }
    fn interval_sec(&self) -> u64 {
        self.interval_sec
    }
    fn keep_days(&self) -> u32 {
        self.keep_days
    }
    fn ram_total_gb(&self) -> f64 {
        crate::win::sys_info().ram_total as f64 / (1024.0 * 1024.0 * 1024.0)
    }
    fn folder(&self) -> PathBuf {
        crate::history::history_dir()
    }
}

/// The mockup's day, repeated for seven days: an editor started at 08:30 that grows to
/// 61 GB, two builds, memory freed twice around 12:10 and 12:25, a short critical spell,
/// and an editor restart at 12:45. "Now" is 14:00 on 14 Nov 2026. Deterministic.
pub struct FakeHistory {
    pub empty: bool,
}

impl FakeHistory {
    pub fn mockup() -> FakeHistory {
        FakeHistory { empty: false }
    }
    pub fn empty() -> FakeHistory {
        FakeHistory { empty: true }
    }
    const NOW: i64 = 20_771 * 86_400 + 14 * 3600; // 2026-11-14 14:00 (offset 0)
    const TOTAL: f64 = 127.7;

    /// One sample at local hour `h` (0..24) of day `day` (0 = today, 1 = yesterday...).
    fn sample_at(unix: i64) -> Sample {
        let day = Self::NOW.div_euclid(86_400) - unix.div_euclid(86_400);
        let h = unix.rem_euclid(86_400) as f64 / 3600.0;
        let amp = [1.0, 0.86, 0.93, 0.8, 0.9, 0.84, 0.95][(day as usize) % 7];
        let wiggle = |k: f64| (unix as f64 / k).sin();
        // Editor: starts 08:30, grows, restarts at 12:45 (new process loads the project), quits 19:00.
        let (editor, pid) = if !(8.5..19.0).contains(&h) {
            (0.0, 0)
        } else if h < 12.75 {
            ((5.0 + 13.15 * (h - 8.5)) * amp + 0.3 * wiggle(97.0), 4100 + day as u32)
        } else {
            let load = ((h - 12.75) / 0.08).clamp(0.0, 1.0);
            ((3.0 + 38.0 * load + 2.1 * (h - 12.75)) * amp + 0.2 * wiggle(83.0), 5200 + day as u32)
        };
        // Compile workers during two builds.
        let workers_gb = if (10.5..11.25).contains(&h) {
            10.0
        } else if (12.0..12.7).contains(&h) {
            27.0 * (1.0 - ((h - 12.35) / 0.35).abs()).max(0.0) + 6.0
        } else {
            0.0
        };
        // Groups creep up through the morning and are cut back when memory is freed at 12:06
        // and again at 12:24, during the second build.
        let k = if h < 8.0 {
            1.0
        } else if h < 12.1 {
            1.0 + 0.08 * (h - 8.0)
        } else if h < 12.4 {
            0.45 + 1.5 * (h - 12.1)
        } else {
            0.5 + 0.5 * (h - 12.4) / 11.6 // back to 1.0 by midnight
        };
        let browsers = 6.0 * k + 0.05 * wiggle(61.0);
        let chat = 1.6 * k + 0.03 * wiggle(47.0);
        let agents = 9.3 * k + 0.08 * wiggle(71.0);
        let groups_sum = browsers + chat + agents;
        let ue = editor + workers_gb;
        let ram_free = (Self::TOTAL - 22.0 - groups_sum - ue).clamp(1.5, Self::TOTAL);
        let commit_free = (ram_free * 0.72 - 3.0 + 0.4 * wiggle(113.0)).max(0.5);
        let pressure = if ram_free < 5.1 {
            Level::Critical
        } else if ram_free < 12.8 {
            Level::Elevated
        } else {
            Level::Normal
        };
        Sample {
            unix,
            time: local_time_string(unix, 0),
            mode: if editor > 0.0 { "editor".into() } else { "idle".into() },
            pressure,
            ram_free_gb: ram_free,
            commit_free_gb: commit_free,
            ue_committed_gb: ue,
            editor_committed_gb: editor,
            workers: if workers_gb > 0.0 { 3 } else { 0 },
            groups: vec![
                GroupRam { name: "Browsers".into(), ram_gb: browsers },
                GroupRam { name: "Chat".into(), ram_gb: chat },
                GroupRam { name: "Agents".into(), ram_gb: agents },
            ],
            editors: if editor > 0.0 { vec![(pid, editor)] } else { vec![] },
        }
    }
}

impl HistorySource for FakeHistory {
    fn samples(&self, since: Duration) -> Vec<Sample> {
        if self.empty {
            return Vec::new();
        }
        let from = Self::NOW - since.as_secs() as i64;
        let mut out = Vec::new();
        let mut t = from - from.rem_euclid(60);
        while t <= Self::NOW {
            if t >= from {
                out.push(Self::sample_at(t));
            }
            // Every 10 seconds for the last day, every minute before (as if thinned).
            t += if Self::NOW - t <= 86_400 { 10 } else { 60 };
        }
        out
    }
    fn interval_sec(&self) -> u64 {
        10
    }
    fn keep_days(&self) -> u32 {
        7
    }
    fn ram_total_gb(&self) -> f64 {
        Self::TOTAL
    }
    fn folder(&self) -> PathBuf {
        std::env::temp_dir()
    }
    fn now_unix(&self) -> i64 {
        Self::NOW
    }
}

// ---------------------------------------------------------------- chart maths (pure)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    H1,
    H6,
    H24,
    D7,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::H1, Range::H6, Range::H24, Range::D7];
    pub fn secs(self) -> i64 {
        match self {
            Range::H1 => 3600,
            Range::H6 => 6 * 3600,
            Range::H24 => 86_400,
            Range::D7 => 7 * 86_400,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Range::H1 => "1 h",
            Range::H6 => "6 h",
            Range::H24 => "24 h",
            Range::D7 => "7 d",
        }
    }
}

/// One point of the three lines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartPoint {
    pub t: i64,
    pub ue: f64,
    pub ram: f64,
    pub commit: f64,
    pub level: Level,
}

impl ChartPoint {
    pub fn from_sample(s: &Sample) -> ChartPoint {
        ChartPoint { t: s.unix, ue: s.ue_committed_gb, ram: s.ram_free_gb, commit: s.commit_free_gb, level: s.pressure }
    }
}

fn level_rank(l: Level) -> u8 {
    match l {
        Level::Normal => 0,
        Level::Elevated => 1,
        Level::Critical => 2,
    }
}

/// At most `max` points. Each bucket keeps both its minimum and its maximum of every line (in
/// the order they happened), so a spike shorter than a pixel still shows; the bucket's
/// pressure is its worst.
pub fn downsample(pts: &[ChartPoint], max: usize) -> Vec<ChartPoint> {
    if pts.len() <= max || max < 4 {
        return pts.to_vec();
    }
    let n = pts.len();
    let buckets = max / 2;
    let mut out = Vec::with_capacity(buckets * 2);
    for b in 0..buckets {
        let s = &pts[b * n / buckets..(b + 1) * n / buckets];
        if s.is_empty() {
            continue;
        }
        if s.len() == 1 {
            out.push(s[0]);
            continue;
        }
        let level = s.iter().map(|p| p.level).max_by_key(|l| level_rank(*l)).unwrap_or(Level::Normal);
        // (first, second) in time order of the bucket's min and max.
        let pair = |f: &dyn Fn(&ChartPoint) -> f64| -> (f64, f64) {
            let (mut lo, mut hi) = (0usize, 0usize);
            for (i, p) in s.iter().enumerate() {
                if f(p) < f(&s[lo]) {
                    lo = i;
                }
                if f(p) > f(&s[hi]) {
                    hi = i;
                }
            }
            if lo <= hi {
                (f(&s[lo]), f(&s[hi]))
            } else {
                (f(&s[hi]), f(&s[lo]))
            }
        };
        let ue = pair(&|p| p.ue);
        let ram = pair(&|p| p.ram);
        let commit = pair(&|p| p.commit);
        out.push(ChartPoint { t: s[0].t, ue: ue.0, ram: ram.0, commit: commit.0, level });
        out.push(ChartPoint { t: s[s.len() - 1].t, ue: ue.1, ram: ram.1, commit: commit.1, level });
    }
    out
}

fn groups_sum(s: &Sample) -> f64 {
    s.groups.iter().map(|g| g.ram_gb).sum()
}

/// Where memory was freed. The CSV has no "freed" column, so it is inferred: the summed RAM of
/// the app groups dropped by more than 25% from one sample to the next (and by at least
/// 0.25 GB, so tiny groups jittering don't count). Returns the sample times.
pub fn freed_markers(samples: &[Sample]) -> Vec<i64> {
    samples
        .windows(2)
        .filter(|w| {
            let (a, b) = (groups_sum(&w[0]), groups_sum(&w[1]));
            a - b >= 0.25 && b < a * 0.75
        })
        .map(|w| w[1].unix)
        .collect()
}

/// Where an editor started: its committed memory went from 0 to something, or dropped by more
/// than half while still running (a restart: the new process starts small).
pub fn editor_starts(samples: &[Sample]) -> Vec<i64> {
    const ZERO: f64 = 0.05;
    samples
        .windows(2)
        .filter(|w| {
            let (a, b) = (w[0].editor_committed_gb, w[1].editor_committed_gb);
            b > ZERO && (a <= ZERO || b < a * 0.5)
        })
        .map(|w| w[1].unix)
        .collect()
}

/// Contiguous runs of tight-or-worse (`critical == false`) or critical pressure: (from, to).
pub fn bands(pts: &[ChartPoint], critical: bool) -> Vec<(i64, i64)> {
    let hit = |p: &ChartPoint| if critical { p.level == Level::Critical } else { p.level != Level::Normal };
    let mut out = Vec::new();
    let mut start: Option<i64> = None;
    for (i, p) in pts.iter().enumerate() {
        match (hit(p), start) {
            (true, None) => start = Some(p.t),
            (false, Some(s)) => {
                out.push((s, p.t));
                start = None;
            }
            _ => {}
        }
        if i == pts.len() - 1 {
            if let Some(s) = start {
                out.push((s, p.t));
            }
        }
    }
    out
}

/// The smallest "nice" tick step that gives at most `max_ticks` ticks over `span` seconds.
pub fn tick_step(span: i64, max_ticks: i64) -> i64 {
    const STEPS: [i64; 13] = [60, 300, 600, 900, 1800, 3600, 7200, 10_800, 14_400, 21_600, 43_200, 86_400, 172_800];
    let max_ticks = max_ticks.max(1);
    STEPS.iter().copied().find(|s| span / s <= max_ticks).unwrap_or(STEPS[STEPS.len() - 1])
}

/// Tick times between `from` and `to`, on whole steps of local time (`offset` = local - UTC).
pub fn ticks(from: i64, to: i64, offset: i64, max_ticks: i64) -> Vec<i64> {
    let step = tick_step(to - from, max_ticks);
    let first_local = (from + offset).div_euclid(step) * step + if (from + offset).rem_euclid(step) == 0 { 0 } else { step };
    let mut out = Vec::new();
    let mut t = first_local - offset;
    while t <= to {
        out.push(t);
        t += step;
    }
    out
}

/// Days since 1970-01-01 for a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
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
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

fn local_time_string(unix: i64, offset: i64) -> String {
    let l = unix + offset;
    let (y, m, d) = civil_from_days(l.div_euclid(86_400));
    let s = l.rem_euclid(86_400);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, s / 3600, s / 60 % 60, s % 60)
}

/// Local minus UTC in seconds, read off a sample's local `time` and its `unix`.
pub fn offset_from(s: &Sample) -> i64 {
    let p = |a: usize, b: usize| s.time.get(a..b).and_then(|x| x.parse::<i64>().ok());
    match (p(0, 4), p(5, 7), p(8, 10), p(11, 13), p(14, 16), p(17, 19)) {
        (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) => {
            let local = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
            ((local - s.unix) as f64 / 60.0).round() as i64 * 60
        }
        _ => 0,
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "13:00", or "Nov 12" for day steps.
pub fn tick_label(t: i64, offset: i64, step: i64) -> String {
    let l = t + offset;
    if step >= 86_400 {
        let (_, m, d) = civil_from_days(l.div_euclid(86_400));
        format!("{} {}", MONTHS[(m - 1) as usize], d)
    } else {
        let s = l.rem_euclid(86_400);
        format!("{:02}:{:02}", s / 3600, s / 60 % 60)
    }
}

fn readout_time(t: i64, offset: i64, range: Range) -> String {
    let hm = tick_label(t, offset, 60);
    if range == Range::D7 || range == Range::H24 {
        format!("{} {}", tick_label(t, offset, 86_400), hm)
    } else {
        hm
    }
}

/// Least-squares slope (GB per hour) of the editor's memory over the last 30 minutes of
/// `samples`, after its latest start. `None` with under 10 minutes of it.
pub fn editor_growth(samples: &[Sample]) -> Option<f64> {
    let last = samples.last()?;
    if last.editor_committed_gb <= 0.05 {
        return None;
    }
    let start = editor_starts(samples).last().copied().unwrap_or(i64::MIN);
    let pts: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.unix >= last.unix - 1800 && s.unix >= start && s.editor_committed_gb > 0.05)
        .map(|s| (s.unix as f64, s.editor_committed_gb))
        .collect();
    if pts.len() < 2 || pts[pts.len() - 1].0 - pts[0].0 < 600.0 {
        return None;
    }
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let num: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let den: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    (den > 0.0).then(|| num / den * 3600.0)
}

/// The cards under the chart: (title, line).
pub fn cards(samples: &[Sample]) -> Vec<(String, String)> {
    let Some(last) = samples.last() else { return Vec::new() };
    let peak_editor = samples.iter().map(|s| s.editor_committed_gb).fold(0.0, f64::max);
    let editor = if last.editor_committed_gb > 0.05 {
        let trend = match editor_growth(samples) {
            Some(g) if g >= 0.5 => format!("growing {:.1} GB an hour", g),
            _ => "steady".to_string(),
        };
        format!("Now {:.1} GB · peak {:.1} GB · {}", last.editor_committed_gb, peak_editor, trend)
    } else if peak_editor > 0.05 {
        format!("Not running · peak {:.1} GB", peak_editor)
    } else {
        "Not running".to_string()
    };
    let mut out = vec![("Unreal Editor".to_string(), editor)];
    for g in &last.groups {
        let peak = samples.iter().filter_map(|s| s.groups.iter().find(|x| x.name == g.name)).map(|x| x.ram_gb).fold(0.0, f64::max);
        out.push((g.name.clone(), format!("Now {:.1} GB · peak {:.1} GB", g.ram_gb, peak)));
    }
    out
}

// ---------------------------------------------------------------- layout

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Range,
    Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegendKind {
    Ue,
    Ram,
    Commit,
    Freed,
    Tight,
}

impl LegendKind {
    const ALL: [LegendKind; 5] = [LegendKind::Ue, LegendKind::Ram, LegendKind::Commit, LegendKind::Freed, LegendKind::Tight];
    fn label(self) -> &'static str {
        match self {
            LegendKind::Ue => "Unreal committed",
            LegendKind::Ram => "RAM free",
            LegendKind::Commit => "Commit free",
            LegendKind::Freed => "Memory freed",
            LegendKind::Tight => "Memory tight or critical",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub eyebrow: Rect,
    pub headline: Rect,
    pub ranges: Rect,
    pub chart: Rect,
    pub plot: Rect,
    pub legend: Vec<(Rect, LegendKind)>,
    pub cards: Vec<Rect>,
    pub footer: Rect,
    pub footer_text: Rect,
    pub link: Rect,
}

impl Layout {
    /// Every rectangle that must not overlap another (for tests).
    pub fn solid(&self) -> Vec<(String, Rect)> {
        let mut v = vec![
            ("eyebrow".to_string(), self.eyebrow),
            ("headline".into(), self.headline),
            ("ranges".into(), self.ranges),
            ("chart".into(), self.chart),
            ("footer_text".into(), self.footer_text),
            ("link".into(), self.link),
        ];
        v.extend(self.legend.iter().map(|(r, k)| (format!("legend {:?}", k), *r)));
        v.extend(self.cards.iter().enumerate().map(|(i, r)| (format!("card {}", i), *r)));
        v
    }
}

pub struct HistoryWindow {
    source: Box<dyn HistorySource>,
    pub range: Range,
    samples: Vec<Sample>,
    points: Vec<ChartPoint>,
    freed: Vec<i64>,
    starts: Vec<i64>,
    now: i64,
    offset: i64,
    cards: Vec<(String, String)>,
    mouse: Option<Pt>,
    pressed: Option<Act>,
    focus: Option<Act>,
    scale: Scale,
    size: (i32, i32),
    layout: Layout,
}

impl HistoryWindow {
    pub fn new(source: Box<dyn HistorySource>) -> HistoryWindow {
        let mut w = HistoryWindow {
            source,
            range: Range::H6,
            samples: Vec::new(),
            points: Vec::new(),
            freed: Vec::new(),
            starts: Vec::new(),
            now: 0,
            offset: 0,
            cards: Vec::new(),
            mouse: None,
            pressed: None,
            focus: None,
            scale: Scale(1.0),
            size: (960, 600),
            layout: Layout::default(),
        };
        w.reload();
        w
    }

    pub fn set_range(&mut self, r: Range) {
        self.range = r;
        self.reload();
    }

    pub fn set_mouse(&mut self, p: Option<Pt>) {
        self.mouse = p;
    }

    pub fn is_empty(&self) -> bool {
        self.samples.len() < 2
    }

    /// Read the samples for the current range again.
    pub fn reload(&mut self) {
        self.now = self.source.now_unix();
        self.samples = self.source.samples(Duration::from_secs(self.range.secs() as u64));
        self.offset = self.samples.last().map(offset_from).unwrap_or(0);
        self.freed = freed_markers(&self.samples);
        self.starts = editor_starts(&self.samples);
        let raw: Vec<ChartPoint> = self.samples.iter().map(ChartPoint::from_sample).collect();
        self.points = downsample(&raw, MAX_POINTS);
        self.cards = cards(&self.samples);
    }

    pub fn layout(&self, c: &Canvas) -> Layout {
        let (w, h) = (c.width(), c.height());
        let px = c.s(28.0);
        let content = Rect::new(px, c.s(24.0), w - 2 * px, 0);
        let mut l = Layout::default();
        let eh = c.line_height(F_EYEBROW);
        let hh = c.line_height(F_CHART_HEADLINE);
        l.eyebrow = Rect::new(content.x, content.y, c.measure_spaced("ATHER MEMGUARD", F_EYEBROW, c.s(3.0)).0 + c.s(4.0), eh);
        l.headline = Rect::new(content.x, content.y + eh + c.s(4.0), c.measure("MEMORY HISTORY", F_CHART_HEADLINE).0 + c.s(4.0), hh);
        let rw = c.s(232.0);
        let rh = c.s(32.0);
        l.ranges = Rect::new(content.right() - rw, l.headline.bottom() - rh, rw, rh);
        if l.ranges.x < l.headline.right() + c.s(16.0) {
            // Very narrow: pills under the headline.
            l.ranges = Rect::new(content.x, l.headline.bottom() + c.s(8.0), rw.min(content.w), rh);
        }
        let top = l.ranges.bottom().max(l.headline.bottom()) + c.s(16.0);

        let fh = c.s(44.0);
        l.footer = Rect::new(0, h - fh, w, fh);
        let link_text = "Open the CSV folder";
        let (lw, lh) = c.measure(link_text, F_HINT);
        l.link = Rect::new(w - px - lw - c.s(2.0), l.footer.cy() - lh / 2, lw + c.s(2.0), lh);
        l.footer_text = Rect::new(px, l.footer.y, (l.link.x - c.s(16.0) - px).max(0), fh);

        // Cards: as many columns of at least 190 px as fit.
        let gap = c.s(12.0);
        let card_h = c.s(10.0) * 2 + c.line_height(F_LABEL) + 2 * c.line_height(F_HINT);
        let n = if self.is_empty() { 0 } else { self.cards.len() as i32 };
        let cols = ((content.w + gap) / (c.s(190.0) + gap)).max(1);
        let rows = if n == 0 { 0 } else { (n + cols - 1) / cols };
        let legend_h = c.s(10.0) + c.line_height(F_HINT);
        let bottom = l.footer.y - c.s(20.0);
        let min_chart = c.s(160.0);
        let mut rows_fit = rows;
        while rows_fit > 0 && bottom - top - legend_h - c.s(18.0) - rows_fit * (card_h + gap) + gap < min_chart {
            rows_fit -= 1;
        }
        let cards_h = if rows_fit > 0 { c.s(18.0) + rows_fit * (card_h + gap) - gap } else { 0 };
        let chart_h = (bottom - top - legend_h - cards_h).max(c.s(60.0));
        l.chart = Rect::new(content.x, top, content.w, chart_h);
        l.plot = Rect::ltrb(l.chart.x + c.s(44.0), l.chart.y + c.s(14.0), l.chart.right() - c.s(18.0), l.chart.bottom() - c.s(26.0));

        if !self.is_empty() {
            let mut x = content.x;
            let ly = l.chart.bottom() + c.s(10.0);
            let lh = c.line_height(F_HINT);
            for k in LegendKind::ALL {
                let tw = c.measure(k.label(), F_HINT).0;
                let iw = c.s(20.0) + tw;
                if x + iw > content.right() {
                    break;
                }
                l.legend.push((Rect::new(x, ly, iw, lh), k));
                x += iw + c.s(18.0);
            }
            let cw = (content.w - (cols - 1) * gap) / cols;
            let cy0 = l.chart.bottom() + legend_h + c.s(18.0);
            for i in 0..(rows_fit * cols).min(n) {
                let (r, col) = (i / cols, i % cols);
                l.cards.push(Rect::new(content.x + col * (cw + gap), cy0 + r * (card_h + gap), cw, card_h));
            }
        }
        l
    }

    fn x_of(&self, plot: Rect, t: i64) -> f64 {
        let from = self.now - self.range.secs();
        plot.x as f64 + (t - from) as f64 / self.range.secs() as f64 * plot.w as f64
    }

    fn y_of(&self, plot: Rect, gb: f64) -> f64 {
        let total = self.source.ram_total_gb().max(1.0);
        plot.bottom() as f64 - (gb / total).clamp(0.0, 1.0) * plot.h as f64
    }

    /// Lines break where samples are further apart than this (the app wasn't running).
    fn gap_secs(&self) -> i64 {
        (self.source.interval_sec() as i64 * 6).max(self.range.secs() / 150)
    }

    fn paint_series(&self, c: &mut Canvas, plot: Rect, f: impl Fn(&ChartPoint) -> f64, col: Rgb, width: f32, dashed: bool) {
        let gap = self.gap_secs();
        let dash_on = c.s(4.0) as f64;
        let dash_period = dash_on * 2.0;
        let mut travelled = 0.0f64;
        let mut prev: Option<(i64, f64, f64)> = None;
        for p in &self.points {
            let (x, y) = (self.x_of(plot, p.t), self.y_of(plot, f(p)));
            if let Some((pt, px, py)) = prev {
                if p.t - pt <= gap {
                    if dashed {
                        // Dashes measured along the whole line, so dense points still dash.
                        let len = ((x - px).powi(2) + (y - py).powi(2)).sqrt();
                        let mut s = 0.0;
                        while s < len {
                            let phase = (travelled + s) % dash_period;
                            let run = if phase < dash_on { dash_on - phase } else { dash_period - phase };
                            let e = (s + run).min(len);
                            if phase < dash_on && len > 0.0 {
                                let a = Pt::new((px + (x - px) * s / len).round() as i32, (py + (y - py) * s / len).round() as i32);
                                let b = Pt::new((px + (x - px) * e / len).round() as i32, (py + (y - py) * e / len).round() as i32);
                                c.line(a, b, col, width, false);
                            }
                            s = e;
                        }
                        travelled += len;
                    } else {
                        c.line(Pt::new(px.round() as i32, py.round() as i32), Pt::new(x.round() as i32, y.round() as i32), col, width, false);
                    }
                }
            }
            prev = Some((p.t, x, y));
        }
    }

    fn paint_chart(&self, c: &mut Canvas, l: &Layout) {
        let chart = l.chart;
        let plot = l.plot;
        c.fill_round_rect_bordered(chart, c.s(RADIUS), SURFACE, BORDER);
        if self.is_empty() {
            let tw = (chart.w - c.s(80.0)).min(c.s(420.0));
            let lines = c.wrap_lines(EMPTY_TEXT, F_BODY, tw, 3);
            let lh = c.line_height(F_BODY) + c.s(2.0);
            let y0 = chart.cy() - lines.len() as i32 * lh / 2;
            for (i, line) in lines.iter().enumerate() {
                c.text(Rect::new(chart.x, y0 + i as i32 * lh, chart.w, lh), line, F_BODY, MUTED, Align::Center);
            }
            return;
        }
        c.push_clip(plot.inset(0, -c.s(2.0)));
        // Pressure bands.
        for (critical, col, alpha) in [(false, WARN, 26u8), (true, ERROR, 41u8)] {
            for (a, b) in bands(&self.points, critical) {
                let x0 = self.x_of(plot, a).round() as i32;
                let x1 = (self.x_of(plot, b).round() as i32).max(x0 + c.px1(2.0));
                c.fill_round_rect_alpha(Rect::new(x0, plot.y, x1 - x0, plot.h), 0, col, alpha);
            }
        }
        c.pop_clip();
        for (a, b) in bands(&self.points, false) {
            let x0 = self.x_of(plot, a).round() as i32;
            let x1 = self.x_of(plot, b).round() as i32;
            let tw = c.measure("tight", F_READ_SMALL).0;
            if x1 - x0 >= tw + c.s(8.0) {
                c.text(Rect::new(x0 + c.s(4.0), plot.y + c.s(2.0), tw + c.s(2.0), c.line_height(F_READ_SMALL)), "tight", F_READ_SMALL, WARN, Align::Left);
            }
        }
        // Gridlines and y labels every quarter of total RAM.
        let total = self.source.ram_total_gb().max(1.0);
        let lab_h = c.line_height(F_READ_SMALL);
        for k in 0..=4 {
            let gb = total * k as f64 / 4.0;
            let y = self.y_of(plot, gb).round() as i32;
            c.hline(plot.x, y, plot.w, BORDER);
            c.text(Rect::new(chart.x + c.s(6.0), y - lab_h / 2, plot.x - chart.x - c.s(14.0), lab_h), &format!("{:.0}", gb), F_READ_SMALL, MUTED, Align::Right);
        }
        // x labels: ticks on whole local hours (or days), and "now" at the right end.
        let from = self.now - self.range.secs();
        let max_ticks = (plot.w / c.s(90.0)).max(2) as i64;
        let step = tick_step(self.range.secs(), max_ticks);
        let ly = plot.bottom() + c.s(8.0);
        let now_w = c.measure("now", F_READ_SMALL).0;
        let now_x = plot.right() - now_w;
        c.text(Rect::new(now_x, ly, now_w + c.s(2.0), lab_h), "now", F_READ_SMALL, MUTED, Align::Left);
        for t in ticks(from, self.now, self.offset, max_ticks) {
            let s = tick_label(t, self.offset, step);
            let tw = c.measure(&s, F_READ_SMALL).0;
            let x = (self.x_of(plot, t).round() as i32 - tw / 2).max(plot.x - c.s(4.0));
            if x + tw > now_x - c.s(12.0) {
                continue;
            }
            c.text(Rect::new(x, ly, tw + c.s(2.0), lab_h), &s, F_READ_SMALL, MUTED, Align::Left);
        }
        // Editor starts: dashed vertical line with a label.
        let mut last_label_x = i32::MIN;
        for &t in &self.starts {
            let x = self.x_of(plot, t).round() as i32;
            if x < plot.x || x > plot.right() {
                continue;
            }
            let mut y = plot.y;
            while y < plot.bottom() {
                let e = (y + c.s(2.0).max(1)).min(plot.bottom());
                c.fill_rect(Rect::new(x, y, c.px1(1.0), e - y), TEXT.over(SURFACE, 0.7));
                y += c.s(6.0).max(3);
            }
            let label = "Editor started";
            let tw = c.measure(label, F_READ_SMALL).0;
            let lx = if x + c.s(6.0) + tw <= plot.right() { x + c.s(6.0) } else { x - c.s(6.0) - tw };
            if lx > last_label_x {
                c.text(Rect::new(lx, plot.y + c.s(16.0), tw + c.s(2.0), lab_h), label, F_READ_SMALL, TEXT_DIM, Align::Left);
                last_label_x = lx + tw + c.s(8.0);
            }
        }
        // The three lines.
        c.push_clip(plot.inset(-c.s(2.0), -c.s(2.0)));
        self.paint_series(c, plot, |p| p.commit, MUTED, c.scale.0 * 1.5, true);
        self.paint_series(c, plot, |p| p.ram, TEXT, c.scale.0 * 1.5, false);
        self.paint_series(c, plot, |p| p.ue, ACCENT, c.scale.0 * 2.5, false);
        c.pop_clip();
        // Memory freed: lime dots on the x axis.
        let d = c.s(8.0);
        for &t in &self.freed {
            let x = self.x_of(plot, t).round() as i32;
            if x >= plot.x && x <= plot.right() {
                c.fill_ellipse(Rect::new(x - d / 2, plot.bottom() - d / 2, d, d), ACCENT);
            }
        }
        // Hover: crosshair and readout.
        if let Some(m) = self.mouse.filter(|m| plot.contains(*m)) {
            self.paint_hover(c, plot, m);
        }
    }

    /// The point nearest the mouse.
    fn nearest(&self, plot: Rect, x: i32) -> Option<ChartPoint> {
        let from = self.now - self.range.secs();
        let t = from + ((x - plot.x) as f64 / plot.w.max(1) as f64 * self.range.secs() as f64) as i64;
        let i = self.points.partition_point(|p| p.t < t);
        let cands = [i.checked_sub(1), Some(i)];
        cands.iter().flatten().filter_map(|&i| self.points.get(i)).min_by_key(|p| (p.t - t).abs()).copied()
    }

    fn paint_hover(&self, c: &mut Canvas, plot: Rect, m: Pt) {
        let Some(p) = self.nearest(plot, m.x) else { return };
        if (p.t - (self.now - self.range.secs()) - ((m.x - plot.x) as f64 / plot.w as f64 * self.range.secs() as f64) as i64).abs() > self.gap_secs() {
            return; // nothing recorded here
        }
        let x = self.x_of(plot, p.t).round() as i32;
        c.fill_rect(Rect::new(x, plot.y, c.px1(1.0), plot.h), TEXT_DIM.over(SURFACE, 0.55));
        let d = c.s(7.0);
        for (v, col) in [(p.commit, MUTED), (p.ram, TEXT), (p.ue, ACCENT)] {
            let y = self.y_of(plot, v).round() as i32;
            c.fill_ellipse(Rect::new(x - d / 2, y - d / 2, d, d), col);
        }
        let rows = [
            ("Unreal committed", p.ue, ACCENT),
            ("RAM free", p.ram, TEXT),
            ("Commit free", p.commit, MUTED),
        ];
        let time = readout_time(p.t, self.offset, self.range);
        let pad = c.s(10.0);
        let lh = c.line_height(F_HINT) + c.s(2.0);
        let w = c.s(196.0);
        let h = pad * 2 + lh * 4;
        let mut bx = x + c.s(12.0);
        if bx + w > plot.right() {
            bx = x - c.s(12.0) - w;
        }
        let by = (m.y - h / 2).clamp(plot.y, (plot.bottom() - h).max(plot.y));
        let r = Rect::new(bx, by, w, h);
        c.fill_round_rect_bordered(r, c.s(RADIUS), RAISED, BORDER);
        let inner = r.inset(pad, pad);
        c.text(Rect::new(inner.x, inner.y, inner.w, lh), &time, F_LABEL, TEXT, Align::Left);
        for (i, (label, v, col)) in rows.iter().enumerate() {
            let y = inner.y + lh * (i as i32 + 1);
            let sw = c.s(10.0);
            c.fill_round_rect(Rect::new(inner.x, y + lh / 2 - c.s(1.5), sw, c.s(3.0).max(2)), c.s(1.0), *col);
            c.text(Rect::new(inner.x + sw + c.s(8.0), y, inner.w, lh), label, F_HINT, TEXT_DIM, Align::Left);
            c.text(Rect::new(inner.x, y, inner.w, lh), &format!("{:.1} GB", v), F_HINT, TEXT, Align::Right);
        }
    }

    fn paint_legend(&self, c: &mut Canvas, l: &Layout) {
        for (r, k) in &l.legend {
            let mid = r.cy();
            let sw = c.s(14.0);
            match k {
                LegendKind::Ue => c.fill_rect(Rect::new(r.x, mid - c.s(1.5), sw, c.s(3.0).max(2)), ACCENT),
                LegendKind::Ram => c.fill_rect(Rect::new(r.x, mid - c.s(1.0), sw, c.s(2.0).max(1)), TEXT),
                LegendKind::Commit => {
                    let dw = c.s(4.0);
                    c.fill_rect(Rect::new(r.x, mid - c.s(1.0), dw, c.s(2.0).max(1)), MUTED);
                    c.fill_rect(Rect::new(r.x + 2 * dw, mid - c.s(1.0), dw, c.s(2.0).max(1)), MUTED);
                }
                LegendKind::Freed => {
                    let d = c.s(8.0);
                    c.fill_ellipse(Rect::new(r.x + (sw - d) / 2, mid - d / 2, d, d), ACCENT);
                }
                LegendKind::Tight => {
                    let s = c.s(10.0);
                    c.fill_rect(Rect::new(r.x + (sw - s) / 2, mid - s / 2, s, s), WARN.over(BG, 0.4));
                }
            }
            c.text(Rect::new(r.x + c.s(20.0), r.y, r.w - c.s(20.0), r.h), k.label(), F_HINT, TEXT_DIM, Align::Left);
        }
    }

    fn paint_cards(&self, c: &mut Canvas, l: &Layout) {
        for (r, (title, line)) in l.cards.iter().zip(&self.cards) {
            c.fill_round_rect_bordered(*r, c.s(RADIUS), SURFACE, BORDER);
            let inner = r.inset(c.s(12.0), c.s(10.0));
            let th = c.line_height(F_LABEL);
            c.text(Rect::new(inner.x, inner.y, inner.w, th), title, F_LABEL, TEXT, Align::Left);
            let hh = c.line_height(F_HINT);
            for (i, s) in wrap_hard(c, line, F_HINT, inner.w, 2).iter().enumerate() {
                c.text_top(Rect::new(inner.x, inner.y + th + i as i32 * hh, inner.w, hh), s, F_HINT, MUTED, Align::Left);
            }
        }
    }

    fn footer_text(&self) -> String {
        format!("Recorded every {} seconds, kept for {} days", self.source.interval_sec(), self.source.keep_days())
    }

    fn ranges_widget(&self) -> Segmented {
        Segmented { options: Range::ALL.iter().map(|r| r.label().to_string()).collect(), selected: Range::ALL.iter().position(|r| *r == self.range).unwrap_or(1) }
    }

    fn hit(&self, p: Pt) -> Option<Act> {
        if self.layout.ranges.contains(p) {
            Some(Act::Range)
        } else if self.layout.link.contains(p) {
            Some(Act::Folder)
        } else {
            None
        }
    }

    fn activate(&mut self, a: Act, p: Option<Pt>, host: &mut Host) {
        match a {
            Act::Range => {
                let idx = match p {
                    Some(p) => {
                        let probe = Canvas::new(1, 1, self.scale);
                        self.ranges_widget().hit(&probe, self.layout.ranges, p)
                    }
                    None => Some((Range::ALL.iter().position(|r| *r == self.range).unwrap_or(0) + 1) % 4),
                };
                if let Some(i) = idx {
                    self.set_range(Range::ALL[i]);
                }
            }
            Act::Folder => {
                let dir = self.source.folder();
                let _ = std::fs::create_dir_all(&dir);
                host.shell_open(&dir.to_string_lossy());
            }
        }
        host.invalidate();
    }
}

impl Surface for HistoryWindow {
    fn paint(&mut self, c: &mut Canvas) {
        self.scale = c.scale;
        self.size = (c.width(), c.height());
        let l = self.layout(c);
        c.fill_rect(c.bounds(), BG);
        c.spaced_text(l.eyebrow, "ATHER MEMGUARD", F_EYEBROW, MUTED, c.s(3.0), Align::Left);
        c.text(l.headline, "MEMORY HISTORY", F_CHART_HEADLINE, TEXT, Align::Left);
        let over = |r: Rect| self.mouse.map(|m| r.contains(m)).unwrap_or(false);
        let st = WState { hover: over(l.ranges), focused: self.focus == Some(Act::Range), mouse: self.mouse.filter(|m| l.ranges.contains(*m)), ..Default::default() };
        self.ranges_widget().paint(c, l.ranges, &st);
        self.paint_chart(c, &l);
        self.paint_legend(c, &l);
        self.paint_cards(c, &l);
        c.fill_rect(l.footer, SURFACE);
        c.hline(0, l.footer.y, l.footer.w, BORDER);
        c.text(l.footer_text, &self.footer_text(), F_HINT, MUTED, Align::Left);
        let lst = WState { hover: over(l.link), focused: self.focus == Some(Act::Folder), ..Default::default() };
        paint_link(c, l.link, "Open the CSV folder", F_HINT, MUTED, &lst);
        if lst.focused {
            focus_ring(c, l.link, c.s(3.0));
        }
        self.layout = l;
    }

    fn event(&mut self, ev: Event, host: &mut Host) -> bool {
        self.scale = host.scale;
        match ev {
            Event::Resized => {
                host.set_timer(TIMER_REFRESH, 10_000);
                host.invalidate();
            }
            Event::Timer(TIMER_REFRESH) => {
                self.reload();
                host.invalidate();
            }
            Event::MouseMove(p) => {
                self.mouse = Some(p);
                host.set_cursor(if self.hit(p).is_some() { Cursor::Hand } else { Cursor::Arrow });
                host.invalidate();
            }
            Event::MouseLeave => {
                self.mouse = None;
                host.invalidate();
            }
            Event::MouseDown(p, MouseButton::Left) => {
                self.pressed = self.hit(p);
                self.focus = self.pressed;
                host.invalidate();
            }
            Event::MouseUp(p, MouseButton::Left) => {
                if let (Some(a), Some(h)) = (self.pressed.take(), self.hit(p)) {
                    if a == h {
                        self.activate(a, Some(p), host);
                    }
                }
            }
            Event::Key(vk, m) => {
                if m.alt {
                    return false;
                }
                match vk {
                    VK_ESCAPE => host.close(),
                    0x57 if m.ctrl => host.close(),
                    VK_TAB => {
                        self.focus = match self.focus {
                            None => Some(Act::Range),
                            Some(Act::Range) => Some(Act::Folder),
                            Some(Act::Folder) => None,
                        };
                        host.invalidate();
                    }
                    VK_LEFT | VK_RIGHT if self.focus == Some(Act::Range) || self.focus.is_none() => {
                        let i = Range::ALL.iter().position(|r| *r == self.range).unwrap_or(0);
                        let j = if vk == VK_RIGHT { (i + 1).min(3) } else { i.saturating_sub(1) };
                        self.set_range(Range::ALL[j]);
                        host.invalidate();
                    }
                    VK_RETURN | VK_SPACE => {
                        if let Some(a) = self.focus {
                            self.activate(a, None, host);
                        }
                    }
                    _ => {}
                }
            }
            Event::Destroyed => {
                if let Some(r) = host.window_rect() {
                    let _ = super::ui_state::save(KIND, r);
                }
            }
            _ => return false,
        }
        true
    }
}

/// Open the memory history window, or bring the open one to the front. Call on the UI thread.
/// It refreshes every 10 seconds while open.
pub fn open_history(source: Box<dyn HistorySource>) -> HWND {
    if let Some(h) = window::find(KIND) {
        window::bring_to_front(h);
        return h;
    }
    let spec = WindowSpec {
        kind: KIND,
        title: TITLE.into(),
        size: (960, 600),
        min_size: (640, 460),
        placement: super::ui_state::load(KIND),
        resizable: true,
    };
    window::open(spec, Box::new(HistoryWindow::new(source)))
}

/// Paint the window at `w` x `h` (96-DPI px), for snapshots.
pub fn render(win: &mut HistoryWindow, scale: Scale, w: i32, h: i32) -> Canvas {
    let mut c = Canvas::new(scale.s(w as f32), scale.s(h as f32), scale);
    win.paint(&mut c);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(unix: i64, editor: f64, groups: f64) -> Sample {
        Sample {
            unix,
            time: local_time_string(unix, 3600),
            mode: "editor".into(),
            pressure: Level::Normal,
            ram_free_gb: 50.0,
            commit_free_gb: 40.0,
            ue_committed_gb: editor,
            editor_committed_gb: editor,
            workers: 0,
            groups: vec![GroupRam { name: "Browsers".into(), ram_gb: groups }],
            editors: vec![],
        }
    }

    #[test]
    fn downsampling_keeps_spikes() {
        let pts: Vec<ChartPoint> = (0..20_000)
            .map(|i| ChartPoint {
                t: i,
                ue: if i == 12_345 { 99.0 } else { 10.0 + (i % 7) as f64 * 0.01 },
                ram: if i == 777 { 0.5 } else { 40.0 },
                commit: 30.0,
                level: if i == 15_000 { Level::Critical } else { Level::Normal },
            })
            .collect();
        let d = downsample(&pts, MAX_POINTS);
        assert!(d.len() <= MAX_POINTS && d.len() > MAX_POINTS - 4, "{}", d.len());
        assert_eq!(d.iter().map(|p| p.ue).fold(0.0, f64::max), 99.0, "a one-sample spike survives");
        assert_eq!(d.iter().map(|p| p.ram).fold(f64::MAX, f64::min), 0.5, "a one-sample dip survives");
        assert!(d.iter().any(|p| p.level == Level::Critical), "a one-sample critical moment survives");
        assert!(d.windows(2).all(|w| w[0].t <= w[1].t), "time order is kept");
        assert_eq!(d.first().unwrap().t, 0);
        assert_eq!(d.last().unwrap().t, 19_999);
        // Short series pass through untouched.
        assert_eq!(downsample(&pts[..100], MAX_POINTS), pts[..100].to_vec());
    }

    #[test]
    fn markers_are_inferred() {
        let s = vec![
            sample(0, 0.0, 8.0),
            sample(10, 4.0, 8.2),  // editor started
            sample(20, 30.0, 8.4),
            sample(30, 31.0, 5.0), // groups dropped 40%: freed
            sample(40, 32.0, 4.6), // small drop: not freed
            sample(50, 12.0, 4.7), // editor dropped by more than half: restart
            sample(60, 11.0, 4.8),
            sample(70, 0.0, 0.1),  // editor closed: not a start; groups -98%: freed
            sample(80, 0.0, 0.08), // tiny group jitter: under 0.25 GB, not freed
        ];
        assert_eq!(freed_markers(&s), vec![30, 70]);
        assert_eq!(editor_starts(&s), vec![10, 50]);
    }

    #[test]
    fn axis_ticks() {
        assert_eq!(tick_step(3600, 6), 600);
        assert_eq!(tick_step(6 * 3600, 6), 3600);
        assert_eq!(tick_step(6 * 3600, 12), 1800);
        assert_eq!(tick_step(86_400, 6), 14_400);
        assert_eq!(tick_step(7 * 86_400, 8), 86_400);
        assert_eq!(tick_step(7 * 86_400, 2), 172_800);
        // Ticks land on whole local hours: UTC+1, from 08:20 local to 14:20 local.
        let offset = 3600;
        let from = 20_771 * 86_400 + 7 * 3600 + 20 * 60;
        let t = ticks(from, from + 6 * 3600, offset, 6);
        assert_eq!(t.len(), 6);
        assert_eq!(tick_label(t[0], offset, 3600), "09:00");
        assert_eq!(tick_label(t[5], offset, 3600), "14:00");
        assert!(t.windows(2).all(|w| w[1] - w[0] == 3600));
        let days = ticks(from - 7 * 86_400, from, offset, 8);
        assert_eq!(tick_label(days[0], offset, 86_400), "Nov 8");
        assert_eq!(days.len(), 7);
        // A tick exactly at the start is kept.
        let t = ticks(36_000, 36_000 + 3600, 0, 6);
        assert_eq!(t[0], 36_000);
    }

    #[test]
    fn local_offset_and_dates() {
        assert_eq!(days_from_civil(2026, 11, 14), 20_771);
        assert_eq!(civil_from_days(20_771), (2026, 11, 14));
        let s = sample(20_771 * 86_400 + 3600, 0.0, 0.0);
        assert_eq!(s.time, "2026-11-14 02:00:00");
        assert_eq!(offset_from(&s), 3600);
    }

    #[test]
    fn bands_and_growth_and_cards() {
        let lv = [Level::Normal, Level::Elevated, Level::Critical, Level::Elevated, Level::Normal, Level::Critical];
        let pts: Vec<ChartPoint> = lv.iter().enumerate().map(|(i, l)| ChartPoint { t: i as i64 * 10, ue: 0.0, ram: 0.0, commit: 0.0, level: *l }).collect();
        assert_eq!(bands(&pts, false), vec![(10, 40), (50, 50)]);
        assert_eq!(bands(&pts, true), vec![(20, 30), (50, 50)]);
        // 2 GB an hour over 20 minutes.
        let s: Vec<Sample> = (0..=120).map(|i| sample(i * 10, 40.0 + i as f64 * 10.0 * 2.0 / 3600.0, 4.0)).collect();
        assert!((editor_growth(&s).unwrap() - 2.0).abs() < 1e-6);
        assert_eq!(editor_growth(&s[..30]), None, "under 10 minutes");
        let c = cards(&s);
        assert_eq!(c[0], ("Unreal Editor".to_string(), "Now 40.7 GB · peak 40.7 GB · growing 2.0 GB an hour".to_string()));
        assert_eq!(c[1], ("Browsers".to_string(), "Now 4.0 GB · peak 4.0 GB".to_string()));
        let flat: Vec<Sample> = (0..=120).map(|i| sample(i * 10, 40.0, 4.0)).collect();
        assert!(cards(&flat)[0].1.ends_with("steady"));
    }

    #[test]
    fn fake_history_has_the_mockup_story() {
        let f = FakeHistory::mockup();
        let s = f.samples(Duration::from_secs(6 * 3600));
        assert_eq!(s.len(), 6 * 360 + 1);
        assert!(!freed_markers(&s).is_empty());
        assert_eq!(editor_starts(&s).len(), 2, "the start at 08:30 and the restart at 12:45");
        assert!(s.iter().any(|x| x.pressure == Level::Elevated));
        assert!(s.iter().any(|x| x.pressure == Level::Critical));
        let week = f.samples(Duration::from_secs(7 * 86_400));
        assert!(downsample(&week.iter().map(ChartPoint::from_sample).collect::<Vec<_>>(), MAX_POINTS).len() <= MAX_POINTS);
        assert!(FakeHistory::empty().samples(Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn layout_does_not_overlap_at_two_scales() {
        for scale in [1.0, 1.5] {
            for (w, h) in [(960, 600), (640, 460), (1400, 900)] {
                for empty in [false, true] {
                    let src: Box<dyn HistorySource> = if empty { Box::new(FakeHistory::empty()) } else { Box::new(FakeHistory::mockup()) };
                    let win = HistoryWindow::new(src);
                    let s = Scale(scale);
                    let c = Canvas::new(s.s(w as f32), s.s(h as f32), s);
                    let l = win.layout(&c);
                    let ctx = format!("scale {scale} {w}x{h} empty {empty}");
                    let solid = l.solid();
                    for (name, r) in &solid {
                        assert!(c.bounds().encloses(r), "{ctx}: {name} {r:?} outside");
                    }
                    for (i, (a, ra)) in solid.iter().enumerate() {
                        for (b, rb) in &solid[i + 1..] {
                            assert!(!ra.intersects(rb), "{ctx}: {a} overlaps {b}");
                        }
                    }
                    assert!(l.chart.encloses(&l.plot), "{ctx}: plot outside chart");
                    assert!(l.plot.h >= s.s(100.0), "{ctx}: chart too small ({})", l.plot.h);
                    assert!(l.chart.bottom() <= l.footer.y, "{ctx}");
                    if !empty {
                        assert_eq!(l.legend.len(), 5, "{ctx}: legend cut");
                        assert!(!l.cards.is_empty(), "{ctx}: no cards");
                    }
                }
            }
        }
    }

    #[test]
    fn clicking_a_range_reloads() {
        let mut w = HistoryWindow::new(Box::new(FakeHistory::mockup()));
        let c = Canvas::new(960, 600, Scale(1.0));
        w.paint(&mut Canvas::new(960, 600, Scale(1.0)));
        let l = w.layout(&c);
        let mut host = Host::detached((960, 600), Scale(1.0));
        let p = Pt::new(l.ranges.x + l.ranges.w / 8, l.ranges.cy());
        w.event(Event::MouseDown(p, MouseButton::Left), &mut host);
        w.event(Event::MouseUp(p, MouseButton::Left), &mut host);
        assert_eq!(w.range, Range::H1);
        assert_eq!(w.samples.len(), 361);
    }
}

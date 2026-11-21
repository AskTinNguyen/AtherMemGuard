//! Memory history: an in-memory ring of the last 24 hours for the chart and the editor
//! growth alert, and one CSV file per day in `history\` in the data folder.
//!
//! CSV columns: time, mode, pressure, ram_free_gb, commit_free_gb, ue_committed_gb,
//! editor_committed_gb, workers, then one RAM column (GB) per app group, named after the
//! group. When the set of groups changes during a day, a new header line is written and
//! the rows after it follow that header.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
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
const GROWTH_MIN_SPAN_SECS: i64 = 20 * 60;
/// An editor's first minutes are spent loading (tens of GB in a couple of minutes, then
/// flat); that ramp is not growth, so samples this young are left out.
const GROWTH_SETTLE_SECS: i64 = 10 * 60;
/// Seconds between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_UNIX_DIFF_SECS: i64 = 11_644_473_600;
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
    /// Committed GB per editor process (pid, creation time as FILETIME or 0, GB). The
    /// creation time tells a recycled pid apart. In memory only, not in the CSV.
    #[serde(skip)]
    pub editors: Vec<(u32, u64, f64)>,
}

impl Sample {
    pub fn from_status(st: &Status, unix: i64) -> Sample {
        let editors: Vec<(u32, u64, f64)> = st
            .ue
            .processes
            .iter()
            .filter(|p| p.kind == "editor")
            .map(|p| (p.pid, p.created, p.committed_gb))
            .collect();
        Sample {
            unix,
            time: st.time.clone(),
            mode: st.mode.clone(),
            pressure: st.pressure,
            ram_free_gb: st.system.ram_free_gb,
            commit_free_gb: st.system.commit_free_gb,
            ue_committed_gb: st.ue.total_committed_gb,
            editor_committed_gb: round2(editors.iter().map(|e| e.2).sum()),
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

// ---------------------------------------------------------------- compact ring (<400 KB for 24h)

#[repr(C)]
#[derive(Clone, Copy)]
struct CompactSample {
    dt: u32,
    ram_free_gb: f32,
    commit_free_gb: f32,
    ue_committed_gb: f32,
    editor_committed_gb: f32,
    /// Bits 0..=9: workers (0..1023); bits 10..=11: pressure (0..2); bits 12..=15: editor_count (0..15).
    packed: u16,
    schema_id: u8,
    mode_id: u8,
}

impl CompactSample {
    fn workers(self) -> usize {
        (self.packed & 0x03FF) as usize
    }

    fn pressure(self) -> Level {
        match (self.packed >> 10) & 0x03 {
            1 => Level::Elevated,
            2 => Level::Critical,
            _ => Level::Normal,
        }
    }

    fn editor_count(self) -> usize {
        ((self.packed >> 12) & 0x0F) as usize
    }
}

fn pack_meta(workers: usize, pressure: Level, editor_count: usize) -> u16 {
    let w = (workers.min(0x03FF)) as u16;
    let p = match pressure {
        Level::Normal => 0u16,
        Level::Elevated => 1u16,
        Level::Critical => 2u16,
    };
    let e = (editor_count.min(0x0F)) as u16;
    w | (p << 10) | (e << 12)
}

fn encode_centi_gb(v: f64) -> u16 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let c = (v * 100.0).round();
    if c <= 32767.0 {
        c as u16
    } else {
        let d = (v * 10.0).round().clamp(0.0, 32767.0) as u16;
        0x8000 | d
    }
}

fn decode_centi_gb(c: u16) -> f64 {
    if c & 0x8000 == 0 {
        c as f64 / 100.0
    } else {
        (c & 0x7FFF) as f64 / 10.0
    }
}

fn reserve_tight<T>(q: &mut VecDeque<T>, needed: usize, chunk: usize) {
    let target = q.len() + needed;
    if q.capacity() < target {
        let aligned = target.next_multiple_of(chunk);
        q.reserve_exact(aligned - q.len());
    }
}

fn format_local_time(local_unix: i64) -> String {
    let day = local_unix.div_euclid(DAY_SECS);
    let secs = local_unix.rem_euclid(DAY_SECS);
    let (y, m, d) = civil_from_days(day);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

#[derive(Default)]
struct SampleOverride {
    tz_secs: Option<i32>,
    raw_time: Option<String>,
    raw_unix: Option<i64>,
    workers: Option<usize>,
    raw_mode: Option<String>,
    extra_editors: Option<Vec<(u32, u64, f64)>>,
    extra_groups: Option<Vec<(String, f64)>>,
}

struct CompactRing {
    samples: VecDeque<CompactSample>,
    group_vals: VecDeque<u16>,
    editor_ids: VecDeque<u16>,
    editor_gbs: VecDeque<f32>,
    group_names: Vec<String>,
    schemas: Vec<Vec<u8>>,
    modes: Vec<String>,
    editor_keys: Vec<(u32, u64)>,
    base_unix: i64,
    default_tz_secs: Option<i32>,
    head_seq: u64,
    overrides: VecDeque<(u64, SampleOverride)>,
    is_monotonic: bool,
}

impl Default for CompactRing {
    fn default() -> Self {
        CompactRing {
            samples: VecDeque::new(),
            group_vals: VecDeque::new(),
            editor_ids: VecDeque::new(),
            editor_gbs: VecDeque::new(),
            group_names: Vec::new(),
            schemas: Vec::new(),
            modes: Vec::new(),
            editor_keys: Vec::new(),
            base_unix: 0,
            default_tz_secs: None,
            head_seq: 0,
            overrides: VecDeque::new(),
            is_monotonic: true,
        }
    }
}

impl CompactRing {
    fn len(&self) -> usize {
        self.samples.len()
    }

    fn sample_unix(&self, idx: usize, s: &CompactSample) -> i64 {
        let seq = self.head_seq + idx as u64;
        self.find_override(seq)
            .and_then(|o| o.raw_unix)
            .unwrap_or_else(|| self.base_unix + s.dt as i64)
    }

    fn front_unix(&self) -> Option<i64> {
        self.samples.front().map(|s| self.sample_unix(0, s))
    }

    fn back_unix(&self) -> Option<i64> {
        self.samples.back().map(|s| self.sample_unix(self.samples.len() - 1, s))
    }

    fn find_override(&self, seq: u64) -> Option<&SampleOverride> {
        self.overrides
            .binary_search_by_key(&seq, |(k, _)| *k)
            .ok()
            .map(|idx| &self.overrides[idx].1)
    }

    fn sample_group_count(&self, s: &CompactSample, seq: u64) -> usize {
        if self.find_override(seq).is_some_and(|o| o.extra_groups.is_some()) {
            0
        } else {
            self.schemas.get(s.schema_id as usize).map_or(0, Vec::len)
        }
    }

    fn sample_editor_count(&self, s: &CompactSample, seq: u64) -> usize {
        if self.find_override(seq).is_some_and(|o| o.extra_editors.is_some()) {
            0
        } else {
            s.editor_count()
        }
    }

    fn compact_intern_tables(&mut self) {
        // Re-index schemas/group_names/modes/editor_keys only from live samples when tables grow large.
        let mut used_modes = vec![false; self.modes.len()];
        let mut used_schemas = vec![false; self.schemas.len()];
        let mut used_editors = vec![false; self.editor_keys.len()];
        for s in &self.samples {
            if let Some(m) = used_modes.get_mut(s.mode_id as usize) {
                *m = true;
            }
            if let Some(sc) = used_schemas.get_mut(s.schema_id as usize) {
                *sc = true;
            }
        }
        for &eid in &self.editor_ids {
            if let Some(e) = used_editors.get_mut(eid as usize) {
                *e = true;
            }
        }

        // Compact modes if any unused.
        if used_modes.iter().any(|&u| !u) {
            let mut map = vec![0u8; self.modes.len()];
            let mut new_modes = Vec::new();
            for (i, u) in used_modes.into_iter().enumerate() {
                if u {
                    map[i] = new_modes.len() as u8;
                    new_modes.push(std::mem::take(&mut self.modes[i]));
                }
            }
            for s in &mut self.samples {
                s.mode_id = map[s.mode_id as usize];
            }
            self.modes = new_modes;
        }

        // Compact editor_keys if any unused.
        if used_editors.iter().any(|&u| !u) {
            let mut map = vec![0u16; self.editor_keys.len()];
            let mut new_keys = Vec::new();
            for (i, u) in used_editors.into_iter().enumerate() {
                if u {
                    map[i] = new_keys.len() as u16;
                    new_keys.push(self.editor_keys[i]);
                }
            }
            for eid in &mut self.editor_ids {
                *eid = map[*eid as usize];
            }
            self.editor_keys = new_keys;
        }

        // Compact schemas & group_names if any schema unused.
        if used_schemas.iter().any(|&u| !u) {
            let mut smap = vec![0u8; self.schemas.len()];
            let mut new_schemas = Vec::new();
            let mut used_groups = vec![false; self.group_names.len()];
            for (i, u) in used_schemas.into_iter().enumerate() {
                if u {
                    smap[i] = new_schemas.len() as u8;
                    let sc = std::mem::take(&mut self.schemas[i]);
                    for &gid in &sc {
                        if let Some(g) = used_groups.get_mut(gid as usize) {
                            *g = true;
                        }
                    }
                    new_schemas.push(sc);
                }
            }
            let mut gmap = vec![0u8; self.group_names.len()];
            let mut new_groups = Vec::new();
            for (i, u) in used_groups.into_iter().enumerate() {
                if u {
                    gmap[i] = new_groups.len() as u8;
                    new_groups.push(std::mem::take(&mut self.group_names[i]));
                }
            }
            for sc in &mut new_schemas {
                for gid in sc.iter_mut() {
                    *gid = gmap[*gid as usize];
                }
            }
            for s in &mut self.samples {
                s.schema_id = smap[s.schema_id as usize];
            }
            self.schemas = new_schemas;
            self.group_names = new_groups;
        }
    }

    fn intern_mode(&mut self, mode: &str) -> Option<u8> {
        if let Some(pos) = self.modes.iter().position(|m| m == mode) {
            return Some(pos as u8);
        }
        if self.modes.len() >= 255 {
            return None;
        }
        let id = self.modes.len() as u8;
        self.modes.push(mode.to_string());
        Some(id)
    }

    fn intern_schema(&mut self, groups: &[GroupRam]) -> Option<u8> {
        let mut pending_names: Vec<String> = Vec::new();
        let mut gids = Vec::with_capacity(groups.len());
        for g in groups {
            let gid = if let Some(i) = self.group_names.iter().position(|n| n == &g.name) {
                i as u8
            } else if let Some(j) = pending_names.iter().position(|n| n == &g.name) {
                let idx = self.group_names.len() + j;
                if idx >= 255 {
                    return None;
                }
                idx as u8
            } else {
                let idx = self.group_names.len() + pending_names.len();
                if idx >= 255 {
                    return None;
                }
                pending_names.push(g.name.clone());
                idx as u8
            };
            gids.push(gid);
        }
        if pending_names.is_empty() {
            if let Some(pos) = self.schemas.iter().position(|s| s == &gids) {
                return Some(pos as u8);
            }
        }
        if self.schemas.len() >= 255 {
            return None;
        }
        self.group_names.extend(pending_names);
        let id = self.schemas.len() as u8;
        self.schemas.push(gids);
        Some(id)
    }

    fn intern_editor(&mut self, key: (u32, u64)) -> Option<u16> {
        if let Some(pos) = self.editor_keys.iter().position(|&k| k == key) {
            return Some(pos as u16);
        }
        if self.editor_keys.len() >= u16::MAX as usize {
            return None;
        }
        let id = self.editor_keys.len() as u16;
        self.editor_keys.push(key);
        Some(id)
    }

    fn push(&mut self, s: Sample) {
        // Compact intern tables upfront before allocating any per-sample IDs so
        // indices assigned during this sample are never shifted or invalidated mid-push.
        if self.modes.len() >= 245
            || self.group_names.len() + s.groups.len() > 240
            || self.schemas.len() >= 240
            || self.editor_keys.len() + s.editors.len() >= 512
        {
            self.compact_intern_tables();
        }

        let mut ov = SampleOverride::default();
        let mut has_ov = false;

        if self.samples.is_empty() {
            self.base_unix = s.unix;
        } else {
            if let Some(last) = self.back_unix() {
                if s.unix < last {
                    self.is_monotonic = false;
                }
            }
            if s.unix < self.base_unix {
                let diff = self.base_unix.saturating_sub(s.unix);
                if let Ok(delta) = u32::try_from(diff) {
                    if self.samples.iter().all(|e| e.dt.checked_add(delta).is_some()) {
                        for existing in &mut self.samples {
                            existing.dt += delta;
                        }
                        self.base_unix = s.unix;
                    }
                }
            }
        }
        let rel = s.unix.saturating_sub(self.base_unix);
        let dt = if s.unix >= self.base_unix {
            if let Ok(d) = u32::try_from(rel) {
                d
            } else {
                ov.raw_unix = Some(s.unix);
                has_ov = true;
                0
            }
        } else {
            ov.raw_unix = Some(s.unix);
            has_ov = true;
            0
        };

        if let Some(local) = parse_local_secs(&s.time) {
            let tz = (local - s.unix) as i32;
            if format_local_time(s.unix + tz as i64) == s.time {
                if self.default_tz_secs.is_none() {
                    self.default_tz_secs = Some(tz);
                } else if self.default_tz_secs != Some(tz) {
                    ov.tz_secs = Some(tz);
                    has_ov = true;
                }
            } else {
                ov.raw_time = Some(s.time.clone());
                has_ov = true;
            }
        } else {
            ov.raw_time = Some(s.time.clone());
            has_ov = true;
        }

        let mode_id = match self.intern_mode(&s.mode) {
            Some(id) => id,
            None => {
                ov.raw_mode = Some(s.mode.clone());
                has_ov = true;
                0
            }
        };

        let schema_id = match self.intern_schema(&s.groups) {
            Some(id) => {
                reserve_tight(&mut self.group_vals, s.groups.len(), 2048);
                for g in &s.groups {
                    self.group_vals.push_back(encode_centi_gb(g.ram_gb));
                }
                id
            }
            None => {
                ov.extra_groups = Some(s.groups.iter().map(|g| (g.name.clone(), g.ram_gb)).collect());
                has_ov = true;
                0
            }
        };

        let e_count = if s.editors.len() <= 15 {
            let mut ids = [0u16; 15];
            let mut ok = true;
            for (i, &(pid, created, _)) in s.editors.iter().enumerate() {
                match self.intern_editor((pid, created)) {
                    Some(id) => ids[i] = id,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                reserve_tight(&mut self.editor_ids, s.editors.len(), 512);
                reserve_tight(&mut self.editor_gbs, s.editors.len(), 512);
                for (i, &(_, _, gb)) in s.editors.iter().enumerate() {
                    self.editor_ids.push_back(ids[i]);
                    self.editor_gbs.push_back(gb as f32);
                }
                s.editors.len()
            } else {
                ov.extra_editors = Some(s.editors.clone());
                has_ov = true;
                0
            }
        } else {
            ov.extra_editors = Some(s.editors.clone());
            has_ov = true;
            0
        };

        if s.workers > 1023 {
            ov.workers = Some(s.workers);
            has_ov = true;
        }

        let seq = self.head_seq + self.samples.len() as u64;
        if has_ov {
            self.overrides.push_back((seq, ov));
        }

        reserve_tight(&mut self.samples, 1, 512);
        self.samples.push_back(CompactSample {
            dt,
            ram_free_gb: s.ram_free_gb as f32,
            commit_free_gb: s.commit_free_gb as f32,
            ue_committed_gb: s.ue_committed_gb as f32,
            editor_committed_gb: s.editor_committed_gb as f32,
            packed: pack_meta(s.workers, s.pressure, e_count),
            schema_id,
            mode_id,
        });

        let cutoff = s.unix - DAY_SECS;
        while self.samples.front().is_some_and(|f| self.sample_unix(0, f) < cutoff) {
            let popped = self.samples.pop_front().unwrap();
            let pseq = self.head_seq;
            self.head_seq += 1;
            let g_n = self.sample_group_count(&popped, pseq);
            let e_n = self.sample_editor_count(&popped, pseq);
            if g_n > 0 {
                self.group_vals.drain(..g_n);
            }
            if e_n > 0 {
                self.editor_ids.drain(..e_n);
                self.editor_gbs.drain(..e_n);
            }
            while self.overrides.front().is_some_and(|(k, _)| *k < self.head_seq) {
                self.overrides.pop_front();
            }
        }
    }

    fn reconstruct(&self, idx: usize, g_start: usize, e_start: usize) -> Sample {
        let s = self.samples[idx];
        let seq = self.head_seq + idx as u64;
        let ov = self.find_override(seq);
        let unix = ov.and_then(|o| o.raw_unix).unwrap_or_else(|| self.base_unix + s.dt as i64);
        let time = if let Some(raw) = ov.and_then(|o| o.raw_time.as_ref()) {
            raw.clone()
        } else {
            let tz = ov.and_then(|o| o.tz_secs).or(self.default_tz_secs).unwrap_or(0) as i64;
            format_local_time(unix + tz)
        };
        let mode = ov
            .and_then(|o| o.raw_mode.as_ref())
            .cloned()
            .unwrap_or_else(|| self.modes.get(s.mode_id as usize).cloned().unwrap_or_default());
        let workers = ov.and_then(|o| o.workers).unwrap_or_else(|| s.workers());
        let groups = if let Some(extra) = ov.and_then(|o| o.extra_groups.as_ref()) {
            extra.iter().map(|(n, ram_gb)| GroupRam { name: n.clone(), ram_gb: *ram_gb }).collect()
        } else {
            let schema = &self.schemas[s.schema_id as usize];
            schema
                .iter()
                .enumerate()
                .map(|(j, &gid)| GroupRam {
                    name: self.group_names[gid as usize].clone(),
                    ram_gb: decode_centi_gb(self.group_vals[g_start + j]),
                })
                .collect()
        };
        let editors = if let Some(extra) = ov.and_then(|o| o.extra_editors.as_ref()) {
            extra.clone()
        } else {
            let e_n = s.editor_count();
            (0..e_n)
                .map(|j| {
                    let (pid, created) = self.editor_keys[self.editor_ids[e_start + j] as usize];
                    (pid, created, self.editor_gbs[e_start + j] as f64)
                })
                .collect()
        };
        Sample {
            unix,
            time,
            mode,
            pressure: s.pressure(),
            ram_free_gb: s.ram_free_gb as f64,
            commit_free_gb: s.commit_free_gb as f64,
            ue_committed_gb: s.ue_committed_gb as f64,
            editor_committed_gb: s.editor_committed_gb as f64,
            workers,
            groups,
            editors,
        }
    }

    fn latest(&self) -> Option<Sample> {
        if self.samples.is_empty() {
            return None;
        }
        let idx = self.samples.len() - 1;
        let last = self.samples[idx];
        let seq = self.head_seq + idx as u64;
        let g_n = self.sample_group_count(&last, seq);
        let e_n = self.sample_editor_count(&last, seq);
        Some(self.reconstruct(idx, self.group_vals.len() - g_n, self.editor_ids.len() - e_n))
    }

    fn series_range(&self, min_unix: i64, max_unix: Option<i64>, strict_after: bool) -> Vec<Sample> {
        let mut out = Vec::new();
        let mut g_off = 0usize;
        let mut e_off = 0usize;
        for (i, s) in self.samples.iter().enumerate() {
            let seq = self.head_seq + i as u64;
            let g_n = self.sample_group_count(s, seq);
            let e_n = self.sample_editor_count(s, seq);
            let unix = self.sample_unix(i, s);
            let min_ok = if strict_after { unix > min_unix } else { unix >= min_unix };
            let max_ok = max_unix.is_none_or(|m| unix <= m);
            if min_ok && max_ok {
                out.push(self.reconstruct(i, g_off, e_off));
            }
            g_off += g_n;
            e_off += e_n;
        }
        out
    }

    fn memory_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<CompactRing>();
        total += self.samples.capacity() * std::mem::size_of::<CompactSample>();
        total += self.group_vals.capacity() * std::mem::size_of::<u16>();
        total += self.editor_ids.capacity() * std::mem::size_of::<u16>();
        total += self.editor_gbs.capacity() * std::mem::size_of::<f32>();
        total += self.group_names.capacity() * std::mem::size_of::<String>()
            + self.group_names.iter().map(String::capacity).sum::<usize>();
        total += self.schemas.capacity() * std::mem::size_of::<Vec<u8>>()
            + self.schemas.iter().map(Vec::capacity).sum::<usize>();
        total += self.modes.capacity() * std::mem::size_of::<String>()
            + self.modes.iter().map(String::capacity).sum::<usize>();
        total += self.editor_keys.capacity() * std::mem::size_of::<(u32, u64)>();
        total += self.overrides.capacity() * std::mem::size_of::<(u64, SampleOverride)>();
        total
    }
}

pub struct History {
    /// `None` = in memory only.
    dir: Option<PathBuf>,
    keep_days: u32,
    ring: CompactRing,
    /// The file and group columns the last header we know of was for.
    header: Option<(String, Vec<String>)>,
    last_cleanup: Option<String>,
    csv_buf: String,
    pending_rows: usize,
    pending_date: Option<String>,
    dir_created: bool,
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
        History {
            dir: Some(dir),
            keep_days: keep_days.max(1),
            ring: CompactRing::default(),
            header: None,
            last_cleanup: None,
            csv_buf: String::new(),
            pending_rows: 0,
            pending_date: None,
            dir_created: false,
        }
    }

    /// A history that keeps samples in memory only.
    pub fn in_memory() -> History {
        History {
            dir: None,
            keep_days: 1,
            ring: CompactRing::default(),
            header: None,
            last_cleanup: None,
            csv_buf: String::new(),
            pending_rows: 0,
            pending_date: None,
            dir_created: false,
        }
    }

    /// The history in the data folder, with the last 24 hours read back from disk.
    pub fn open_default(keep_days: u32) -> History {
        let mut h = History::new(history_dir(), keep_days);
        for s in load_since_from(&history_dir(), Duration::from_secs(DAY_SECS as u64), now_unix(), local_offset_secs()) {
            h.ring.push(s);
        }
        h
    }

    pub fn set_keep_days(&mut self, days: u32) {
        self.keep_days = days.max(1);
    }

    /// Number of samples currently held in the 24-hour ring.
    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Approximate heap + stack memory footprint of the compact ring in bytes.
    pub fn ring_memory_bytes(&self) -> usize {
        self.ring.memory_bytes()
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
        self.ring.push(s);
    }

    /// Flush any buffered CSV rows to disk. Also called automatically on `Drop`.
    pub fn flush(&mut self) {
        if let Err(e) = self.flush_csv() {
            crate::log::write(&format!("history: {}", e));
        }
    }

    fn flush_csv(&mut self) -> Result<(), String> {
        if self.csv_buf.is_empty() {
            self.pending_rows = 0;
            return Ok(());
        }
        let Some(dir) = self.dir.as_ref() else {
            self.csv_buf.clear();
            self.pending_rows = 0;
            return Ok(());
        };
        let Some(date) = self.pending_date.as_deref() else {
            self.csv_buf.clear();
            self.pending_rows = 0;
            return Ok(());
        };
        let res = (|| -> Result<(), String> {
            if !self.dir_created {
                fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
                self.dir_created = true;
            }
            let path = dir.join(format!("{}.csv", date));
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("{}: {}", path.display(), e))?;
            f.write_all(self.csv_buf.as_bytes()).map_err(|e| format!("{}: {}", path.display(), e))
        })();
        if res.is_err() {
            self.header = None;
        }
        self.csv_buf.clear();
        self.pending_rows = 0;
        res
    }

    fn append_csv(&mut self, dir: &Path, s: &Sample) -> Result<(), String> {
        let date = s.date();
        if self.pending_date.as_deref() != Some(date) && !self.csv_buf.is_empty() {
            self.flush_csv()?;
        }
        let header_matches = self.header.as_ref().is_some_and(|(hd, hg)| {
            hd == date && hg.len() == s.groups.len() && hg.iter().zip(s.groups.iter()).all(|(a, b)| a == &b.name)
        });
        if !header_matches {
            let same_day = self.header.as_ref().is_some_and(|(hd, _)| hd == date);
            let groups: Vec<String> = s.groups.iter().map(|g| g.name.clone()).collect();
            let need_header = if same_day {
                true
            } else {
                let path = dir.join(format!("{}.csv", date));
                last_header_groups(&path).as_ref() != Some(&groups)
            };
            if need_header {
                write_header_line(&mut self.csv_buf, &groups);
            }
            self.header = Some((date.to_string(), groups));
        }
        write_row_line(&mut self.csv_buf, s);
        self.pending_rows += 1;
        if self.pending_date.as_deref() != Some(date) {
            self.pending_date = Some(date.to_string());
        }
        if s.pressure != Level::Normal || self.pending_rows >= 6 || self.csv_buf.len() >= 4096 {
            self.flush_csv()?;
        }
        Ok(())
    }

    /// Samples from the last `since` (at most 24 hours: the ring), oldest first.
    pub fn series(&self, since: Duration) -> Vec<Sample> {
        self.series_at(now_unix(), since)
    }

    fn series_at(&self, now: i64, since: Duration) -> Vec<Sample> {
        let from = now - since.as_secs() as i64;
        self.ring.series_range(from, None, false)
    }

    /// The most recent sample.
    pub fn latest(&self) -> Option<Sample> {
        self.ring.latest()
    }

    /// Unix timestamp of the most recent sample, without allocating a `Sample`.
    pub fn latest_unix(&self) -> Option<i64> {
        self.ring.back_unix()
    }

    /// Samples newer than `after` (unix seconds), oldest first. `None` when the ring doesn't
    /// reach back that far, so it can't say what came after.
    pub fn series_after(&self, after: i64) -> Option<Vec<Sample>> {
        if self.ring.front_unix().is_some_and(|u| u > after) {
            return None;
        }
        Some(self.ring.series_range(after, None, true))
    }

    /// Samples from the last `since`, any length: the ring for what it holds, the day files
    /// for what is older. Past days don't change, so each is read once into `cache`.
    pub fn series_with_days(&self, since: Duration, cache: &mut DayCache) -> Vec<Sample> {
        let dir = self.dir.clone().unwrap_or_else(history_dir);
        self.series_with_days_at(&dir, now_unix(), local_offset_secs(), since, cache)
    }

    fn series_with_days_at(&self, dir: &Path, now: i64, offset: i64, since: Duration, cache: &mut DayCache) -> Vec<Sample> {
        let from = now - since.as_secs() as i64;
        let ring_start = self.ring.front_unix().unwrap_or(now + 1);
        let mut out = Vec::new();
        if from < ring_start {
            let today = (now + offset).div_euclid(DAY_SECS);
            let first_day = (from + offset).div_euclid(DAY_SECS);
            let last_day = (ring_start + offset).div_euclid(DAY_SECS).min(today);
            cache.days.retain(|&d, _| d >= first_day && d < today);
            for day in first_day..=last_day {
                let fresh;
                let samples = if day < today {
                    cache.days.entry(day).or_insert_with(|| load_day_from(dir, &date_string(day), offset).unwrap_or_default())
                } else {
                    fresh = load_day_from(dir, &date_string(day), offset).unwrap_or_default();
                    &fresh
                };
                out.extend(samples.iter().filter(|s| s.unix >= from && s.unix < ring_start).cloned());
            }
            out.sort_by_key(|s| s.unix);
        }
        out.extend(self.ring.series_range(from, Some(now), false));
        out
    }

    /// Every sample in one day's file. `date` is `YYYY-MM-DD` (local date).
    pub fn load_day(&self, date: &str) -> Result<Vec<Sample>, String> {
        let dir = self.dir.clone().unwrap_or_else(history_dir);
        load_day_from(&dir, date, local_offset_secs())
    }

    /// How fast this editor's committed memory grows, in GB per hour: the least-squares
    /// slope over the last 30 minutes, capped at twice the slope of either half of that
    /// window. `created` is the process creation time (FILETIME, 0 if unknown), so a
    /// recycled pid gets its own series.
    ///
    /// `None` until there are 20 minutes of samples for it, not counting its first 10
    /// minutes (loading). The cap keeps a load ramp that went flat, or a single step, from
    /// reading as steady growth: both halves have to grow.
    pub fn editor_growth_gb_per_hour(&self, pid: u32, created: u64) -> Option<f64> {
        let target_id = self.ring.editor_keys.iter().position(|&k| k == (pid, created)).map(|i| i as u16);
        let end = self.ring.back_unix()?;
        let mut from = end - GROWTH_WINDOW_SECS;
        if created > 0 {
            from = from.max((created / 10_000_000) as i64 - FILETIME_UNIX_DIFF_SECS + GROWTH_SETTLE_SECS);
        }
        let mut points: Vec<(f64, f64)> = Vec::new();
        let mut e_end = self.ring.editor_ids.len();
        for (rev_i, s) in self.ring.samples.iter().enumerate().rev() {
            let seq = self.ring.head_seq + rev_i as u64;
            let e_count = self.ring.sample_editor_count(s, seq);
            let e_start = e_end - e_count;
            let unix = self.ring.sample_unix(rev_i, s);
            if self.ring.is_monotonic && unix < from {
                break;
            }
            if unix >= from {
                if let Some(tid) = target_id {
                    if let Some(pos) = (e_start..e_end).find(|&idx| self.ring.editor_ids[idx] == tid) {
                        points.push((unix as f64, self.ring.editor_gbs[pos] as f64));
                    }
                }
                if let Some(extra) = self.ring.find_override(seq).and_then(|o| o.extra_editors.as_ref()) {
                    if let Some(e) = extra.iter().find(|e| e.0 == pid && e.1 == created) {
                        points.push((unix as f64, e.2));
                    }
                }
            }
            e_end = e_start;
        }
        if !self.ring.is_monotonic {
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
        } else {
            points.reverse();
        }
        growth_from_points(&points)
    }

    /// Aggregate summary over the last `since` in the ring.
    pub fn session_summary(&self, since: Duration) -> SessionSummary {
        self.session_summary_at(now_unix(), since)
    }

    /// Aggregate summary over `[now - since, now]` in the ring.
    pub fn session_summary_at(&self, now: i64, since: Duration) -> SessionSummary {
        let samples = self.series_at(now, since);
        let mut summary = summarize_samples(&samples);
        if summary.editor_growth_gb_per_hour.is_none() {
            if let Some(latest) = samples.last() {
                let mut best: Option<f64> = None;
                for &(pid, created, _) in &latest.editors {
                    if let Some(g) = self.editor_growth_gb_per_hour(pid, created) {
                        best = Some(best.map_or(g, |b| b.max(g)));
                    }
                }
                summary.editor_growth_gb_per_hour = best.map(round2);
            }
        }
        summary
    }
}

impl Drop for History {
    fn drop(&mut self) {
        self.flush();
    }
}

fn growth_from_points(points: &[(f64, f64)]) -> Option<f64> {
    let (first, last) = (points.first()?.0, points.last()?.0);
    if last - first < GROWTH_MIN_SPAN_SECS as f64 {
        return None;
    }
    let slope = slope_per_hour(points)?;
    let mid = points.partition_point(|p| p.0 < (first + last) / 2.0);
    let early = slope_per_hour(&points[..mid])?;
    let late = slope_per_hour(&points[mid..])?;
    Some(slope.min(2.0 * early).min(2.0 * late))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupPeak {
    pub name: String,
    pub latest_gb: f64,
    pub peak_gb: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionSummary {
    pub sample_count: usize,
    pub window_start: Option<i64>,
    pub window_end: Option<i64>,
    pub window_hours: f64,
    pub span_minutes: f64,
    pub peak_ue_committed_gb: f64,
    pub peak_editor_committed_gb: f64,
    pub latest_ue_committed_gb: f64,
    pub latest_editor_committed_gb: f64,
    pub min_ram_free_gb: f64,
    pub min_commit_free_gb: f64,
    pub latest_ram_free_gb: f64,
    pub latest_commit_free_gb: f64,
    pub peak_workers: usize,
    pub normal_minutes: f64,
    pub elevated_minutes: f64,
    pub critical_minutes: f64,
    pub pressure_transitions: usize,
    pub editor_growth_gb_per_hour: Option<f64>,
    pub groups: Vec<GroupPeak>,
}

/// Summarize a slice of oldest-first `Sample`s (from `History::series` or `load_since`).
pub fn summarize_samples(samples: &[Sample]) -> SessionSummary {
    if samples.is_empty() {
        return SessionSummary {
            sample_count: 0,
            window_start: None,
            window_end: None,
            window_hours: 0.0,
            span_minutes: 0.0,
            peak_ue_committed_gb: 0.0,
            peak_editor_committed_gb: 0.0,
            latest_ue_committed_gb: 0.0,
            latest_editor_committed_gb: 0.0,
            min_ram_free_gb: 0.0,
            min_commit_free_gb: 0.0,
            latest_ram_free_gb: 0.0,
            latest_commit_free_gb: 0.0,
            peak_workers: 0,
            normal_minutes: 0.0,
            elevated_minutes: 0.0,
            critical_minutes: 0.0,
            pressure_transitions: 0,
            editor_growth_gb_per_hour: None,
            groups: Vec::new(),
        };
    }

    let first = &samples[0];
    let last = &samples[samples.len() - 1];
    let span_secs = (last.unix - first.unix).max(0) as f64;
    let mut peak_ue = first.ue_committed_gb;
    let mut peak_ed = first.editor_committed_gb;
    let mut min_ram = first.ram_free_gb;
    let mut min_commit = first.commit_free_gb;
    let mut peak_workers = first.workers;
    let mut group_peaks: Vec<GroupPeak> = Vec::new();
    let mut normal_secs = 0i64;
    let mut elevated_secs = 0i64;
    let mut critical_secs = 0i64;
    let mut transitions = 0usize;

    for (i, s) in samples.iter().enumerate() {
        peak_ue = peak_ue.max(s.ue_committed_gb);
        peak_ed = peak_ed.max(s.editor_committed_gb);
        min_ram = min_ram.min(s.ram_free_gb);
        min_commit = min_commit.min(s.commit_free_gb);
        peak_workers = peak_workers.max(s.workers);
        for g in &s.groups {
            if let Some(gp) = group_peaks.iter_mut().find(|gp| gp.name == g.name) {
                gp.latest_gb = round2(g.ram_gb);
                gp.peak_gb = round2(gp.peak_gb.max(g.ram_gb));
            } else {
                group_peaks.push(GroupPeak {
                    name: g.name.clone(),
                    latest_gb: round2(g.ram_gb),
                    peak_gb: round2(g.ram_gb),
                });
            }
        }
        if let Some(next) = samples.get(i + 1) {
            let dt = (next.unix - s.unix).max(0);
            match s.pressure {
                Level::Normal => normal_secs += dt,
                Level::Elevated => elevated_secs += dt,
                Level::Critical => critical_secs += dt,
            }
            if next.pressure != s.pressure {
                transitions += 1;
            }
        }
    }

    let mut editor_growth: Option<f64> = None;
    let end = last.unix;
    if !last.editors.is_empty() {
        for &(pid, created, _) in &last.editors {
            let mut from = end - GROWTH_WINDOW_SECS;
            if created > 0 {
                from = from.max((created / 10_000_000) as i64 - FILETIME_UNIX_DIFF_SECS + GROWTH_SETTLE_SECS);
            }
            let pts: Vec<(f64, f64)> = samples
                .iter()
                .filter(|s| s.unix >= from)
                .filter_map(|s| s.editors.iter().find(|e| e.0 == pid && e.1 == created).map(|e| (s.unix as f64, e.2)))
                .collect();
            if let Some(g) = growth_from_points(&pts) {
                editor_growth = Some(editor_growth.map_or(g, |b| b.max(g)));
            }
        }
    }

    SessionSummary {
        sample_count: samples.len(),
        window_start: Some(first.unix),
        window_end: Some(last.unix),
        window_hours: round2(span_secs / 3600.0),
        span_minutes: round2(span_secs / 60.0),
        peak_ue_committed_gb: round2(peak_ue),
        peak_editor_committed_gb: round2(peak_ed),
        latest_ue_committed_gb: round2(last.ue_committed_gb),
        latest_editor_committed_gb: round2(last.editor_committed_gb),
        min_ram_free_gb: round2(min_ram),
        min_commit_free_gb: round2(min_commit),
        latest_ram_free_gb: round2(last.ram_free_gb),
        latest_commit_free_gb: round2(last.commit_free_gb),
        peak_workers,
        normal_minutes: round2(normal_secs as f64 / 60.0),
        elevated_minutes: round2(elevated_secs as f64 / 60.0),
        critical_minutes: round2(critical_secs as f64 / 60.0),
        pressure_transitions: transitions,
        editor_growth_gb_per_hour: editor_growth.map(round2),
        groups: group_peaks,
    }
}

/// Parsed day files by day number, for `History::series_with_days`.
#[derive(Default)]
pub struct DayCache {
    days: HashMap<i64, Vec<Sample>>,
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

fn write_csv_field(buf: &mut String, s: &str) {
    if s.contains([',', '"', '\n', '\r']) {
        buf.push('"');
        for c in s.chars() {
            if c == '"' {
                buf.push_str("\"\"");
            } else {
                buf.push(c);
            }
        }
        buf.push('"');
    } else {
        buf.push_str(s);
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

fn write_header_line(buf: &mut String, groups: &[String]) {
    for (i, col) in FIXED_COLUMNS.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        buf.push_str(col);
    }
    for g in groups {
        buf.push(',');
        write_csv_field(buf, g);
    }
    buf.push('\n');
}

fn write_row_line(buf: &mut String, s: &Sample) {
    write_csv_field(buf, &s.time);
    buf.push(',');
    write_csv_field(buf, &s.mode);
    buf.push(',');
    buf.push_str(s.pressure.label());
    let _ = write!(
        buf,
        ",{:.2},{:.2},{:.2},{:.2},{}",
        s.ram_free_gb, s.commit_free_gb, s.ue_committed_gb, s.editor_committed_gb, s.workers
    );
    for g in &s.groups {
        let _ = write!(buf, ",{:.2}", g.ram_gb);
    }
    buf.push('\n');
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
        let editors: Vec<(u32, u64, f64)> = editors.iter().map(|&(pid, gb)| (pid, 0, gb)).collect();
        sample_c(unix, &editors)
    }

    /// FILETIME for a unix time.
    fn ft(unix: i64) -> u64 {
        (unix + FILETIME_UNIX_DIFF_SECS) as u64 * 10_000_000
    }

    fn sample_c(unix: i64, editors: &[(u32, u64, f64)]) -> Sample {
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
            editor_committed_gb: editors.iter().map(|e| e.2).sum::<f64>() + 0.0,
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
        let g = h.editor_growth_gb_per_hour(7, 0).unwrap();
        assert!((g - 6.0).abs() < 1e-6, "{}", g);
        assert!(h.editor_growth_gb_per_hour(9, 0).unwrap().abs() < 1e-9);
        assert!(h.editor_growth_gb_per_hour(1234, 0).is_none());
        assert!(h.editor_growth_gb_per_hour(7, ft(T0)).is_none(), "a different creation time is another process");
    }

    #[test]
    fn growth_needs_twenty_minutes() {
        let mut h = History::in_memory();
        for i in 0..=119 {
            h.push(sample(T0 + i * 10, &[(7, 20.0 + i as f64 * 0.01)]));
        }
        assert!(h.editor_growth_gb_per_hour(7, 0).is_none()); // 1190 s
        h.push(sample(T0 + 1200, &[(7, 21.2)]));
        assert!(h.editor_growth_gb_per_hour(7, 0).is_some());
        // Only the last 30 minutes count: an old ramp followed by a flat hour is flat.
        let mut h = History::in_memory();
        for i in 0..360 {
            let gb = if i < 180 { i as f64 } else { 180.0 };
            h.push(sample(T0 + i * 20, &[(7, gb)]));
        }
        assert!(h.editor_growth_gb_per_hour(7, 0).unwrap().abs() < 1e-9);
    }

    /// Like the history CSV of a real editor: loads ~2 -> 46 GB in 150 s, then stays flat.
    #[test]
    fn growth_ignores_load_ramp() {
        let born = T0;
        for created in [ft(born), 0] {
            let mut h = History::in_memory();
            let mut worst: f64 = 0.0;
            for i in 0..=(60 * 6) {
                let t = born + 5 + i * 10;
                let age = (t - born) as f64;
                let gb = if age < 150.0 { 2.0 + 44.0 * age / 150.0 } else { 46.0 + if i % 2 == 0 { 0.02 } else { -0.02 } };
                h.push(sample_c(t, &[(7, created, gb)]));
                if let Some(g) = h.editor_growth_gb_per_hour(7, created) {
                    worst = worst.max(g);
                }
            }
            assert!(worst < 1.0, "created {}: read {} GB/h from a load ramp", created, worst);
        }
    }

    #[test]
    fn growth_ignores_single_step() {
        // A long-running editor, flat at 20 GB, takes one 2 GB step at minute 40.
        let created = ft(T0 - 3600);
        let mut h = History::in_memory();
        let mut worst: f64 = 0.0;
        for i in 0..=(90 * 6) {
            let t = T0 + i * 10;
            let gb = if i < 40 * 6 { 20.0 } else { 22.0 };
            h.push(sample_c(t, &[(7, created, gb)]));
            if let Some(g) = h.editor_growth_gb_per_hour(7, created) {
                worst = worst.max(g);
            }
        }
        assert!(worst < 3.0, "read {} GB/h from a single step", worst);
        // Steady growth still reads as growth.
        let mut h = History::in_memory();
        for i in 0..=(30 * 6) {
            let t = T0 + i * 10;
            h.push(sample_c(t, &[(7, created, 20.0 + 12.0 * (i * 10) as f64 / 3600.0)]));
        }
        let g = h.editor_growth_gb_per_hour(7, created).unwrap();
        assert!((g - 12.0).abs() < 1e-6, "{}", g);
    }

    #[test]
    fn growth_series_per_process_not_per_pid() {
        // Editor pid 7 sits at 40 GB for an hour and exits; a new editor gets pid 7 and
        // grows 12 GB/h from 5 GB. The old samples must not bend the new slope.
        let (old, new) = (ft(T0 - 7200), ft(T0 + 3600));
        let mut h = History::in_memory();
        for i in 0..(60 * 6) {
            h.push(sample_c(T0 + i * 10, &[(7, old, 40.0)]));
        }
        for i in 0..=(40 * 6) {
            let t = T0 + 3600 + i * 10;
            h.push(sample_c(t, &[(7, new, 5.0 + 12.0 * (i * 10) as f64 / 3600.0)]));
        }
        let g = h.editor_growth_gb_per_hour(7, new).unwrap();
        assert!((g - 12.0).abs() < 1e-6, "{}", g);
        assert!(h.editor_growth_gb_per_hour(7, old).is_none());
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
        assert!(h.ring.front_unix().unwrap() >= now - DAY_SECS);
        assert_eq!(h.series_at(now, Duration::from_secs(3600)).len(), 7);
        assert_eq!(h.latest().unwrap().unix, now);
        assert_eq!(h.latest_unix(), Some(now));
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
    fn series_after_and_long_series_from_cached_days() {
        let dir = temp_dir();
        // Two past days on disk, then a ring that holds the last hours.
        let mut disk = History::new(dir.clone(), 7);
        for i in 0..48 {
            disk.push(sample(T0 - 2 * DAY_SECS + i * 3600, &[]));
        }
        let mut h = History::in_memory();
        for i in 0..6 {
            h.push(sample(T0 + i * 600, &[]));
        }
        let now = T0 + 3000;
        let mut cache = DayCache::default();
        let all = h.series_with_days_at(&dir, now, 0, Duration::from_secs(3 * DAY_SECS as u64), &mut cache);
        assert_eq!(all.len(), 48 + 6);
        assert!(all.windows(2).all(|w| w[0].unix < w[1].unix));
        assert_eq!(all.last().unwrap().unix, T0 + 3000);
        // A changed past-day file isn't read again: past days don't change.
        let first = all[0].date().to_string();
        fs::write(dir.join(format!("{}.csv", first)), "").unwrap();
        let again = h.series_with_days_at(&dir, now, 0, Duration::from_secs(3 * DAY_SECS as u64), &mut cache);
        assert_eq!(again, all);
        // A short range comes from the ring alone.
        assert_eq!(h.series_with_days_at(&dir, now, 0, Duration::from_secs(1200), &mut cache).len(), 3);

        assert_eq!(h.series_after(T0 + 1800).unwrap().iter().map(|s| s.unix).collect::<Vec<_>>(), [T0 + 2400, T0 + 3000]);
        assert!(h.series_after(T0 + 3000).unwrap().is_empty());
        assert_eq!(h.series_after(T0).unwrap().len(), 5);
        assert!(h.series_after(T0 - 1).is_none(), "older than the ring");
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

    #[test]
    fn compact_ring_24h_under_400kb() {
        assert_eq!(std::mem::size_of::<CompactSample>(), 24);
        let mut h = History::in_memory();
        let day_samples = 8640i64; // 10s interval for 24h
        for i in 0..day_samples {
            let t = T0 + i * 10;
            let day = t.div_euclid(DAY_SECS);
            let secs = t.rem_euclid(DAY_SECS);
            let s = Sample {
                unix: t,
                time: format!("{} {:02}:{:02}:{:02}", date_string(day), secs / 3600, secs / 60 % 60, secs % 60),
                mode: if i % 2 == 0 { "editor".into() } else { "editor+build".into() },
                pressure: if i % 20 == 0 { Level::Elevated } else { Level::Normal },
                ram_free_gb: 24.5,
                commit_free_gb: 48.25,
                ue_committed_gb: 36.0 + (i as f64 * 0.001),
                editor_committed_gb: 18.0 + (i as f64 * 10.0 / 3600.0 * 0.5),
                workers: (i % 16) as usize,
                groups: vec![
                    GroupRam { name: "Browsers".into(), ram_gb: 2.34 },
                    GroupRam { name: "Chat".into(), ram_gb: 0.78 },
                    GroupRam { name: "Agents".into(), ram_gb: 1.12 },
                    GroupRam { name: "Background".into(), ram_gb: 0.45 },
                ],
                editors: vec![(101, ft(T0 - 3600), 18.0 + (i as f64 * 10.0 / 3600.0 * 0.5))],
            };
            h.push(s);
        }
        assert_eq!(h.ring_len(), 8640);
        let bytes = h.ring_memory_bytes();
        assert!(bytes < 400 * 1024, "compact ring used {} bytes (>= 400 KB)", bytes);

        let latest = h.latest().unwrap();
        assert_eq!(latest.groups.len(), 4);
        assert_eq!(latest.groups[0].ram_gb, 2.34);
        assert_eq!(h.series_at(T0 + (day_samples - 1) * 10, Duration::from_secs(60)).len(), 7);
        assert_eq!(h.series_after(T0 + (day_samples - 3) * 10).unwrap().len(), 2);
        let g = h.editor_growth_gb_per_hour(101, ft(T0 - 3600)).unwrap();
        assert!((g - 0.5).abs() < 1e-4, "{}", g);
    }

    #[test]
    fn csv_batches_normal_samples_and_flushes_on_threshold_pressure_and_drop() {
        let dir = temp_dir();
        let make_normal = |unix: i64| {
            let mut s = sample(unix, &[]);
            s.pressure = Level::Normal;
            s
        };
        let date = make_normal(T0).date().to_string();
        let csv_path = dir.join(format!("{}.csv", date));

        {
            let mut h = History::new(dir.clone(), 7);
            // 5 Normal samples remain buffered (no CSV write yet).
            for i in 0..5 {
                h.push(make_normal(T0 + i * 10));
            }
            assert!(!csv_path.exists(), "5 normal samples should still be buffered");

            // 6th Normal sample triggers batch flush.
            h.push(make_normal(T0 + 50));
            assert!(csv_path.exists(), "6th normal sample should flush CSV batch");
            assert_eq!(load_day_from(&dir, &date, 0).unwrap().len(), 6);

            // 2 more Normal samples stay buffered until Drop.
            h.push(make_normal(T0 + 60));
            h.push(make_normal(T0 + 70));
            assert_eq!(load_day_from(&dir, &date, 0).unwrap().len(), 6);
        }
        // Drop flushed the remaining 2 rows.
        assert_eq!(load_day_from(&dir, &date, 0).unwrap().len(), 8);

        // Elevated pressure sample flushes immediately along with prior Normal sample.
        let mut h2 = History::new(dir.clone(), 7);
        h2.push(make_normal(T0 + 80));
        assert_eq!(load_day_from(&dir, &date, 0).unwrap().len(), 8);
        h2.push(sample(T0 + 90, &[])); // Level::Elevated
        assert_eq!(load_day_from(&dir, &date, 0).unwrap().len(), 10);
        drop(h2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_summary_computes_peaks_mins_durations_and_growth() {
        let empty = summarize_samples(&[]);
        assert_eq!(empty.sample_count, 0);
        assert_eq!(empty.window_start, None);

        let created = ft(T0 - 3600);
        let mut h = History::in_memory();
        let mut now = T0;
        for i in 0..=180 {
            let t = T0 + i * 10;
            now = t;
            let mut s = sample_c(t, &[(7, created, 20.0 + 6.0 * (i * 10) as f64 / 3600.0)]);
            s.pressure = if i < 60 {
                Level::Normal
            } else if i < 120 {
                Level::Elevated
            } else {
                Level::Critical
            };
            s.ram_free_gb = if i == 100 { 4.2 } else { 15.0 };
            s.commit_free_gb = if i == 120 { 8.5 } else { 30.0 };
            s.ue_committed_gb = if i == 150 { 52.0 } else { 35.0 };
            s.workers = if i == 90 { 12 } else { 2 };
            if i == 10 {
                s.groups[0].ram_gb = 6.5;
            }
            h.push(s);
        }

        let summary = h.session_summary_at(now, Duration::from_secs(3600));
        assert_eq!(summary.sample_count, 181);
        assert_eq!(summary.span_minutes, 30.0);
        assert_eq!(summary.window_hours, 0.5);
        assert_eq!(summary.peak_ue_committed_gb, 52.0);
        assert_eq!(summary.min_ram_free_gb, 4.2);
        assert_eq!(summary.min_commit_free_gb, 8.5);
        assert_eq!(summary.peak_workers, 12);
        assert_eq!(summary.normal_minutes, 10.0);
        assert_eq!(summary.elevated_minutes, 10.0);
        assert_eq!(summary.critical_minutes, 10.0);
        assert_eq!(summary.pressure_transitions, 2);
        assert_eq!(summary.groups[0].peak_gb, 6.5);
        assert_eq!(summary.groups[0].latest_gb, 2.5);
        let growth = summary.editor_growth_gb_per_hour.unwrap();
        assert!((growth - 6.0).abs() < 0.05, "{}", growth);

        // summarize_samples directly on a slice matches.
        let series = h.series_at(now, Duration::from_secs(3600));
        assert_eq!(summarize_samples(&series), summary);

        // Live session_summary(Duration) with current timestamps works too.
        let mut h_live = History::in_memory();
        let live_now = now_unix();
        h_live.push(sample_c(live_now - 60, &[]));
        h_live.push(sample_c(live_now, &[]));
        assert_eq!(h_live.session_summary(Duration::from_secs(3600)).sample_count, 2);
    }

    #[test]
    fn compact_ring_pre_compaction_avoids_midloop_panic_and_stale_reuse() {
        // Case 1: 511 churned editor keys (>24h) + sample with 2 new editors -> no OOB panic.
        let mut h1 = History::in_memory();
        for i in 0..511u32 {
            let t = T0 + (i as i64) * 1000;
            h1.push(sample_c(t, &[(10_000 + i, 1, 10.0)]));
        }
        let t_next1 = T0 + 511 * 1000;
        h1.push(sample_c(t_next1, &[(90_001, 1, 15.0), (90_002, 1, 25.0)]));
        let latest1 = h1.latest().expect("latest1");
        assert_eq!(latest1.editors, vec![(90_001, 1, 15.0), (90_002, 1, 25.0)]);

        // Case 2: 512 churned editor keys + sample reusing stale evicted key (20000, 1) + new key (99999, 1).
        let mut h2 = History::in_memory();
        for i in 0..512u32 {
            let t = T0 + (i as i64) * 1000;
            h2.push(sample_c(t, &[(20_000 + i, 1, 10.0)]));
        }
        let t_next2 = T0 + 512 * 1000;
        h2.push(sample_c(t_next2, &[(20_000, 1, 12.0), (99_999, 1, 34.0)]));
        let latest2 = h2.latest().expect("latest2");
        assert_eq!(latest2.editors, vec![(20_000, 1, 12.0), (99_999, 1, 34.0)]);
    }

    #[test]
    fn compact_ring_300_live_modes_and_schemas_within_24h() {
        let mut h = History::in_memory();
        let start = T0;
        for i in 0..300usize {
            let mut s = sample(start + (i as i64) * 10, &[]);
            s.mode = format!("mode_{}", i);
            s.groups = vec![GroupRam { name: format!("Grp_{}", i), ram_gb: 1.25 }];
            h.push(s);
        }
        let all = h.series_at(start + 3000, Duration::from_secs(4000));
        assert_eq!(all.len(), 300);
        assert_eq!(all[240].mode, "mode_240");
        assert_eq!(all[240].groups[0].name, "Grp_240");
        assert_eq!(all[240].groups[0].ram_gb, 1.25);
        assert_eq!(all[299].mode, "mode_299");
        assert_eq!(all[299].groups[0].name, "Grp_299");
        assert_eq!(all[299].groups[0].ram_gb, 1.25);
    }

    #[test]
    fn flush_csv_header_recovery_and_large_ooo_delta() {
        // Header recovery when first flush fails (csv path blocked by directory).
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let mut s1 = sample(T0, &[]);
        s1.pressure = Level::Elevated; // forces immediate flush
        let date = s1.date().to_string();
        let csv_path = dir.join(format!("{}.csv", date));
        fs::create_dir_all(&csv_path).unwrap(); // block OpenOptions::open

        let mut h = History::new(dir.clone(), 7);
        h.push(s1);
        // Remove blocking directory and push another sample: header must be written.
        fs::remove_dir(&csv_path).unwrap();
        let mut s2 = sample(T0 + 10, &[]);
        s2.pressure = Level::Elevated;
        h.push(s2);
        drop(h);

        let content = fs::read_to_string(&csv_path).unwrap();
        assert!(content.lines().next().unwrap().starts_with("time,mode,pressure,"), "missing header: {}", content);
        assert_eq!(load_day_from(&dir, &date, 0).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);

        // Out-of-order timestamp > u32::MAX seconds behind base_unix does not corrupt existing samples.
        let mut hr = History::in_memory();
        hr.push(sample(2_000_000_000, &[]));
        hr.push(sample(-5_000_000_000, &[]));
        let all = hr.series_at(2_000_000_100, Duration::from_secs(u64::MAX / 2));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].unix, 2_000_000_000);
        assert_eq!(all[1].unix, -5_000_000_000);
    }

    #[test]
    fn t5_compact_ring_overflow_editors_workers_tz_and_compact_tables() {
        let mut h = History::in_memory();

        // 1. Push samples that exercise >15 distinct editors (extra_editors), workers > 1023
        //    (ov.workers), and mid-ring timezone shift (ov.tz_secs).
        let editors_20: Vec<(u32, u64, f64)> = (0..20u32).map(|i| (5000 + i, 100 + i as u64, 1.5 + i as f64 * 0.1)).collect();
        let mut s1 = sample_c(T0, &editors_20);
        s1.workers = 2500;
        s1.time = format_local_time(T0 + 3600); // +01:00 tz
        h.push(s1.clone());

        let mut s2 = sample_c(T0 + 60, &[(9999, 777, 12.25)]);
        s2.workers = 1234;
        s2.time = format_local_time(T0 + 60 - 18_000); // -05:00 mid-stream tz shift
        h.push(s2.clone());

        // 2. Evict older entries with unused schema/mode/editor/extra_groups, then run compact_intern_tables.
        let old_base = T0 + 120;
        for i in 0..30u32 {
            let mut s = sample_c(old_base + (i as i64) * 10, &[(30_000 + i, 55, 2.0)]);
            s.mode = format!("old_mode_{}", i);
            s.groups = vec![GroupRam { name: format!("OldGrp_{}", i), ram_gb: 1.0 }];
            h.push(s);
        }
        // Push one sample >= 24h after T0 + 120 + 290 so the first batch is evicted, leaving only fresh samples.
        let far = old_base + DAY_SECS + 1000;
        let mut keep_sample = sample_c(far, &[(44_444, 888, 9.5)]);
        keep_sample.mode = "steady_mode".into();
        keep_sample.groups = vec![GroupRam { name: "SteadyGrp".into(), ram_gb: 3.25 }];
        keep_sample.workers = 2048;
        keep_sample.time = format_local_time(far + 7200);
        h.push(keep_sample.clone());

        h.ring.compact_intern_tables();
        let latest = h.latest().unwrap();
        assert_eq!(latest.mode, "steady_mode");
        assert_eq!(latest.groups, vec![GroupRam { name: "SteadyGrp".into(), ram_gb: 3.25 }]);
        assert_eq!(latest.editors, vec![(44_444, 888, 9.5)]);
        assert_eq!(latest.workers, 2048);
        assert_eq!(latest.time, keep_sample.time);

        // Verify direct roundtrip of >15 editors + workers>1023 + tz shift when still in 24h window.
        let mut h_ov = History::in_memory();
        h_ov.push(s1.clone());
        h_ov.push(s2.clone());
        let got = h_ov.series_at(T0 + 60, Duration::from_secs(3600));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].editors.len(), 20);
        assert_eq!(got[0].workers, 2500);
        assert_eq!(got[0].time, s1.time);
        assert_eq!(got[1].workers, 1234);
        assert_eq!(got[1].time, s2.time);
    }

    #[test]
    fn t5_history_csv_midnight_rollover_flushes_pending_date() {
        let dir = temp_dir();
        let mut h = History::new(dir.clone(), 7);

        // Day 1: Normal sample stays buffered in csv_buf / pending_date.
        let mut d1 = sample(T0, &[]);
        d1.pressure = Level::Normal;
        d1.time = "2026-11-20 23:59:50".into();
        h.push(d1);
        let f1 = dir.join("2026-11-20.csv");
        assert!(!f1.exists(), "single normal sample should remain buffered before rollover");

        // Day 2: Pushing a sample for 2026-11-21 must flush 2026-11-20.csv immediately.
        let mut d2 = sample(T0 + 20, &[]);
        d2.pressure = Level::Normal;
        d2.time = "2026-11-21 00:00:10".into();
        h.push(d2);

        assert!(f1.is_file(), "midnight rollover must flush 2026-11-20.csv to disk");
        let day1_rows = load_day_from(&dir, "2026-11-20", 0).unwrap();
        assert_eq!(day1_rows.len(), 1);
        assert_eq!(day1_rows[0].time, "2026-11-20 23:59:50");

        drop(h);
        let day2_rows = load_day_from(&dir, "2026-11-21", 0).unwrap();
        assert_eq!(day2_rows.len(), 1);
        assert_eq!(day2_rows[0].time, "2026-11-21 00:00:10");
        let _ = fs::remove_dir_all(&dir);
    }
}

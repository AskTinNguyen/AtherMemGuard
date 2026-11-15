//! Disk headroom and Dev Drive health (proposal sections 4 A-C and 5).
//!
//! The pure parts are here and unit-tested: the volume model, levels from the space available
//! to the caller with hysteresis, roles, the Dev Drive findings and their wording, the `wait`
//! disk gate, and the parsers for `fsutil devdrv query` and the Defender probe. The Win32 reads
//! are in `disk_win`; scanning and the checkup are in `disk_scan`.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, DiskSettings};
use crate::disk_win::{self, DefenderFacts, VolumeFacts};
use crate::engine::Level;

const GIB: f64 = (1u64 << 30) as f64;
/// A level is only left when free space is this far past its threshold (no flapping).
pub const HYSTERESIS_BYTES: u64 = 5 << 30;
/// Defender facts change a few times a year: read them at most once a day.
pub const DEFENDER_CACHE: Duration = Duration::from_secs(24 * 3600);

fn gb1(bytes: u64) -> f64 {
    (bytes as f64 / GIB * 10.0).round() / 10.0
}
fn gb2(bytes: u64) -> f64 {
    (bytes as f64 / GIB * 100.0).round() / 100.0
}

// ---------------------------------------------------------------- levels

/// Bytes below which a level applies: the smaller of the GB and the percent figure, so a
/// 150 GB rule is not swamped by 10% of an 8 TB disk.
pub fn threshold(gb: f64, pct: f64, total: u64) -> u64 {
    let by_gb = (gb.max(0.0) * GIB) as u64;
    let by_pct = (total as f64 * pct.max(0.0) / 100.0) as u64;
    by_gb.min(by_pct)
}

/// The level for `avail` bytes free (available to the caller) on a volume of `total` bytes.
pub fn level_for(avail: u64, total: u64, s: &DiskSettings) -> Level {
    if avail < threshold(s.critical_free_gb, s.critical_free_pct, total) {
        Level::Critical
    } else if avail < threshold(s.elevated_free_gb, s.elevated_free_pct, total) {
        Level::Elevated
    } else {
        Level::Normal
    }
}

/// The level with 5 GB hysteresis: getting worse applies at once, getting better only once
/// free space is 5 GB past the threshold of the level being left.
pub fn level_with_hysteresis(prev: Option<Level>, avail: u64, total: u64, s: &DiskSettings) -> Level {
    let raw = level_for(avail, total, s);
    let Some(prev) = prev else { return raw };
    if raw >= prev {
        return raw;
    }
    let crit = threshold(s.critical_free_gb, s.critical_free_pct, total);
    let elev = threshold(s.elevated_free_gb, s.elevated_free_pct, total);
    match prev {
        Level::Critical if avail < crit + HYSTERESIS_BYTES => Level::Critical,
        Level::Critical | Level::Elevated if avail < elev + HYSTERESIS_BYTES => Level::Elevated.max(raw),
        _ => raw,
    }
}

// ---------------------------------------------------------------- volume model

/// One volume, as `memguard disk --json` reports it (proposal section 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    /// `\\?\Volume{...}\`: one row per volume, however it is reached.
    pub volume_id: String,
    pub paths: Vec<String>,
    pub label: String,
    pub fs: String,
    pub dev_drive: bool,
    /// `None` when it could not be read.
    pub trusted: Option<bool>,
    pub block_cloning: bool,
    /// "partition", "vhd", "network" or "unknown".
    pub backing: String,
    /// The file a VHD-backed volume lives in, when known.
    pub backed_by: Option<String>,
    pub total_gb: f64,
    pub free_gb: f64,
    pub free_basis: String,
    pub reserved_gb: Option<f64>,
    pub free_pct: f64,
    pub level: Level,
    /// Needs the disk history (a later version): always null for now.
    pub trend_gb_per_day: Option<f64>,
    /// "dev_drive", "system", "pagefile", "projects", "ddc", "zen", "temp", "watched".
    pub roles: Vec<String>,
    /// Plain-words notes, such as ReFS returning deleted space late.
    pub notes: Vec<String>,
    /// Bytes available to the caller and the total, for the arithmetic (not in the JSON).
    #[serde(skip)]
    pub avail: u64,
    #[serde(skip)]
    pub total: u64,
    /// Rotating disk (seek penalty): never suggested as a cache destination.
    #[serde(skip)]
    pub fixed_local: bool,
}

impl Volume {
    /// "D:" (the first drive letter), else the first mount path, else the GUID path.
    pub fn name(&self) -> String {
        self.paths
            .iter()
            .find(|p| p.len() <= 3 && p.as_bytes().get(1) == Some(&b':'))
            .map(|p| p.trim_end_matches('\\').to_string())
            .or_else(|| self.paths.first().cloned())
            .unwrap_or_else(|| self.volume_id.clone())
    }

    pub fn letter_path(&self) -> String {
        format!("{}\\", self.name().trim_end_matches('\\'))
    }
}

/// What the roles of a volume follow from: where the system, page files, projects, the
/// Unreal caches and TEMP live (each as a volume GUID path).
#[derive(Debug, Clone, Default)]
pub struct RoleContext {
    pub system: Option<String>,
    pub pagefile: Vec<String>,
    pub projects: Vec<String>,
    pub ddc: Option<String>,
    pub zen: Option<String>,
    pub temp: Option<String>,
    /// Volumes named in `[disk] watch`.
    pub watched: Vec<String>,
}

pub fn roles(guid: &str, dev_drive: bool, ctx: &RoleContext) -> Vec<String> {
    let is = |g: &Option<String>| g.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(guid));
    let has = |v: &[String]| v.iter().any(|g| g.eq_ignore_ascii_case(guid));
    let mut r = Vec::new();
    if dev_drive {
        r.push("dev_drive");
    }
    if is(&ctx.system) {
        r.push("system");
    }
    if has(&ctx.pagefile) {
        r.push("pagefile");
    }
    if has(&ctx.projects) {
        r.push("projects");
    }
    if is(&ctx.ddc) {
        r.push("ddc");
    }
    if is(&ctx.zen) {
        r.push("zen");
    }
    if is(&ctx.temp) {
        r.push("temp");
    }
    if has(&ctx.watched) {
        r.push("watched");
    }
    r.into_iter().map(String::from).collect()
}

/// Turn the raw facts into the model. Pure.
pub fn volume_from(f: &VolumeFacts, ctx: &RoleContext, s: &DiskSettings, prev: Option<Level>) -> Volume {
    let dev = f.dev_volume.unwrap_or(false);
    let pct = if f.total == 0 { 0.0 } else { (f.avail as f64 / f.total as f64 * 1000.0).round() / 10.0 };
    let backing = match (f.drive_type, f.bus_type) {
        (4, _) => "network",
        (_, Some(b)) if b == disk_win::BUS_FILE_BACKED_VIRTUAL => "vhd",
        (_, Some(_)) => "partition",
        _ => "unknown",
    };
    Volume {
        volume_id: f.guid.clone(),
        paths: f.paths.clone(),
        label: f.label.clone(),
        fs: f.fs.clone(),
        dev_drive: dev,
        trusted: if dev { f.trusted } else { None },
        block_cloning: f.fs_flags & disk_win::FS_BLOCK_REFCOUNTING != 0,
        backing: backing.into(),
        backed_by: None,
        total_gb: gb2(f.total),
        free_gb: gb2(f.avail),
        free_basis: "available_to_caller".into(),
        reserved_gb: f.reserved.map(gb2),
        free_pct: pct,
        level: level_with_hysteresis(prev, f.avail, f.total, s),
        trend_gb_per_day: None,
        roles: roles(&f.guid, dev, ctx),
        notes: if f.fs.eq_ignore_ascii_case("ReFS") { vec![crate::disk_scan::REFS_DELAY_NOTE.to_string()] } else { Vec::new() },
        avail: f.avail,
        total: f.total,
        fixed_local: f.drive_type == 3 && f.total > 0 && f.seek_penalty != Some(true),
    }
}

/// Is this volume watched? `["auto"]` = any role; otherwise only the volumes `watch` names.
pub fn is_watched(v: &Volume, s: &DiskSettings) -> bool {
    let auto = s.watch.iter().any(|w| w.eq_ignore_ascii_case("auto"));
    (auto && v.roles.iter().any(|r| r != "watched")) || v.roles.iter().any(|r| r == "watched")
}

/// The worst level among watched volumes.
pub fn overall_level(vols: &[Volume], s: &DiskSettings) -> Level {
    vols.iter().filter(|v| is_watched(v, s)).map(|v| v.level).max().unwrap_or(Level::Normal)
}

/// "D: Dev Drive · 370 GB free" for the flyout: the watched volume that needs attention most
/// (worst level, then least free), the Dev Drive when everything is fine.
pub fn flyout_line(vols: &[Volume], s: &DiskSettings) -> Option<String> {
    let watched: Vec<&Volume> = vols.iter().filter(|v| is_watched(v, s)).collect();
    let worst = watched.iter().copied().max_by(|a, b| a.level.cmp(&b.level).then(b.free_pct.total_cmp(&a.free_pct)))?;
    let v = if worst.level == Level::Normal { watched.iter().copied().find(|v| v.dev_drive).unwrap_or(worst) } else { worst };
    let kind = if v.dev_drive {
        " Dev Drive"
    } else if v.roles.iter().any(|r| r == "system") {
        " system drive"
    } else {
        ""
    };
    let state = match v.level {
        Level::Normal => "",
        Level::Elevated => " · getting full",
        Level::Critical => " · almost full",
    };
    Some(format!("{}{} · {:.0} GB free{}", v.name(), kind, v.free_gb, state))
}

// ---------------------------------------------------------------- toasts

/// What a level change should say: (title, body), or `None` when nothing is worth a toast.
/// `cook_gb` is the disk a cook needs (`[prime] cook.disk_gb`).
pub fn level_change_words(v: &Volume, from: Level, to: Level, cook_gb: Option<f64>) -> Option<(String, String)> {
    let name = v.name();
    let free = format!("{:.0} GB", v.free_gb.floor());
    if to > from {
        let system = v.roles.iter().any(|r| r == "system" || r == "pagefile");
        if system {
            return Some((
                format!("{} is low on space", name),
                format!("{} has {} free. Windows may not be able to grow the page file, and Unreal can run out of memory.", name, free),
            ));
        }
        let title = if v.dev_drive {
            "Your Dev Drive is almost full".to_string()
        } else if to == Level::Critical {
            format!("{} is almost full", name)
        } else {
            format!("{} is getting full", name)
        };
        let need = match cook_gb {
            Some(c) if c > 0.0 && v.roles.iter().any(|r| r == "projects" || r == "dev_drive") => {
                format!(", and a cook usually needs {:.0} GB", c)
            }
            _ => String::new(),
        };
        return Some((title, format!("{} has {} free{}. Run a disk checkup to see what can go.", name, free, need)));
    }
    if to < from && to == Level::Normal {
        return Some((format!("{} has room again", name), format!("{} free on {}.", free, name)));
    }
    None
}

/// Remembers each watched volume's level, so a toast fires once per level change.
#[derive(Debug, Default, Clone)]
pub struct DiskWatch {
    levels: std::collections::HashMap<String, Level>,
}

impl DiskWatch {
    pub fn previous(&self, volume_id: &str) -> Option<Level> {
        self.levels.get(volume_id).copied()
    }

    /// Record this poll; returns (volume, from, to) for every watched volume whose level changed.
    /// The first poll is a baseline, except that a volume already low is reported once.
    pub fn update(&mut self, vols: &[Volume], s: &DiskSettings) -> Vec<(Volume, Level, Level)> {
        let mut out = Vec::new();
        for v in vols.iter().filter(|v| is_watched(v, s)) {
            let prev = self.levels.insert(v.volume_id.clone(), v.level);
            let from = prev.unwrap_or(Level::Normal);
            if from != v.level {
                out.push((v.clone(), from, v.level));
            }
        }
        out
    }
}

// ---------------------------------------------------------------- reading this PC

/// Where the Unreal caches are: the legacy file-system DDC (or `UE-LocalDataCachePath`) and Zen.
pub fn ue_cache_dirs() -> (Option<PathBuf>, Option<PathBuf>) {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let ddc = std::env::var_os("UE-LocalDataCachePath")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty() && !p.to_string_lossy().eq_ignore_ascii_case("None"))
        .or_else(|| local.as_ref().map(|l| l.join(r"UnrealEngine\Common\DerivedDataCache")));
    let zen = local.map(|l| l.join(r"UnrealEngine\Common\Zen"));
    (ddc, zen)
}

/// The nearest existing folder of `p` (a cache folder may not exist yet; its volume does).
fn existing(p: &std::path::Path) -> Option<String> {
    p.ancestors().find(|a| a.exists()).map(|a| a.display().to_string())
}

fn guid_of(path: &str) -> Option<String> {
    disk_win::volume_of(path).map(|(_, g)| g)
}

/// Read every volume and its roles. Cheap: no folder is walked except the top two levels of
/// Dev Drives, to see whether they hold Unreal projects.
pub fn read_volumes(cfg: &Config, prev: &DiskWatch) -> Vec<Volume> {
    let s = cfg.disk();
    let facts = disk_win::volumes();
    let mut ctx = RoleContext::default();
    let sysdrive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    ctx.system = guid_of(&format!("{}\\", sysdrive));
    ctx.pagefile = disk_win::pagefile_drives().into_iter().filter_map(|c| guid_of(&format!("{}:\\", c))).collect();
    let (ddc, zen) = ue_cache_dirs();
    ctx.ddc = ddc.as_deref().and_then(existing).and_then(|p| guid_of(&p));
    ctx.zen = zen.as_deref().and_then(existing).and_then(|p| guid_of(&p));
    ctx.temp = existing(&std::env::temp_dir()).and_then(|p| guid_of(&p));
    for p in &s.project_dirs {
        if let Some(g) = existing(std::path::Path::new(p)).and_then(|p| guid_of(&p)) {
            ctx.projects.push(g);
        }
    }
    for f in facts.iter().filter(|f| f.dev_volume == Some(true)) {
        if let Some(root) = f.paths.first() {
            if crate::disk_scan::has_projects_near(root) {
                ctx.projects.push(f.guid.clone());
            }
        }
    }
    for w in s.watch.iter().filter(|w| !w.eq_ignore_ascii_case("auto")) {
        let p = if w.len() == 2 && w.ends_with(':') { format!("{}\\", w) } else { w.clone() };
        if let Some(g) = existing(std::path::Path::new(&p)).and_then(|p| guid_of(&p)) {
            ctx.watched.push(g);
        }
    }
    facts
        .iter()
        .filter(|f| f.total > 0 && !f.paths.is_empty())
        .map(|f| volume_from(f, &ctx, &s, prev.previous(&f.guid)))
        .collect()
}

// ---------------------------------------------------------------- Dev Drive health

/// The machine-wide Dev Drive policy and the Defender facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevDriveState {
    /// Dev Drives are enabled on this PC (`fsutil devdrv query`); null = unknown.
    pub enabled: Option<bool>,
    /// "antivirus_allowed", "antivirus_disallowed" or "unknown".
    pub av_filters: String,
    /// "on", "off" or "unknown".
    pub defender_performance_mode: String,
    pub realtime_protection: Option<bool>,
    /// Other antivirus products registered with Windows Security; null = unknown.
    pub third_party_av: Option<u32>,
    /// When the Defender facts were read (they are cached for a day).
    pub defender_checked: String,
}

/// `fsutil devdrv query` output, matched against known English phrases only. Anything else
/// is "unknown", never a guess: (enabled, av_filters).
pub fn parse_fsutil_devdrv(text: &str) -> (Option<bool>, &'static str) {
    let t = text.to_ascii_lowercase();
    let enabled = if t.contains("developer volumes are not enabled") || t.contains("developer volumes are disabled") {
        Some(false)
    } else if t.contains("developer volumes are enabled") {
        Some(true)
    } else {
        None
    };
    let av = if t.contains("not protected by antivirus filter") || t.contains("are not protected by antivirus") {
        "antivirus_disallowed"
    } else if t.contains("protected by antivirus filter") {
        "antivirus_allowed"
    } else {
        "unknown"
    };
    (enabled, av)
}

/// The Defender probe's `key=number` lines. Anything that isn't a number is ignored.
pub fn parse_defender(text: &str) -> DefenderFacts {
    let mut d = DefenderFacts::default();
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else { continue };
        let Ok(n) = v.trim().parse::<u32>() else { continue };
        match k.trim() {
            "pm" => d.performance_mode_status = Some(n),
            "rt" => d.realtime_enabled = Some(n != 0),
            "am" => d.defender_active = Some(n != 0),
            "av" => d.third_party_av = Some(n),
            _ => {}
        }
    }
    d
}

/// "on" / "off" / "unknown" from `PerformanceModeStatus` (0 enabled, 1 disabled on this
/// PC's cmdlet enum; anything else is unknown).
pub fn performance_mode_words(status: Option<u32>) -> &'static str {
    match status {
        Some(0) => "on",
        Some(1) => "off",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub needs_admin: bool,
    pub command: String,
    pub check: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskFinding {
    /// "dev:not_trusted", "dev:perf_mode_off", "dev:av_disallowed", "dev:caches_on_c",
    /// "dev:vhd_host_low", "dev:third_party_av".
    pub id: String,
    /// "problem", "caution" or "info".
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub fix: Option<Fix>,
}

fn finding(id: &str, severity: &str, title: String, detail: String, fix: Option<Fix>) -> DiskFinding {
    DiskFinding { id: id.into(), severity: severity.into(), title, detail, fix }
}

/// Unreal and toolchain caches on the system drive, as sized by the last disk checkup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachesOnSystem {
    pub gb: f64,
    /// When they were sized.
    pub measured: String,
}

/// The Dev Drive findings (proposal 4 B). Pure.
pub fn dev_findings(vols: &[Volume], dev: &DevDriveState, caches: Option<&CachesOnSystem>, s: &DiskSettings) -> Vec<DiskFinding> {
    let mut out = Vec::new();
    let devs: Vec<&Volume> = vols.iter().filter(|v| v.dev_drive).collect();
    for v in &devs {
        if v.trusted == Some(false) {
            let n = v.name();
            out.push(finding(
                "dev:not_trusted",
                "problem",
                format!("{} is a Dev Drive but isn't trusted", n),
                format!("{} is a Dev Drive but isn't trusted, so Defender scans it the slow way.", n),
                Some(Fix {
                    needs_admin: true,
                    command: format!("fsutil devdrv trust {}", n),
                    check: format!("fsutil devdrv query {} (as administrator) says the volume is trusted", n),
                }),
            ));
        }
    }
    let any_trusted = devs.iter().any(|v| v.trusted == Some(true));
    if any_trusted && dev.defender_performance_mode == "off" && dev.realtime_protection != Some(false) {
        out.push(finding(
            "dev:perf_mode_off",
            "caution",
            "Defender's performance mode is off".into(),
            "Defender's performance mode is off, so files on your Dev Drive are scanned before they open.".into(),
            Some(Fix {
                needs_admin: true,
                command: "Set-MpPreference -PerformanceModeStatus Enabled".into(),
                check: "(Get-MpPreference).PerformanceModeStatus reads 0".into(),
            }),
        ));
    }
    if !devs.is_empty() && dev.av_filters == "antivirus_disallowed" {
        out.push(finding(
            "dev:av_disallowed",
            "info",
            "Antivirus filters are switched off on Dev Drives".into(),
            "Antivirus filters are switched off on Dev Drives. That's fast, but nothing scans these files.".into(),
            None,
        ));
    }
    if let Some(c) = caches.filter(|c| c.gb >= 20.0) {
        let sys = vols.iter().find(|v| v.roles.iter().any(|r| r == "system"));
        let sys_name = sys.map(|v| v.name()).unwrap_or_else(|| "C:".into());
        let need = (c.gb * GIB) as u64;
        // A destination only if it would still be above `elevated` afterwards.
        let dest = vols
            .iter()
            .filter(|v| sys.map_or(true, |s| s.volume_id != v.volume_id) && v.fixed_local && v.backing != "network")
            .filter(|v| v.avail > need && level_for(v.avail - need, v.total, s) == Level::Normal)
            .max_by_key(|v| v.avail);
        let tight: Vec<String> = devs
            .iter()
            .filter(|d| !(d.avail > need && level_for(d.avail - need, d.total, s) == Level::Normal))
            .map(|d| d.name())
            .collect();
        let mut detail = format!("About {:.0} GB of Unreal and toolchain caches are on {}.", c.gb, sys_name);
        if !tight.is_empty() {
            detail.push_str(&format!(" {} doesn't have room for them and still enough free for a cook.", tight.join(" and ")));
        }
        match dest {
            Some(d) => detail.push_str(&format!(" {} has room ({:.0} GB free).", d.name(), d.free_gb)),
            None => detail.push_str(" No other drive has room for them and enough left over, so leave them where they are for now."),
        }
        if !devs.is_empty() || dest.is_some() {
            out.push(finding("dev:caches_on_c", "info", format!("About {:.0} GB of caches are on {}", c.gb, sys_name), detail, None));
        }
    }
    for v in devs.iter().filter(|v| v.backing == "vhd") {
        out.push(finding(
            "dev:vhd_host_low",
            "info",
            format!("{} lives in a virtual disk file", v.name()),
            format!(
                "{} is a Dev Drive inside a VHD file. Ather MemGuard can't see how much room that file may still grow into on its host drive; check the host has at least as much free as {} may still grow.",
                v.name(),
                v.name()
            ),
            None,
        ));
    }
    if !devs.is_empty() && dev.third_party_av.is_some_and(|n| n > 0) {
        out.push(finding(
            "dev:third_party_av",
            "info",
            "Another antivirus is installed".into(),
            "Performance mode only applies to Microsoft Defender. Ask your antivirus vendor how it treats Dev Drives.".into(),
            None,
        ));
    }
    out
}

/// A VHD-backed Dev Drive whose host is tight: file size vs maximum size vs host free. Pure;
/// used once the backing file can be read (not on this PC: no VHD).
pub fn vhd_host_low(file_bytes: u64, max_bytes: u64, host_free: u64) -> bool {
    max_bytes > file_bytes && max_bytes - file_bytes > host_free
}

// ---------------------------------------------------------------- the disk report

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskReport {
    pub level: Level,
    pub volumes: Vec<Volume>,
    pub dev_drive: DevDriveState,
    pub findings: Vec<DiskFinding>,
    pub advice: Vec<String>,
    /// The latest cleanups: predicted at once, measured once the drive has been watched.
    #[serde(default)]
    pub recent_cleanups: Vec<serde_json::Value>,
}

fn defender_cache_file() -> PathBuf {
    crate::paths::data_dir().join("disk-defender.json")
}

#[derive(Serialize, Deserialize)]
struct DefenderCache {
    unix: u64,
    time: String,
    facts: DefenderFacts,
    fsutil: Option<String>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The Dev Drive policy and Defender facts, from the day's cache unless `refresh`.
pub fn dev_drive_state(refresh: bool) -> DevDriveState {
    let path = defender_cache_file();
    let cached: Option<DefenderCache> = if refresh || cfg!(test) {
        None
    } else {
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok())
    };
    let c = match cached.filter(|c| unix_now().saturating_sub(c.unix) < DEFENDER_CACHE.as_secs()) {
        Some(c) => c,
        None => {
            let c = DefenderCache { unix: unix_now(), time: crate::win::local_time_string(), facts: disk_win::probe_defender(), fsutil: disk_win::fsutil_devdrv_query() };
            if crate::disk_scan::cache_writes_allowed() {
                let _ = std::fs::create_dir_all(crate::paths::data_dir());
                let _ = std::fs::write(&path, serde_json::to_string_pretty(&c).unwrap_or_default());
            }
            c
        }
    };
    let (enabled, av) = parse_fsutil_devdrv(c.fsutil.as_deref().unwrap_or(""));
    let defender_active = c.facts.defender_active != Some(false);
    DevDriveState {
        enabled,
        av_filters: av.into(),
        defender_performance_mode: if defender_active { performance_mode_words(c.facts.performance_mode_status) } else { "unknown" }.into(),
        realtime_protection: c.facts.realtime_enabled,
        third_party_av: c.facts.third_party_av,
        defender_checked: c.time,
    }
}

/// `memguard disk`: volumes, levels and Dev Drive health. Read-only; no folder is walked
/// (the cache size for `dev:caches_on_c` comes from the last disk checkup).
pub fn report(cfg: &Config, refresh: bool) -> DiskReport {
    let s = cfg.disk();
    let vols = read_volumes(cfg, &DiskWatch::default());
    let dev = dev_drive_state(refresh);
    let caches = crate::disk_scan::cached_system_caches();
    let findings = if s.check_dev_drive { dev_findings(&vols, &dev, caches.as_ref(), &s) } else { Vec::new() };
    let mut advice = Vec::new();
    for v in vols.iter().filter(|v| is_watched(v, &s) && v.level > Level::Normal) {
        advice.push(match level_change_words(v, Level::Normal, v.level, cfg.prime.cook.disk_gb) {
            Some((t, b)) => format!("{}. {}", t, b),
            None => format!("{} is low on space.", v.name()),
        });
    }
    if caches.is_none() && s.check_dev_drive && vols.iter().any(|v| v.dev_drive) {
        advice.push("Run `memguard disk checkup` once to size the caches on the system drive.".into());
    }
    let recent_cleanups = crate::disk_clean::recent_cleanups(&crate::disk_clean::DiskFiles::default(), 5);
    DiskReport { level: overall_level(&vols, &s), volumes: vols, dev_drive: dev, findings, advice, recent_cleanups }
}

/// `memguard disk` as text.
pub fn render_text(r: &DiskReport) -> String {
    let mut s = String::new();
    for v in &r.volumes {
        let mut kind = Vec::new();
        if v.dev_drive {
            kind.push(if v.trusted == Some(false) { "Dev Drive, not trusted" } else { "Dev Drive" });
        }
        for role in &v.roles {
            match role.as_str() {
                "system" => kind.push("system"),
                "pagefile" => kind.push("page file"),
                "projects" => kind.push("projects"),
                "ddc" => kind.push("Unreal DDC"),
                "zen" => kind.push("Zen"),
                "temp" => kind.push("TEMP"),
                _ => {}
            }
        }
        let level = match v.level {
            Level::Normal => "",
            Level::Elevated => "   getting full",
            Level::Critical => "   ALMOST FULL",
        };
        s.push_str(&format!(
            "{:<4} {:<5} {:>9.1} GB free of {:>8.1} GB ({:>4.1}%)  {}{}\n",
            v.name(),
            v.fs,
            v.free_gb,
            v.total_gb,
            v.free_pct,
            if kind.is_empty() { String::new() } else { kind.join(", ") },
            level
        ));
        if v.fs.eq_ignore_ascii_case("ReFS") {
            s.push_str(&format!("     {}\n", crate::disk_scan::REFS_DELAY_NOTE));
        }
    }
    let d = &r.dev_drive;
    s.push_str(&format!(
        "\nDev Drives: {}. Antivirus filters: {}. Defender performance mode: {}.\n",
        match d.enabled {
            Some(true) => "enabled",
            Some(false) => "not enabled",
            None => "unknown",
        },
        match d.av_filters.as_str() {
            "antivirus_allowed" => "on",
            "antivirus_disallowed" => "off",
            _ => "unknown",
        },
        d.defender_performance_mode
    ));
    for f in &r.findings {
        s.push_str(&format!("\n{}\n  {}\n", f.title, f.detail));
        if let Some(fix) = &f.fix {
            s.push_str(&format!("  Fix{}: {}\n  Check: {}\n", if fix.needs_admin { " (as administrator)" } else { "" }, fix.command, fix.check));
        }
    }
    for a in &r.advice {
        s.push_str(&format!("- {}\n", a));
    }
    if !r.recent_cleanups.is_empty() {
        s.push_str("\nRecent cleanups:\n");
        for c in &r.recent_cleanups {
            let other = c["other_activity"] == true;
            let measured = match c["measured_free_space_rise_gb"].as_f64() {
                Some(m) if m > 0.0 => format!(
                    "free space rose by {}{:.1} GB in the {} min after it (measured; other activity on the drive can add to or hide this){}",
                    if other { "about " } else { "" },
                    m,
                    c["measured_watched_sec"].as_u64().unwrap_or(0).div_ceil(60).max(1),
                    if other { ", other writes made it unclear" } else { "" }
                ),
                Some(_) => "free space didn't rise while it was watched (measured)".to_string(),
                None if c["state"] == "couldnt_measure" => "couldn't measure the free space".to_string(),
                None if c["state"] == "watching" => "being measured".to_string(),
                None => "not measured".to_string(),
            };
            s.push_str(&format!(
                "  {} {}: predicted {:.1} GB, {}\n",
                c["time"].as_str().unwrap_or(""),
                c["volume"].as_str().unwrap_or(""),
                c["predicted_freed_gb"].as_f64().unwrap_or(0.0),
                measured
            ));
        }
    }
    s
}

/// The small `disk` block of `memguard_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskBrief {
    pub level: Level,
    pub volumes: Vec<BriefVolume>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefVolume {
    pub volume: String,
    pub free_gb: f64,
    pub free_pct: f64,
    pub level: Level,
    pub roles: Vec<String>,
}

pub fn brief(vols: &[Volume], s: &DiskSettings) -> DiskBrief {
    DiskBrief {
        level: overall_level(vols, s),
        volumes: vols
            .iter()
            .filter(|v| is_watched(v, s))
            .map(|v| BriefVolume { volume: v.name(), free_gb: (v.free_gb * 10.0).round() / 10.0, free_pct: v.free_pct, level: v.level, roles: v.roles.clone() })
            .collect(),
    }
}

/// The flyout's disk line from a status `disk` block: the watched drive that needs attention
/// most (the Dev Drive first among equals), the Dev Drive when all are fine.
/// "D: Dev Drive · 370 GB free". Other drives that need attention are counted, not hidden.
pub fn brief_line(b: &DiskBrief) -> Option<(String, Level)> {
    let dev = |v: &BriefVolume| v.roles.iter().any(|r| r == "dev_drive");
    let worst = b.volumes.iter().max_by(|a, c| a.level.cmp(&c.level).then(dev(a).cmp(&dev(c))).then(c.free_pct.total_cmp(&a.free_pct)))?;
    let v = if worst.level == Level::Normal { b.volumes.iter().find(|v| v.roles.iter().any(|r| r == "dev_drive")).unwrap_or(worst) } else { worst };
    let kind = if v.roles.iter().any(|r| r == "dev_drive") {
        " Dev Drive"
    } else if v.roles.iter().any(|r| r == "system") {
        " system drive"
    } else {
        ""
    };
    let state = match v.level {
        Level::Normal => "",
        Level::Elevated => " · getting full",
        Level::Critical => " · almost full",
    };
    let others = b.volumes.iter().filter(|o| o.level != Level::Normal && o.volume != v.volume).count();
    let more = match others {
        0 => String::new(),
        1 => " · 1 more drive low".into(),
        n => format!(" · {} more drives low", n),
    };
    Some((format!("{}{} · {:.0} GB free{}{}", v.volume, kind, v.free_gb.floor(), state, more), v.level))
}

/// The disk block for `memguard status` and `memguard_status`, or `None` when disk watching is off.
pub fn status_brief(cfg: &Config) -> Option<DiskBrief> {
    let s = cfg.disk();
    s.enabled.then(|| brief(&read_volumes(cfg, &DiskWatch::default()), &s))
}

// ---------------------------------------------------------------- the wait gate

/// One volume the `wait` gate checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskCheck {
    pub volume: String,
    pub free_gb: f64,
    pub need_gb: f64,
    /// Why it was checked: "project", "ddc", "zen", "temp".
    pub roles: Vec<String>,
    pub ok: bool,
}

/// A volume the gate looks at: (key, name, bytes available to the caller).
pub type GateVolume = (String, String, u64);

/// Combine the needs so each volume is checked once, with the largest need of its roles.
/// `project` needs `disk_gb`; each cache volume needs `cache_min_gb`. Pure.
pub fn gate_checks(project: Option<GateVolume>, disk_gb: f64, caches: &[(&str, GateVolume)], cache_min_gb: f64) -> Vec<DiskCheck> {
    let mut out: Vec<(String, DiskCheck)> = Vec::new();
    let mut add = |vol: &GateVolume, role: &str, need: f64| {
        if let Some((_, c)) = out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&vol.0)) {
            c.need_gb = c.need_gb.max(need);
            if !c.roles.iter().any(|r| r == role) {
                c.roles.push(role.into());
            }
        } else {
            out.push((vol.0.clone(), DiskCheck { volume: vol.1.clone(), free_gb: gb1(vol.2), need_gb: need, roles: vec![role.into()], ok: true }));
        }
    };
    if let Some(p) = &project {
        add(p, "project", disk_gb);
    }
    if cache_min_gb > 0.0 {
        for (role, v) in caches {
            add(v, role, cache_min_gb);
        }
    }
    out.into_iter()
        .map(|(_, mut c)| {
            c.ok = c.free_gb >= c.need_gb;
            c
        })
        .collect()
}

/// Read this PC for the gate: the volume of `path` (default: the current folder) and the
/// volumes of the DDC, Zen and TEMP.
pub fn gate(cfg: &Config, path: Option<&str>, disk_gb: f64) -> Vec<DiskCheck> {
    let s = cfg.disk();
    let read = |p: &str| -> Option<GateVolume> {
        let p = existing(std::path::Path::new(p))?;
        let (root, guid) = disk_win::volume_of(&p)?;
        let (avail, _, _) = disk_win::free_space(&root)?;
        Some((guid, root.trim_end_matches('\\').to_string(), avail))
    };
    let cwd = std::env::current_dir().ok().map(|p| p.display().to_string());
    let project = path.map(str::to_string).or(cwd).and_then(|p| read(&p));
    let (ddc, zen) = ue_cache_dirs();
    let mut caches: Vec<(&str, GateVolume)> = Vec::new();
    for (role, p) in [("ddc", ddc), ("zen", zen), ("temp", Some(std::env::temp_dir()))] {
        if let Some(v) = p.and_then(|p| read(&p.display().to_string())) {
            caches.push((role, v));
        }
    }
    gate_checks(project, disk_gb, &caches, s.cache_min_free_gb)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    fn s() -> DiskSettings {
        DiskSettings::default()
    }

    fn facts(letter: &str, fs: &str, total_gb: u64, avail_gb: u64, dev: bool) -> VolumeFacts {
        VolumeFacts {
            guid: format!(r"\\?\Volume{{{}}}\", letter),
            paths: vec![format!("{}:\\", letter)],
            drive_type: 3,
            label: String::new(),
            fs: fs.into(),
            fs_flags: if fs == "ReFS" { disk_win::FS_BLOCK_REFCOUNTING } else { 0 },
            total: total_gb * G,
            avail: avail_gb * G,
            free: avail_gb * G,
            cluster: 4096,
            dev_volume: Some(dev),
            trusted: Some(dev),
            reserved: None,
            serial64: None,
            fs_version: None,
            bus_type: Some(17),
            seek_penalty: Some(false),
        }
    }

    fn ctx_for(sys: &str, projects: &[&str]) -> RoleContext {
        let g = |l: &str| format!(r"\\?\Volume{{{}}}\", l);
        RoleContext { system: Some(g(sys)), pagefile: vec![g(sys)], projects: projects.iter().map(|p| g(p)).collect(), temp: Some(g(sys)), ..Default::default() }
    }

    #[test]
    fn the_smaller_of_gb_and_percent_applies() {
        // 8 TB disk: 10% would be 800 GB; the 150 GB rule wins.
        assert_eq!(threshold(150.0, 10.0, 8000 * G), 150 * G);
        // 500 GB disk: 10% is 50 GB, below 150 GB.
        assert_eq!(threshold(150.0, 10.0, 500 * G), 50 * G);
        assert_eq!(level_for(320 * G, 1907 * G, &s()), Level::Normal);
        assert_eq!(level_for(120 * G, 1907 * G, &s()), Level::Elevated);
        assert_eq!(level_for(38 * G, 1907 * G, &s()), Level::Critical);
        // Small disk: percent figures apply.
        assert_eq!(level_for(40 * G, 500 * G, &s()), Level::Elevated);
        assert_eq!(level_for(20 * G, 500 * G, &s()), Level::Critical);
    }

    #[test]
    fn hysteresis_needs_5_gb_to_get_better() {
        let t = 1907 * G;
        let st = s();
        assert_eq!(level_with_hysteresis(Some(Level::Elevated), 152 * G, t, &st), Level::Elevated);
        assert_eq!(level_with_hysteresis(Some(Level::Elevated), 156 * G, t, &st), Level::Normal);
        assert_eq!(level_with_hysteresis(Some(Level::Normal), 149 * G, t, &st), Level::Elevated);
        assert_eq!(level_with_hysteresis(Some(Level::Critical), 52 * G, t, &st), Level::Critical);
        assert_eq!(level_with_hysteresis(Some(Level::Critical), 56 * G, t, &st), Level::Elevated);
        assert_eq!(level_with_hysteresis(Some(Level::Critical), 300 * G, t, &st), Level::Normal);
        assert_eq!(level_with_hysteresis(Some(Level::Critical), 153 * G, t, &st), Level::Elevated);
        assert_eq!(level_with_hysteresis(Some(Level::Elevated), 10 * G, t, &st), Level::Critical);
        assert_eq!(level_with_hysteresis(None, 152 * G, t, &st), Level::Normal);
    }

    #[test]
    fn one_toast_per_level_change() {
        let st = s();
        let ctx = ctx_for("C", &["D"]);
        let mut w = DiskWatch::default();
        let poll = |w: &mut DiskWatch, avail: u64| {
            let f = facts("D", "ReFS", 1907, avail, true);
            let v = volume_from(&f, &ctx, &st, w.previous(&f.guid));
            w.update(&[v], &st)
        };
        assert!(poll(&mut w, 320).is_empty(), "first poll at normal is a baseline");
        let ch = poll(&mut w, 148);
        assert_eq!(ch.len(), 1);
        assert_eq!((ch[0].1, ch[0].2), (Level::Normal, Level::Elevated));
        assert!(poll(&mut w, 147).is_empty());
        assert!(poll(&mut w, 152).is_empty(), "inside the 5 GB band: no change");
        assert!(poll(&mut w, 149).is_empty());
        let ch = poll(&mut w, 40);
        assert_eq!((ch[0].1, ch[0].2), (Level::Elevated, Level::Critical));
        let ch = poll(&mut w, 200);
        assert_eq!((ch[0].1, ch[0].2), (Level::Critical, Level::Normal));
        // A volume already low at the first poll is said once.
        let mut w2 = DiskWatch::default();
        assert_eq!(poll(&mut w2, 40).len(), 1);
    }

    #[test]
    fn toast_wording() {
        let st = s();
        let ctx = ctx_for("C", &["D"]);
        let d = volume_from(&facts("D", "ReFS", 1907, 38, true), &ctx, &st, None);
        let (t, b) = level_change_words(&d, Level::Elevated, Level::Critical, Some(100.0)).unwrap();
        assert_eq!(t, "Your Dev Drive is almost full");
        assert_eq!(b, "D: has 38 GB free, and a cook usually needs 100 GB. Run a disk checkup to see what can go.");
        let c = volume_from(&facts("C", "NTFS", 1903, 120, false), &ctx, &st, None);
        let (t, b) = level_change_words(&c, Level::Normal, Level::Elevated, Some(100.0)).unwrap();
        assert_eq!(t, "C: is low on space");
        assert!(b.contains("Windows may not be able to grow the page file, and Unreal can run out of memory."), "{}", b);
        let (t, _) = level_change_words(&d, Level::Critical, Level::Normal, None).unwrap();
        assert_eq!(t, "D: has room again");
        assert!(level_change_words(&d, Level::Critical, Level::Elevated, None).is_none());
    }

    #[test]
    fn roles_and_one_row_per_volume() {
        let st = s();
        let mut ctx = ctx_for("C", &["D"]);
        ctx.ddc = Some(r"\\?\Volume{C}\".into());
        let mut f = facts("D", "ReFS", 1907, 369, true);
        f.paths.push(r"C:\mnt\dev\".into());
        let d = volume_from(&f, &ctx, &st, None);
        assert_eq!(d.roles, vec!["dev_drive", "projects"]);
        assert_eq!(d.name(), "D:");
        assert!(d.block_cloning && d.trusted == Some(true));
        assert_eq!(d.backing, "partition");
        let c = volume_from(&facts("C", "NTFS", 1903, 298, false), &ctx, &st, None);
        assert_eq!(c.roles, vec!["system", "pagefile", "ddc", "temp"]);
        assert_eq!(c.trusted, None);
        let g = volume_from(&facts("G", "NTFS", 7451, 6172, false), &ctx, &st, None);
        assert!(g.roles.is_empty() && !is_watched(&g, &st));
        let line = flyout_line(&[c.clone(), d.clone(), g], &st).unwrap();
        assert_eq!(line, "D: Dev Drive · 369 GB free");
        let tight = volume_from(&facts("C", "NTFS", 1903, 40, false), &ctx, &st, None);
        assert_eq!(flyout_line(&[tight, d], &st).unwrap(), "C: system drive · 40 GB free · almost full");
        let mut vhd = facts("V", "ReFS", 100, 50, true);
        vhd.bus_type = Some(disk_win::BUS_FILE_BACKED_VIRTUAL);
        assert_eq!(volume_from(&vhd, &ctx, &st, None).backing, "vhd");
    }

    #[test]
    fn fsutil_output_is_matched_or_unknown() {
        let en = "Developer volumes are enabled.\r\n\r\nDeveloper volumes are protected by antivirus filter.\r\n";
        assert_eq!(parse_fsutil_devdrv(en), (Some(true), "antivirus_allowed"));
        let off = "Developer volumes are enabled.\nDeveloper volumes are not protected by antivirus filter.\n";
        assert_eq!(parse_fsutil_devdrv(off), (Some(true), "antivirus_disallowed"));
        assert_eq!(parse_fsutil_devdrv("Die Entwicklervolumes sind aktiviert."), (None, "unknown"));
        assert_eq!(parse_fsutil_devdrv(""), (None, "unknown"));
    }

    #[test]
    fn defender_probe_reads_numbers_only() {
        let d = parse_defender("pm=1\r\nrt=1\r\nam=1\r\nav=0\r\nnoise\r\npm2=x\r\n");
        assert_eq!(d.performance_mode_status, Some(1));
        assert_eq!(d.realtime_enabled, Some(true));
        assert_eq!(d.third_party_av, Some(0));
        assert_eq!(performance_mode_words(d.performance_mode_status), "off");
        let bad = parse_defender("pm=Disabled\n");
        assert_eq!(bad.performance_mode_status, None);
        assert_eq!(performance_mode_words(Some(7)), "unknown");
    }

    fn dev_state(pm: &str, av: &str) -> DevDriveState {
        DevDriveState { enabled: Some(true), av_filters: av.into(), defender_performance_mode: pm.into(), realtime_protection: Some(true), third_party_av: Some(0), defender_checked: String::new() }
    }

    #[test]
    fn dev_drive_findings() {
        let st = s();
        let ctx = ctx_for("C", &["D"]);
        let c = volume_from(&facts("C", "NTFS", 1903, 298, false), &ctx, &st, None);
        let d = volume_from(&facts("D", "ReFS", 1907, 370, true), &ctx, &st, None);
        let e = volume_from(&facts("E", "NTFS", 1863, 1739, false), &ctx, &st, None);
        let mut hdd = facts("G", "NTFS", 7451, 6172, false);
        hdd.seek_penalty = Some(true);
        let g = volume_from(&hdd, &ctx, &st, None);
        let vols = vec![c, d.clone(), e, g];
        let caches = CachesOnSystem { gb: 200.0, measured: String::new() };
        let f = dev_findings(&vols, &dev_state("off", "antivirus_allowed"), Some(&caches), &st);
        let ids: Vec<&str> = f.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["dev:perf_mode_off", "dev:caches_on_c"]);
        assert_eq!(f[0].detail, "Defender's performance mode is off, so files on your Dev Drive are scanned before they open.");
        assert_eq!(f[0].fix.as_ref().unwrap().command, "Set-MpPreference -PerformanceModeStatus Enabled");
        // D: (370 GB free, 200 GB would leave 170 but... the rule is "still above elevated"):
        // 370 - 200 = 170 > 150, so with 370 free D: has room; on this PC D: had 320.
        let d320 = volume_from(&facts("D", "ReFS", 1907, 320, true), &ctx, &st, None);
        let vols2 = vec![vols[0].clone(), d320, vols[2].clone(), vols[3].clone()];
        let f = dev_findings(&vols2, &dev_state("on", "antivirus_allowed"), Some(&caches), &st);
        assert_eq!(f.len(), 1);
        assert!(f[0].detail.starts_with("About 200 GB of Unreal and toolchain caches are on C:."), "{}", f[0].detail);
        assert!(f[0].detail.contains("D: doesn't have room"), "{}", f[0].detail);
        assert!(f[0].detail.contains("E: has room"), "never the HDD G:: {}", f[0].detail);
        // Untrusted, filters off, third-party AV.
        let mut un = facts("D", "ReFS", 1907, 370, true);
        un.trusted = Some(false);
        let du = volume_from(&un, &ctx, &st, None);
        let mut state = dev_state("off", "antivirus_disallowed");
        state.third_party_av = Some(1);
        let f = dev_findings(&[du], &state, None, &st);
        let ids: Vec<&str> = f.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["dev:not_trusted", "dev:av_disallowed", "dev:third_party_av"]);
        assert_eq!(f[0].detail, "D: is a Dev Drive but isn't trusted, so Defender scans it the slow way.");
        assert_eq!(f[0].fix.as_ref().unwrap().command, "fsutil devdrv trust D:");
        // Unknown performance mode: no finding, never a guess.
        assert!(dev_findings(&[d], &dev_state("unknown", "unknown"), None, &st).is_empty());
        assert!(vhd_host_low(100 * G, 500 * G, 300 * G));
        assert!(!vhd_host_low(100 * G, 500 * G, 450 * G));
    }

    #[test]
    fn gate_checks_each_volume_once() {
        let d = ("D".to_string(), "D:".to_string(), 120 * G);
        let c = ("C".to_string(), "C:".to_string(), 298 * G);
        let checks = gate_checks(Some(d.clone()), 100.0, &[("ddc", c.clone()), ("zen", c.clone()), ("temp", c.clone())], 20.0);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].roles, vec!["project"]);
        assert!(checks[0].ok);
        assert_eq!(checks[1].roles, vec!["ddc", "zen", "temp"]);
        assert_eq!(checks[1].need_gb, 20.0);
        // Project and TEMP on one volume: one check with the larger need.
        let checks = gate_checks(Some(d.clone()), 130.0, &[("temp", d.clone())], 20.0);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].need_gb, 130.0);
        assert!(!checks[0].ok);
        assert_eq!(checks[0].roles, vec!["project", "temp"]);
        assert!(gate_checks(None, 100.0, &[], 20.0).is_empty());
    }
}

#[cfg(test)]
mod brief_line_tests {
    use super::*;

    fn vol(name: &str, level: Level, free_gb: f64, free_pct: f64, roles: &[&str]) -> BriefVolume {
        BriefVolume { volume: name.into(), free_gb, free_pct, level, roles: roles.iter().map(|r| r.to_string()).collect() }
    }

    #[test]
    fn dev_drive_first_among_equals_and_others_are_counted() {
        let b = DiskBrief {
            level: Level::Critical,
            volumes: vec![vol("C:", Level::Critical, 292.4, 15.4, &["system"]), vol("D:", Level::Critical, 447.3, 23.4, &["dev_drive", "projects"])],
        };
        let (line, level) = brief_line(&b).unwrap();
        assert_eq!(line, "D: Dev Drive · 447 GB free · almost full · 1 more drive low");
        assert_eq!(level, Level::Critical);
    }

    #[test]
    fn worse_drive_still_wins_over_the_dev_drive() {
        let b = DiskBrief {
            level: Level::Critical,
            volumes: vec![vol("C:", Level::Critical, 20.0, 1.0, &["system"]), vol("D:", Level::Elevated, 140.0, 7.3, &["dev_drive"])],
        };
        assert_eq!(brief_line(&b).unwrap().0, "C: system drive · 20 GB free · almost full · 1 more drive low");
    }

    #[test]
    fn all_fine_shows_the_dev_drive_alone() {
        let b = DiskBrief {
            level: Level::Normal,
            volumes: vec![vol("C:", Level::Normal, 292.4, 15.4, &["system"]), vol("D:", Level::Normal, 447.3, 23.4, &["dev_drive"])],
        };
        assert_eq!(brief_line(&b).unwrap().0, "D: Dev Drive · 447 GB free");
    }
}

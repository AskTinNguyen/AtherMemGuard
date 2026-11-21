//! Settings: schema, built-in defaults, layering and the comment-preserving editing API.
//!
//! Effective settings = the built-in defaults (`BUILTIN_TOML`), then `team.toml` in the data
//! folder if it exists, then the user's `settings.toml`. Layers are merged as TOML before they
//! are parsed: tables deep-merge, `[[group]]` entries merge by `name` (new names append), and
//! a layer that sets `trim_above_mb` or `trim_above_pct` replaces the soft cap of the layers
//! below it, whichever of the two they used.
//!
//! The editing functions (`set_user_value`, `reset_user_value`, `add_group`, ...) edit
//! `settings.toml` with `toml_edit`, so the user's comments and layout survive. Every edit is
//! validated by re-parsing the result together with the other layers, and written atomically.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The defaults compiled into the program. Every key is set here; the files on disk only
/// override. Soft caps are percentages of total RAM so they suit 16 GB and 128 GB PCs alike.
pub const BUILTIN_TOML: &str = r#"# Ather MemGuard built-in defaults.
interval_sec = 10
trim_cooldown_sec = 120
foreground_grace_sec = 45

[general]
preset = "balanced"
notifications = true
auto_prime = true
prime_hotkey = "Ctrl+Alt+M"
check_ram_speed = true
elevated_helper = false

[updates]
check_automatically = true
manifest_url = ""

[history]
enabled = true
keep_days = 7

[pressure]
elevated_ram_free_pct = 10
elevated_commit_free_pct = 12
critical_ram_free_pct = 4
critical_commit_free_pct = 6
hysteresis_pct = 2.0
hysteresis_gb = 2.0

[ue]
protect = [
    "UnrealEditor*", "ShaderCompileWorker", "UnrealBuildTool", "dotnet", "cl", "link",
    "clang-cl", "lld-link", "UbaAgent", "UbaCli", "UbaHost", "zenserver", "UnrealPak",
    "LiveCodingConsole", "UnrealTraceServer", "CrashReportClient*", "EpicWebHelper",
    "AutomationTool", "msbuild", "git", "git-lfs",
    "UnrealHeaderTool", "UnrealLightmass", "dxc", "fxc", "ispc", "rc", "lib", "llvm-lib",
]
workers = [
    "cl", "clang-cl", "link", "lld-link", "ShaderCompileWorker",
    "UbaHost", "UbaAgent", "LiveCodingConsole", "UnrealHeaderTool", "UnrealLightmass",
    "UnrealPak", "dxc", "fxc", "ispc", "rc", "lib", "llvm-lib",
]
editor_restart_hint_gb = 48
editor_growth_alert_gb_per_hour = 6
worker_throttle = false
min_workers = 4

[prime]
editor = { ram_gb = 16, commit_gb = 24, disk_gb = 20 }
build = { ram_gb = 24, commit_gb = 40, disk_gb = 50 }
cook = { ram_gb = 32, commit_gb = 48, disk_gb = 100 }

[doctor]
keep = []
close = []
close_while_unreal = []
ignore_below_mb = 150
review_above_mb = 1024
nudge_above_gb = 4.0

[disk]
enabled = true
watch = ["auto"]
elevated_free_gb = 150
elevated_free_pct = 10
critical_free_gb = 50
critical_free_pct = 5
cache_min_free_gb = 20
project_dirs = []
lane_dirs = []
scan_depth = 3
scan_budget_sec = 60
scan_threads = 4
fresh_sec = 300
min_age_hours = 2
hold_hours = 24
keep = []
check_dev_drive = true

[[group]]
name = "Browsers"
processes = ["chrome", "msedge", "firefox", "brave", "msedgewebview2"]
priority = "low"
trim_above_pct = 4.8
[group.ue]
trim_above_pct = 2.4
[group.elevated]
trim_above_pct = 0.8
cpu = "below"
io_priority = "verylow"
[group.critical]
trim_above_pct = 0.2

[[group]]
name = "Chat"
processes = ["Slack", "ChatGPT", "Wispr Flow", "Discord", "Teams", "ms-teams", "Telegram", "Zoom"]
priority = "verylow"
trim_above_pct = 1.6
[group.ue]
trim_above_pct = 1.2
[group.elevated]
trim_above_pct = 0.4
cpu = "below"
io_priority = "verylow"
[group.critical]
trim_above_pct = 0.05

[[group]]
name = "Agents"
processes = ["claude", "codex"]
priority = "low"
[group.elevated]
trim_above_pct = 4.8
io_priority = "verylow"
[group.critical]
trim_above_pct = 1.6

[[group]]
name = "Background"
processes = [
    "steam", "steamwebhelper", "EpicGamesLauncher", "OneDrive", "copilotapp",
    "PhoneExperienceHost", "Widgets", "WidgetService", "Spotify",
]
priority = "verylow"
trim_above_pct = 0.4
cpu = "eco"
io_priority = "verylow"
[group.critical]
trim_above_pct = 0.05
"#;

/// Written to settings.toml on first run (and by `memguard config --reset`). It documents
/// every key but leaves them commented out, so the built-in and team defaults apply.
pub const DEFAULT_TOML: &str = r#"# Ather MemGuard settings. Saved changes are picked up within one interval.
#
# Only what you set here is used; everything else comes from the built-in defaults, or from
# team.toml in this folder if your team gave you one. Remove the # in front of a line to
# set it. `memguard config show` prints the settings in effect.

[general]
# gentle | balanced | aggressive. Scales every soft cap: gentle x2, balanced x1,
# aggressive x0.5. Aggressive also lowers the CPU priority of every group while Unreal runs.
preset = "balanced"
# notifications = true          # toasts for pressure changes, updates and hints
# auto_prime = true             # free RAM the moment a build or cook starts
# prime_hotkey = "Ctrl+Alt+M"   # empty = no hotkey
# check_ram_speed = true        # say once if the RAM runs below its rated speed (`memguard ram`)
# elevated_helper = false       # opt-in scheduled-task elevation for standby purge / page combining

# [updates]
# check_automatically = true
# manifest_url = ""             # empty = the built-in GitHub URL

# [history]
# enabled = true                # keep a memory history (history\YYYY-MM-DD.csv here)
# keep_days = 7

# interval_sec = 10             # how often to check, in seconds
# Minimum time between trims of the same group. It doubles (up to 16x) while a group keeps
# climbing straight back over its limit, and is ignored at critical pressure.
# trim_cooldown_sec = 120
# Seconds after an app leaves the foreground before it can be trimmed or throttled.
# foreground_grace_sec = 45

# [pressure]
# Percent of total RAM that is free / percent of the commit limit that is free.
# elevated_ram_free_pct = 10
# elevated_commit_free_pct = 12
# critical_ram_free_pct = 4
# critical_commit_free_pct = 6
# hysteresis_pct = 2.0
# hysteresis_gb = 2.0

# [ue]
# Never touched by any group, whatever the group lists say. A trailing * is a wildcard.
# protect = ["UnrealEditor*", "ShaderCompileWorker", "UnrealBuildTool", "dotnet", "cl", ...]
# Compile / shader workers, counted in status and used by worker_throttle.
# workers = ["cl", "clang-cl", "link", "lld-link", "ShaderCompileWorker", ...]
# Suggest restarting the editor when one editor process commits more than this.
# editor_restart_hint_gb = 48
# Tell you when an editor's memory keeps growing faster than this. 0 = off.
# editor_growth_alert_gb_per_hour = 6
# Opt-in. At critical pressure, pause the newest workers (never fewer than min_workers
# keep running) and resume them when pressure drops.
# worker_throttle = false
# min_workers = 4

# [prime]
# Headroom `memguard prime --for X` and `memguard wait --for X` aim for. Tune to your project.
# disk_gb is the free disk space `memguard wait` wants on the project's drive (--disk-path).
# editor = { ram_gb = 16, commit_gb = 24, disk_gb = 20 }
# build = { ram_gb = 24, commit_gb = 40, disk_gb = 50 }
# cook = { ram_gb = 32, commit_gb = 48, disk_gb = 100 }

# [doctor]
# keep = []                     # never proposed for closing
# close = []                    # always treated as not needed for Unreal
# close_while_unreal = []       # asked to close whenever Unreal is running
# ignore_below_mb = 150
# review_above_mb = 1024
# nudge_above_gb = 4.0

# [disk]
# enabled = true                # watch free space and say once when a drive gets low
# watch = ["auto"]              # Dev Drives, the system drive and drives with projects, DDC/Zen
#                               # or TEMP; or drive letters and folders, e.g. ["D:", 'E:\Builds']
# A level applies below the smaller of the GB and the percent figure.
# elevated_free_gb = 150
# elevated_free_pct = 10
# critical_free_gb = 50
# critical_free_pct = 5
# cache_min_free_gb = 20        # what `memguard wait` wants free where the DDC, Zen and TEMP are
# project_dirs = []             # where the disk checkup looks for Unreal projects
# lane_dirs = []                # extra folders holding worktrees or lanes (siblings are found anyway)
# scan_depth = 3                # how deep to look for project roots
# scan_budget_sec = 60          # how long a disk checkup may count, in seconds
# scan_threads = 4
# fresh_sec = 300               # older counts are reported as "about"
# min_age_hours = 2             # cleanup leaves alone anything written more recently
# hold_hours = 24               # how long the bin keeps what was moved into it
# keep = []                     # folders the disk checkup never lists
# check_dev_drive = true        # Dev Drive health rows in the checkups

# App groups. The built-in ones are Browsers, Chat, Agents and Background. A [[group]] here
# with the same name changes only the keys you set; a new name adds a group.
#
#   processes       executable names; a trailing * is a wildcard
#   enabled         false = leave this group alone
#   priority        normal | belownormal | medium | low | verylow
#                   Windows takes RAM from lower-priority apps first. No crash risk.
#   trim_above_pct  soft cap as a percent of total RAM: above it, the group's memory is
#                   pushed to the compressed store / page file. Frees RAM, not commit.
#   trim_above_mb   the same in MB. Wins over trim_above_pct when both are set. 0 = off.
#                   Soft caps are never below 128 MB.
#   cpu             normal | below | idle | eco   (eco = Windows efficiency mode)
#   io_priority     verylow | low | normal
#   suspend         true freezes the apps completely until pressure drops. Use with care.
#   ws_cap_mb       optional physical working-set cap on the group. 0 = off.
#   ws_cap_per_process_mb   the same, per process. 0 = off.
#   cpu_rate_pct    optional Job Object CPU rate cap (1..100). 0 or unset = off.
#   hard_cap_mb     hard cap on the group's committed memory. Apps that hit it usually
#                   crash. Sandboxed browser/Electron helpers can't be capped. 0 = off.
#   hard_cap_per_process_mb   the same, per process.
#
# [group.ue], [group.elevated] and [group.critical] override the group while Unreal is
# running, and while memory pressure is elevated or critical.
#
# [[group]]
# name = "Browsers"
# trim_above_pct = 4.8
# [group.ue]
# trim_above_pct = 2.4
#
# [[group]]
# name = "Chat"
# enabled = false
#
# [[group]]
# name = "Notes"
# processes = ["Obsidian", "Notion"]
# priority = "low"
# trim_above_mb = 1024
"#;

/// The default settings files that 0.1 wrote. A settings.toml identical to one of these was
/// never edited, so it is replaced by the new template (the old file is kept as .bak).
const V01_DEFAULTS: [&str; 3] = [
    include_str!("fixtures/settings-0.1.toml"),
    include_str!("fixtures/settings-0.1-early.toml"),
    include_str!("fixtures/settings-memcap.toml"),
];

/// Soft caps never resolve below this (except 0, which means off).
pub const MIN_TRIM_MB: u64 = 128;

// ---------------------------------------------------------------- schema

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemPriority {
    VeryLow,
    Low,
    Medium,
    BelowNormal,
    Normal,
}

impl MemPriority {
    /// Windows MEMORY_PRIORITY_* value.
    pub fn value(self) -> u32 {
        match self {
            MemPriority::VeryLow => 1,
            MemPriority::Low => 2,
            MemPriority::Medium => 3,
            MemPriority::BelowNormal => 4,
            MemPriority::Normal => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cpu {
    Normal,
    Below,
    Idle,
    Eco,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IoPriority {
    VeryLow,
    Low,
    Normal,
}

impl IoPriority {
    /// Windows ProcessIoPriority value (`0 = VeryLow`, `1 = Low`, `2 = Normal`).
    pub fn value(self) -> u32 {
        match self {
            IoPriority::VeryLow => 0,
            IoPriority::Low => 1,
            IoPriority::Normal => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Gentle,
    #[default]
    Balanced,
    Aggressive,
}

impl Preset {
    /// Multiplier for every resolved soft cap.
    pub fn factor(self) -> f64 {
        match self {
            Preset::Gentle => 2.0,
            Preset::Balanced => 1.0,
            Preset::Aggressive => 0.5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Preset::Gentle => "gentle",
            Preset::Balanced => "balanced",
            Preset::Aggressive => "aggressive",
        }
    }
}

/// A soft cap from one table: MB wins over percent when both are set. `None` = not set here.
pub fn limit_mb(mb: Option<u64>, pct: Option<f64>, ram_total_mb: u64) -> Option<u64> {
    match (mb, pct) {
        (Some(m), _) => Some(m),
        (None, Some(p)) => Some((ram_total_mb as f64 * p / 100.0).round().max(0.0) as u64),
        (None, None) => None,
    }
}

/// Apply the preset multiplier and the 128 MB floor to a resolved soft cap. 0 stays off.
pub fn scale_soft_cap(mb: u64, preset: Preset) -> u64 {
    if mb == 0 {
        0
    } else {
        ((mb as f64 * preset.factor()).round() as u64).max(MIN_TRIM_MB)
    }
}

/// Overrides applied on top of a group's base settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<MemPriority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_above_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_above_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Cpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_priority: Option<IoPriority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspend: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_cap_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_cap_per_process_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_rate_pct: Option<u32>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    pub processes: Vec<String>,
    /// false = the group is ignored.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<MemPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_above_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_above_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Cpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_priority: Option<IoPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<bool>,
    #[serde(default)]
    pub ws_cap_mb: u64,
    #[serde(default)]
    pub ws_cap_per_process_mb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_rate_pct: Option<u32>,
    #[serde(default)]
    pub hard_cap_mb: u64,
    #[serde(default)]
    pub hard_cap_per_process_mb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ue: Option<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevated: Option<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical: Option<Policy>,
}

fn default_pressure_hysteresis_pct() -> f64 {
    2.0
}
fn default_pressure_hysteresis_gb() -> f64 {
    2.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pressure {
    pub elevated_ram_free_pct: f64,
    pub elevated_commit_free_pct: f64,
    pub critical_ram_free_pct: f64,
    pub critical_commit_free_pct: f64,
    #[serde(default = "default_pressure_hysteresis_pct")]
    pub hysteresis_pct: f64,
    #[serde(default = "default_pressure_hysteresis_gb")]
    pub hysteresis_gb: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ue {
    pub protect: Vec<String>,
    pub workers: Vec<String>,
    pub editor_restart_hint_gb: f64,
    /// Alert when an editor's committed memory grows faster than this. 0 = off.
    #[serde(default = "default_growth_alert")]
    pub editor_growth_alert_gb_per_hour: f64,
    #[serde(default)]
    pub worker_throttle: bool,
    #[serde(default = "default_min_workers")]
    pub min_workers: usize,
}

fn default_min_workers() -> usize {
    4
}
fn default_growth_alert() -> f64 {
    6.0
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub ram_gb: f64,
    pub commit_gb: f64,
    /// Free disk space `memguard wait` wants on the project's volume. None = no disk gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prime {
    pub editor: Target,
    pub build: Target,
    pub cook: Target,
}

impl Prime {
    pub fn target(&self, name: &str) -> Option<Target> {
        match name {
            "editor" => Some(self.editor),
            "build" => Some(self.build),
            "cook" => Some(self.cook),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub preset: Preset,
    /// Toasts for pressure changes, updates and hints.
    pub notifications: bool,
    /// Trim every group the moment a build or cook starts.
    pub auto_prime: bool,
    /// Empty = no hotkey.
    pub prime_hotkey: String,
    /// Check whether the RAM runs below its rated speed (the checkup row and a one-time toast).
    pub check_ram_speed: bool,
    /// Opt-in elevated scheduled-task helper for standby purge / page combining. Zero-admin default.
    pub elevated_helper: bool,
}

impl Default for General {
    fn default() -> Self {
        General {
            preset: Preset::Balanced,
            notifications: true,
            auto_prime: true,
            prime_hotkey: "Ctrl+Alt+M".into(),
            check_ram_speed: true,
            elevated_helper: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Updates {
    pub check_automatically: bool,
    /// Empty = the built-in GitHub URL.
    pub manifest_url: String,
}

impl Default for Updates {
    fn default() -> Self {
        Updates { check_automatically: true, manifest_url: String::new() }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistorySettings {
    pub enabled: bool,
    pub keep_days: u32,
}

impl Default for HistorySettings {
    fn default() -> Self {
        HistorySettings { enabled: true, keep_days: 7 }
    }
}

/// `[disk]`: headroom levels, the wait gate's cache volumes and the disk checkup.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiskSettings {
    pub enabled: bool,
    /// `["auto"]`, or drive letters and folders.
    pub watch: Vec<String>,
    pub elevated_free_gb: f64,
    pub elevated_free_pct: f64,
    pub critical_free_gb: f64,
    pub critical_free_pct: f64,
    /// Free space `memguard wait` wants on the volumes holding the DDC, Zen and TEMP.
    pub cache_min_free_gb: f64,
    pub project_dirs: Vec<String>,
    pub lane_dirs: Vec<String>,
    pub scan_depth: u32,
    pub scan_budget_sec: u64,
    pub scan_threads: usize,
    pub fresh_sec: u64,
    /// Cleanup refuses a whole item when anything in it was written more recently than this.
    pub min_age_hours: f64,
    /// How long the bin keeps what was moved into it before it empties itself.
    pub hold_hours: f64,
    /// Folders the disk checkup never lists.
    pub keep: Vec<String>,
    pub check_dev_drive: bool,
}

impl Default for DiskSettings {
    fn default() -> Self {
        DiskSettings {
            enabled: true,
            watch: vec!["auto".into()],
            elevated_free_gb: 150.0,
            elevated_free_pct: 10.0,
            critical_free_gb: 50.0,
            critical_free_pct: 5.0,
            cache_min_free_gb: 20.0,
            project_dirs: Vec::new(),
            lane_dirs: Vec::new(),
            scan_depth: 3,
            scan_budget_sec: 60,
            scan_threads: 4,
            fresh_sec: 300,
            min_age_hours: 2.0,
            hold_hours: 24.0,
            keep: Vec::new(),
            check_dev_drive: true,
        }
    }
}

/// `[doctor]` checkup settings (keep / close lists and thresholds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DoctorSettings {
    pub keep: Vec<String>,
    pub close: Vec<String>,
    pub close_while_unreal: Vec<String>,
    pub ignore_below_mb: u64,
    pub review_above_mb: u64,
    pub nudge_above_gb: f64,
}

impl Default for DoctorSettings {
    fn default() -> Self {
        DoctorSettings {
            keep: Vec::new(),
            close: Vec::new(),
            close_while_unreal: Vec::new(),
            ignore_below_mb: 150,
            review_above_mb: 1024,
            nudge_above_gb: 4.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_interval")]
    pub interval_sec: u64,
    #[serde(default = "default_cooldown")]
    pub trim_cooldown_sec: u64,
    #[serde(default = "default_foreground_grace")]
    pub foreground_grace_sec: u64,
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub updates: Updates,
    #[serde(default)]
    pub history: HistorySettings,
    #[serde(default = "default_pressure")]
    pub pressure: Pressure,
    #[serde(default = "default_ue")]
    pub ue: Ue,
    #[serde(default = "default_prime")]
    pub prime: Prime,
    #[serde(default)]
    pub doctor: DoctorSettings,
    /// Disk and Dev Drive settings; `Config::disk()` fills in the defaults when the table is
    /// missing. 0.2.1 carries this table untyped, so files stay compatible both ways.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskSettings>,
    /// Every group, including disabled ones (`enabled = false`); the engine skips those.
    #[serde(default, rename = "group")]
    pub groups: Vec<Group>,
}

fn default_interval() -> u64 {
    10
}
fn default_cooldown() -> u64 {
    120
}
fn default_foreground_grace() -> u64 {
    45
}

fn builtin_table_ref() -> &'static toml::Table {
    static TABLE: OnceLock<toml::Table> = OnceLock::new();
    TABLE.get_or_init(|| parse_table("built-in defaults", BUILTIN_TOML).expect("built-in default table is valid"))
}

fn builtin_config_ref() -> &'static Config {
    static CFG: OnceLock<Config> = OnceLock::new();
    CFG.get_or_init(|| Config::from_table(builtin_table_ref().clone()).expect("built-in default config is valid"))
}

fn default_pressure() -> Pressure {
    builtin_config_ref().pressure.clone()
}
fn default_ue() -> Ue {
    builtin_config_ref().ue.clone()
}
fn default_prime() -> Prime {
    builtin_config_ref().prime.clone()
}

impl Config {
    /// The defaults compiled into the program.
    pub fn builtin() -> Config {
        builtin_config_ref().clone()
    }

    /// Pre-compile UE protect/workers and active group patterns for fast tick matching.
    pub fn compile_patterns(&self) -> CompiledPatterns {
        CompiledPatterns::from_config(self)
    }

    /// Parse one complete settings text on its own (no layering), normalise and validate it.
    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.finish()
    }

    fn from_table(table: toml::Table) -> Result<Config, String> {
        let cfg: Config = toml::Value::Table(table).try_into().map_err(|e: toml::de::Error| e.to_string())?;
        cfg.finish()
    }

    fn finish(mut self) -> Result<Config, String> {
        self.interval_sec = self.interval_sec.clamp(2, 600);
        for g in &mut self.groups {
            for p in &mut g.processes {
                if p.len() >= 4 && p.get(p.len() - 4..).is_some_and(|t| t.eq_ignore_ascii_case(".exe")) {
                    p.truncate(p.len() - 4);
                }
            }
        }
        for list in [&mut self.doctor.keep, &mut self.doctor.close, &mut self.doctor.close_while_unreal] {
            for p in list.iter_mut() {
                if p.len() >= 4 && p.get(p.len() - 4..).is_some_and(|t| t.eq_ignore_ascii_case(".exe")) {
                    p.truncate(p.len() - 4);
                }
            }
        }
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), String> {
        let pct_ok = |v: Option<f64>| v.map_or(true, |p| p.is_finite() && (0.0..=100.0).contains(&p));
        let cpu_rate_ok = |v: Option<u32>| v.is_none_or(|r| r <= 100);
        if self.foreground_grace_sec > 3600 {
            return Err("foreground_grace_sec must be between 0 and 3600".into());
        }
        let mut names: Vec<&str> = Vec::new();
        for g in &self.groups {
            let name = g.name.trim();
            if name.is_empty() {
                return Err("a group has an empty name".into());
            }
            if g.name.contains('.') {
                return Err(format!("group \"{}\": names can't contain a dot", g.name));
            }
            if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                return Err(format!("group \"{}\" is defined twice", g.name));
            }
            names.push(name);
            if g.processes.is_empty() || g.processes.iter().any(|p| p.trim().is_empty()) {
                return Err(format!("group \"{}\": processes must list at least one name, none empty", g.name));
            }
            let policies = [&g.ue, &g.elevated, &g.critical];
            if !pct_ok(g.trim_above_pct) || policies.iter().any(|p| p.as_ref().is_some_and(|p| !pct_ok(p.trim_above_pct))) {
                return Err(format!("group \"{}\": trim_above_pct must be between 0 and 100", g.name));
            }
            if !cpu_rate_ok(g.cpu_rate_pct) || policies.iter().any(|p| p.as_ref().is_some_and(|p| !cpu_rate_ok(p.cpu_rate_pct))) {
                return Err(format!("group \"{}\": cpu_rate_pct must be between 0 and 100", g.name));
            }
        }
        if !(1..=3650).contains(&self.history.keep_days) {
            return Err("history.keep_days must be between 1 and 3650".into());
        }
        let ue = &self.ue;
        if !(ue.editor_growth_alert_gb_per_hour.is_finite() && ue.editor_growth_alert_gb_per_hour >= 0.0) {
            return Err("ue.editor_growth_alert_gb_per_hour must be 0 (off) or more".into());
        }
        if !(ue.editor_restart_hint_gb.is_finite() && ue.editor_restart_hint_gb >= 0.0) {
            return Err("ue.editor_restart_hint_gb must be 0 or more".into());
        }
        let p = &self.pressure;
        for v in [p.elevated_ram_free_pct, p.elevated_commit_free_pct, p.critical_ram_free_pct, p.critical_commit_free_pct] {
            if !(v.is_finite() && (0.0..=100.0).contains(&v)) {
                return Err("pressure percentages must be between 0 and 100".into());
            }
        }
        if !(p.hysteresis_pct.is_finite() && (0.0..=50.0).contains(&p.hysteresis_pct)) {
            return Err("pressure.hysteresis_pct must be between 0 and 50".into());
        }
        if !(p.hysteresis_gb.is_finite() && p.hysteresis_gb >= 0.0) {
            return Err("pressure.hysteresis_gb must be 0 or more".into());
        }
        if !(self.doctor.nudge_above_gb.is_finite() && self.doctor.nudge_above_gb >= 0.0) {
            return Err("doctor.nudge_above_gb must be 0 or more".into());
        }
        for (field, list) in [
            ("doctor.keep", &self.doctor.keep),
            ("doctor.close", &self.doctor.close),
            ("doctor.close_while_unreal", &self.doctor.close_while_unreal),
        ] {
            if list.iter().any(|p| p.trim().is_empty()) {
                return Err(format!("{}: entries cannot be empty", field));
            }
        }
        let hk = self.general.prime_hotkey.trim();
        if !hk.is_empty() {
            let parts: Vec<&str> = hk.split('+').map(str::trim).collect();
            if parts.len() < 2 || parts.iter().any(|s| s.is_empty()) {
                return Err(format!("general.prime_hotkey \"{}\" should look like \"Ctrl+Alt+M\", or be empty", hk));
            }
        }
        for t in [self.prime.editor, self.prime.build, self.prime.cook] {
            if t.disk_gb.is_some_and(|g| !(g.is_finite() && g >= 0.0)) {
                return Err("prime: disk_gb must be 0 or more".into());
            }
        }
        if let Some(d) = &self.disk {
            let pct_ok = |v: f64| v.is_finite() && (0.0..=100.0).contains(&v);
            let non_negative = |v: f64| v.is_finite() && v >= 0.0;
            if ![d.elevated_free_pct, d.critical_free_pct].into_iter().all(pct_ok) {
                return Err("disk: elevated_free_pct and critical_free_pct must be between 0 and 100".into());
            }
            if ![d.elevated_free_gb, d.critical_free_gb, d.cache_min_free_gb, d.min_age_hours, d.hold_hours].into_iter().all(non_negative) {
                return Err("disk: GB and hour settings must be 0 or more".into());
            }
            if !(1..=16).contains(&d.scan_depth) || !(1..=64).contains(&d.scan_threads) || !(1..=3600).contains(&d.scan_budget_sec) {
                return Err("disk: scan_depth must be 1 to 16, scan_threads 1 to 64, scan_budget_sec 1 to 3600".into());
            }
        }
        let url = self.updates.manifest_url.trim();
        if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("updates.manifest_url must be empty or start with https://".into());
        }
        Ok(())
    }

    /// The `[disk]` settings, with the defaults when the table is missing.
    pub fn disk(&self) -> DiskSettings {
        self.disk.clone().unwrap_or_default()
    }

    /// Groups the engine acts on.
    pub fn active_groups(&self) -> impl Iterator<Item = &Group> {
        self.groups.iter().filter(|g| g.enabled)
    }

    /// Layered settings from the data folder (built-in < team.toml < settings.toml). Writes
    /// the template first if there is no settings.toml, and migrates an untouched 0.1 file.
    pub fn load() -> Result<Config, String> {
        load_layered().map(|(c, _)| c)
    }

    /// Load settings, falling back to `team.toml` over built-in defaults if `settings.toml`
    /// is broken, or built-in defaults alone if `team.toml` is absent/broken.
    pub fn load_or_default() -> (Config, Option<String>) {
        match Config::load() {
            Ok(c) => (c, None),
            Err(e) => {
                let cfg = Store::real()
                    .load_team_over_builtin()
                    .unwrap_or_else(Config::builtin);
                (cfg, Some(e))
            }
        }
    }
}

/// Case-insensitive process-name match. A trailing `*` in the pattern is a wildcard.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    match p.strip_suffix('*') {
        Some(prefix) => n.starts_with(prefix),
        None => p == n,
    }
}

pub fn any_matches(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| name_matches(p, name))
}

/// A pattern list lowercased once, for matching many process names: the same rules as
/// `name_matches`, without lowercasing both sides on every comparison.
#[derive(Debug, Clone, Default)]
pub struct Patterns {
    match_all: bool,
    exact: Vec<String>,
    prefixes: Vec<String>,
}

impl Patterns {
    pub fn new(patterns: &[String]) -> Patterns {
        let mut out = Patterns::default();
        for p in patterns {
            let p = p.to_ascii_lowercase();
            if p == "*" {
                out.match_all = true;
                continue;
            }
            match p.strip_suffix('*') {
                Some(prefix) => {
                    if prefix.is_empty() {
                        out.match_all = true;
                    } else {
                        out.prefixes.push(prefix.to_string());
                    }
                }
                None => out.exact.push(p),
            }
        }
        if out.match_all {
            out.exact.clear();
            out.prefixes.clear();
            return out;
        }
        out.exact.sort();
        out.exact.dedup();
        out.prefixes.sort();
        out.prefixes.dedup();
        let mut minimal: Vec<String> = Vec::with_capacity(out.prefixes.len());
        for p in out.prefixes {
            if !minimal.last().is_some_and(|prev| p.starts_with(prev.as_str())) {
                minimal.push(p);
            }
        }
        out.prefixes = minimal;
        if !out.prefixes.is_empty() {
            out.exact.retain(|e| !out.prefixes.iter().any(|p| e.starts_with(p.as_str())));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        !self.match_all && self.exact.is_empty() && self.prefixes.is_empty()
    }

    /// Does `lower_name` (already ASCII-lowercased) match any pattern?
    pub fn matches_lower(&self, lower_name: &str) -> bool {
        if self.match_all {
            return true;
        }
        self.exact.binary_search_by(|p| p.as_str().cmp(lower_name)).is_ok()
            || self.prefixes.iter().any(|p| lower_name.starts_with(p.as_str()))
    }

    pub fn matches(&self, name: &str) -> bool {
        self.matches_lower(&name.to_ascii_lowercase())
    }
}

/// Pre-compiled matchers for `ue.protect`, `ue.workers`, and `cfg.active_groups()`.
#[derive(Debug, Clone, Default)]
pub struct CompiledPatterns {
    pub protect: Patterns,
    pub workers: Patterns,
    /// One `Patterns` per `cfg.active_groups()`, in the same order.
    pub groups: Vec<Patterns>,
}

impl CompiledPatterns {
    pub fn from_config(cfg: &Config) -> Self {
        CompiledPatterns {
            protect: Patterns::new(&cfg.ue.protect),
            workers: Patterns::new(&cfg.ue.workers),
            groups: cfg.active_groups().map(|g| Patterns::new(&g.processes)).collect(),
        }
    }

    /// Index into `groups` (matching `cfg.active_groups()`) of the first group matching `lower_name`.
    pub fn first_group_lower(&self, lower_name: &str) -> Option<usize> {
        self.groups.iter().position(|p| p.matches_lower(lower_name))
    }
}

// ---------------------------------------------------------------- layering

/// Where a value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Builtin,
    Team,
    User,
}

/// A value together with the layer it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved<T> {
    pub value: T,
    pub from: Layer,
}

const TRIM_MB: &str = "trim_above_mb";
const TRIM_PCT: &str = "trim_above_pct";

fn twin(key: &str) -> Option<&'static str> {
    match key {
        TRIM_MB => Some(TRIM_PCT),
        TRIM_PCT => Some(TRIM_MB),
        _ => None,
    }
}

fn parse_table(label: &str, text: &str) -> Result<toml::Table, String> {
    text.parse::<toml::Table>().map_err(|e| format!("{}: {}", label, e.to_string().trim_end()))
}

/// Deep-merge `over` into `base`. At the top level `group` arrays merge by name.
fn merge_tables(base: &mut toml::Table, over: &toml::Table, top: bool) -> Result<(), String> {
    for (k, v) in over {
        if top && k == "group" {
            merge_groups(base, v)?;
            continue;
        }
        if let Some(t) = twin(k) {
            if !over.contains_key(t) {
                base.remove(t);
            }
        }
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_tables(b, o, false)?,
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(())
}

fn merge_groups(base: &mut toml::Table, over: &toml::Value) -> Result<(), String> {
    let toml::Value::Array(entries) = over else {
        return Err("group must be a list of [[group]] tables".into());
    };
    let slot = base.entry("group").or_insert_with(|| toml::Value::Array(Vec::new()));
    let toml::Value::Array(list) = slot else {
        return Err("group must be a list of [[group]] tables".into());
    };
    for e in entries {
        let toml::Value::Table(et) = e else {
            return Err("group must be a list of [[group]] tables".into());
        };
        let Some(name) = et.get("name").and_then(|n| n.as_str()) else {
            return Err("every [[group]] needs a name".into());
        };
        let existing = list.iter_mut().find_map(|b| match b {
            toml::Value::Table(bt) if bt.get("name").and_then(|n| n.as_str()) == Some(name) => Some(bt),
            _ => None,
        });
        match existing {
            Some(bt) => merge_tables(bt, et, false)?,
            None => list.push(e.clone()),
        }
    }
    Ok(())
}

/// Warnings about a single layer: keys that are set but have no effect.
fn layer_warnings(label: &str, t: &toml::Table, out: &mut Vec<String>) {
    let both = |t: &toml::Table| t.contains_key(TRIM_MB) && t.contains_key(TRIM_PCT);
    if let Some(toml::Value::Array(groups)) = t.get("group") {
        for g in groups.iter().filter_map(|g| g.as_table()) {
            let name = g.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let mut tables = vec![("", g)];
            for sub in ["ue", "elevated", "critical"] {
                if let Some(st) = g.get(sub).and_then(|s| s.as_table()) {
                    tables.push((sub, st));
                }
            }
            for (sub, st) in tables {
                if both(st) {
                    let place = if sub.is_empty() { format!("group \"{}\"", name) } else { format!("group \"{}\" [{}]", name, sub) };
                    out.push(format!("{}: {} sets both trim_above_mb and trim_above_pct; trim_above_mb is used", label, place));
                }
            }
        }
    }
}

/// Pure layering: built-in, then `team` (if any), then `user`. Errors name the layer at fault.
pub fn config_from_layers(team: Option<&str>, user: &str) -> Result<(Config, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut merged = builtin_table_ref().clone();
    if let Some(t) = team {
        let tt = parse_table("team.toml", t)?;
        layer_warnings("team.toml", &tt, &mut warnings);
        merge_tables(&mut merged, &tt, true).map_err(|e| format!("team.toml: {}", e))?;
        // Validate the team layer on its own first, so a mistake there is reported as such.
        Config::from_table(merged.clone()).map_err(|e| format!("team.toml: {}", e))?;
    }
    let ut = parse_table("settings.toml", user)?;
    layer_warnings("settings.toml", &ut, &mut warnings);
    merge_tables(&mut merged, &ut, true).map_err(|e| format!("settings.toml: {}", e))?;
    let cfg = Config::from_table(merged).map_err(|e| format!("settings.toml: {}", e))?;
    Ok((cfg, warnings))
}

fn normalize(text: &str) -> String {
    text.trim_start_matches('\u{feff}').replace("\r\n", "\n")
}

/// True if `text` is one of the default files 0.1 wrote, unedited (line endings ignored).
pub fn is_untouched_v01(text: &str) -> bool {
    let t = normalize(text);
    V01_DEFAULTS.iter().any(|d| normalize(d) == t)
}

static TMP_NONCE: AtomicU64 = AtomicU64::new(0);

/// Write `data` to `path` through a unique same-directory temp file and a retry-protected rename.
pub fn write_atomic_bytes(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        }
    }
    let nonce = TMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp_name = format!("{}.{}.{}.tmp", file_name, std::process::id(), nonce);
    let tmp = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    };
    if let Err(e) = fs::write(&tmp, data) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {}", tmp.display(), e));
    }
    let backoffs = [5u64, 10, 20, 40, 80, 160];
    let mut last_err = None;
    for (i, &ms) in backoffs.iter().enumerate() {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retryable = e.kind() == std::io::ErrorKind::PermissionDenied
                    || matches!(e.raw_os_error(), Some(5 | 32 | 33));
                last_err = Some(e);
                if !retryable || i + 1 == backoffs.len() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(format!("{}: {}", path.display(), last_err.expect("at least one attempt")))
}

/// Write `text` to `path` through a temp file and a rename, so readers never see half a file.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    write_atomic_bytes(path, text.as_bytes())
}

// ---------------------------------------------------------------- paths inside the settings

enum SettingPath {
    /// e.g. `general.preset`, `prime.build.ram_gb`, `interval_sec`
    Plain(Vec<String>),
    /// `group.<name>.<keys...>`; no keys = the whole group entry
    Group { name: String, keys: Vec<String> },
}

impl SettingPath {
    fn parse(path: &str) -> Result<SettingPath, String> {
        let segs: Vec<String> = path.split('.').map(|s| s.trim().to_string()).collect();
        if segs.iter().any(|s| s.is_empty()) {
            return Err(format!("\"{}\" is not a settings path (like general.preset or group.Browsers.trim_above_pct)", path));
        }
        if segs[0] == "group" {
            if segs.len() < 2 {
                return Err("say which group: group.<name>.<key>".into());
            }
            return Ok(SettingPath::Group { name: segs[1].clone(), keys: segs[2..].to_vec() });
        }
        Ok(SettingPath::Plain(segs))
    }
}

fn find_group<'a>(t: &'a toml::Table, name: &str) -> Option<&'a toml::Table> {
    t.get("group")?.as_array()?.iter().filter_map(|g| g.as_table()).find(|g| g.get("name").and_then(|n| n.as_str()) == Some(name))
}

/// The table a path's last key lives in, and that key.
fn lookup_parent<'a>(t: &'a toml::Table, p: &'a SettingPath) -> Option<(&'a toml::Table, &'a str)> {
    let (mut cur, keys) = match p {
        SettingPath::Plain(k) => (t, k.as_slice()),
        SettingPath::Group { name, keys } => (find_group(t, name)?, keys.as_slice()),
    };
    let (last, init) = keys.split_last()?;
    for k in init {
        cur = cur.get(k)?.as_table()?;
    }
    Some((cur, last.as_str()))
}

fn lookup<'a>(t: &'a toml::Table, p: &'a SettingPath) -> Option<&'a toml::Value> {
    if let SettingPath::Group { name, keys } = p {
        if keys.is_empty() {
            return t.get("group")?.as_array()?.iter().find(|g| {
                g.as_table().and_then(|g| g.get("name")).and_then(|n| n.as_str()) == Some(name.as_str())
            });
        }
    }
    let (parent, key) = lookup_parent(t, p)?;
    parent.get(key)
}

/// Does this layer decide the value at `p`? (Setting the soft-cap twin also decides it.)
fn layer_decides(t: &toml::Table, p: &SettingPath) -> bool {
    if lookup(t, p).is_some() {
        return true;
    }
    match lookup_parent(t, p) {
        Some((parent, key)) => twin(key).is_some_and(|tw| parent.contains_key(tw)),
        None => false,
    }
}

// ---------------------------------------------------------------- toml_edit helpers

fn set_in(t: &mut dyn toml_edit::TableLike, keys: &[String], mut item: toml_edit::Item) -> Result<(), String> {
    let (first, rest) = keys.split_first().ok_or("empty settings path")?;
    if rest.is_empty() {
        // Keep the comment after the old value (`key = 1 # why`).
        let old_decor = t.get(first).and_then(|i| i.as_value()).map(|v| v.decor().clone());
        if let (Some(decor), Some(v)) = (old_decor, item.as_value_mut()) {
            *v.decor_mut() = decor;
        }
        if let Some(tw) = twin(first) {
            t.remove(tw);
        }
        t.insert(first, item);
        return Ok(());
    }
    if t.get(first).is_none() {
        t.insert(first, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let sub = t.get_mut(first).and_then(|i| i.as_table_like_mut()).ok_or_else(|| format!("{} is not a table", first))?;
    set_in(sub, rest, item)
}

/// Remove the key at `keys`. Returns whether anything was removed. Empty sub-tables of a
/// group are removed too.
fn remove_in(t: &mut dyn toml_edit::TableLike, keys: &[String], prune: bool) -> bool {
    let Some((first, rest)) = keys.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return t.remove(first).is_some();
    }
    let Some(sub) = t.get_mut(first).and_then(|i| i.as_table_like_mut()) else {
        return false;
    };
    let removed = remove_in(sub, rest, prune);
    if removed && prune && sub.is_empty() {
        let commented = t.get(first).and_then(|i| i.as_table()).is_some_and(|tb| has_comment(tb.decor()));
        if !commented {
            t.remove(first);
        }
    }
    removed
}

fn has_comment(d: &toml_edit::Decor) -> bool {
    let c = |r: Option<&toml_edit::RawString>| r.and_then(|r| r.as_str()).is_some_and(|s| s.contains('#'));
    c(d.prefix()) || c(d.suffix())
}

fn group_index(doc: &toml_edit::DocumentMut, name: &str) -> Option<usize> {
    doc.get("group")?.as_array_of_tables()?.iter().position(|g| g.get("name").and_then(|n| n.as_str()) == Some(name))
}

fn groups_mut(doc: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::ArrayOfTables, String> {
    if doc.get("group").is_none() {
        doc.insert("group", toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    }
    doc.get_mut("group")
        .and_then(|g| g.as_array_of_tables_mut())
        .ok_or_else(|| "settings.toml: group must be written as [[group]] tables".to_string())
}

/// The user's group entry, created (with just its name) if needed.
fn user_group<'a>(doc: &'a mut toml_edit::DocumentMut, name: &str) -> Result<&'a mut toml_edit::Table, String> {
    let idx = group_index(doc, name);
    let groups = groups_mut(doc)?;
    let i = match idx {
        Some(i) => i,
        None => {
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value(name));
            groups.push(t);
            groups.len() - 1
        }
    };
    Ok(groups.get_mut(i).expect("index is valid"))
}

// ---------------------------------------------------------------- the settings store

struct LoadedState {
    merged: toml::Table,
    team: Option<toml::Table>,
    user: toml::Table,
    cfg: Config,
    warnings: Vec<String>,
}

/// The settings files in one data folder. The public functions use the real data folder;
/// tests use a temp folder.
struct Store {
    dir: PathBuf,
}

impl Store {
    fn real() -> Store {
        Store { dir: crate::paths::data_dir() }
    }

    fn user_path(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }

    fn team_path(&self) -> PathBuf {
        self.dir.join("team.toml")
    }

    fn read(path: &Path) -> Result<String, String> {
        fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// Create settings.toml from the template, or migrate an untouched 0.1 default file.
    fn ensure_user_file(&self) -> Result<Option<String>, String> {
        let path = self.user_path();
        if !path.exists() {
            write_atomic(&path, DEFAULT_TOML)?;
            return Ok(None);
        }
        let text = Self::read(&path)?;
        if is_untouched_v01(&text) {
            let bak = self.dir.join("settings.toml.bak");
            fs::copy(&path, &bak).map_err(|e| format!("{}: {}", bak.display(), e))?;
            write_atomic(&path, DEFAULT_TOML)?;
            return Ok(Some(format!(
                "settings.toml was the unedited 0.1 default; it now uses the new template (old file kept as {})",
                bak.display()
            )));
        }
        Ok(None)
    }

    fn team_text(&self) -> Result<Option<String>, String> {
        let path = self.team_path();
        if path.exists() {
            Self::read(&path).map(Some)
        } else {
            Ok(None)
        }
    }

    fn user_text(&self) -> Result<String, String> {
        self.ensure_user_file()?;
        Self::read(&self.user_path())
    }

    /// Single-read, single-parse load of `settings.toml` and `team.toml` over cached `BUILTIN_TOML`.
    fn load_state(&self) -> Result<LoadedState, String> {
        let user_label = self.user_path().display().to_string();
        let mut warnings: Vec<String> = self.ensure_user_file()?.into_iter().collect();
        let user_str = Self::read(&self.user_path())?;
        let user_table = parse_table("settings.toml", &user_str)
            .map_err(|e| e.replace("settings.toml:", &format!("{}:", user_label)))?;

        let team_text = match self.team_text() {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("{}; team settings ignored", e));
                None
            }
        };

        let mut merged = builtin_table_ref().clone();
        let mut valid_team: Option<toml::Table> = None;
        if let Some(t_str) = team_text.as_deref() {
            let mut team_try = merged.clone();
            let mut team_warnings = Vec::new();
            let team_res = parse_table("team.toml", t_str).and_then(|tt| {
                layer_warnings("team.toml", &tt, &mut team_warnings);
                merge_tables(&mut team_try, &tt, true).map_err(|e| format!("team.toml: {}", e))?;
                Config::from_table(team_try.clone()).map_err(|e| format!("team.toml: {}", e))?;
                Ok(tt)
            });
            match team_res {
                Ok(tt) => {
                    merged = team_try;
                    valid_team = Some(tt);
                    warnings.extend(team_warnings);
                }
                Err(e) => {
                    warnings.push(format!("{}; team settings ignored", e));
                }
            }
        }

        layer_warnings("settings.toml", &user_table, &mut warnings);
        merge_tables(&mut merged, &user_table, true).map_err(|e| format!("{}: {}", user_label, e))?;
        let cfg = Config::from_table(merged.clone()).map_err(|e| format!("{}: {}", user_label, e))?;
        Ok(LoadedState { merged, team: valid_team, user: user_table, cfg, warnings })
    }

    fn load(&self) -> Result<(Config, Vec<String>), String> {
        let state = self.load_state()?;
        Ok((state.cfg, state.warnings))
    }

    /// Validate a new user-layer text against the current team layer.
    fn check_user(&self, text: &str) -> Result<(), String> {
        let team = self.team_text().ok().flatten();
        let team = team.filter(|t| config_from_layers(Some(t), "").is_ok());
        config_from_layers(team.as_deref(), text).map(|_| ())
    }

    fn user_doc(&self) -> Result<toml_edit::DocumentMut, String> {
        self.user_text()?.parse::<toml_edit::DocumentMut>().map_err(|e| format!("settings.toml: {}", e))
    }

    fn save_user(&self, doc: &toml_edit::DocumentMut) -> Result<(), String> {
        let text = doc.to_string();
        self.check_user(&text)?;
        write_atomic(&self.user_path(), &text)
    }

    fn set(&self, path: &str, value: toml_edit::Item) -> Result<(), String> {
        if !value.is_value() {
            return Err("settings values must be plain values (strings, numbers, booleans, arrays or inline tables)".into());
        }
        let p = SettingPath::parse(path)?;
        let mut doc = self.user_doc()?;
        match &p {
            SettingPath::Plain(keys) => set_in(doc.as_table_mut(), keys, value)?,
            SettingPath::Group { name, keys } => {
                if keys.is_empty() {
                    return Err("say which key of the group to set: group.<name>.<key>".into());
                }
                if keys[0] == "name" {
                    return Err("a group's name can't be changed; add a new group instead".into());
                }
                set_in(user_group(&mut doc, name)?, keys, value)?;
            }
        }
        self.save_user(&doc)
    }

    fn reset(&self, path: &str) -> Result<(), String> {
        let p = SettingPath::parse(path)?;
        let mut doc = self.user_doc()?;
        let changed = match &p {
            SettingPath::Plain(keys) => remove_in(doc.as_table_mut(), keys, true),
            SettingPath::Group { name, keys } => match group_index(&doc, name) {
                None => false,
                Some(i) => {
                    let groups = groups_mut(&mut doc)?;
                    if keys.is_empty() {
                        groups.remove(i);
                        true
                    } else if keys[0] == "name" {
                        return Err("to remove a group, reset group.<name> or use remove_group".into());
                    } else {
                        let g = groups.get_mut(i).expect("index is valid");
                        let removed = remove_in(g, keys, true);
                        // Nothing but the name left: the entry no longer changes anything.
                        // Keep it if the user wrote a comment on it.
                        if removed && g.len() == 1 && g.contains_key("name") && !has_comment(g.decor()) {
                            groups.remove(i);
                        }
                        removed
                    }
                }
            },
        };
        if !changed {
            return Ok(());
        }
        self.save_user(&doc)
    }

    fn user_table_only(&self) -> Option<toml::Table> {
        self.user_text().ok().and_then(|t| t.parse::<toml::Table>().ok())
    }

    fn valid_team_table(&self) -> Option<toml::Table> {
        let t_str = self.team_text().ok().flatten()?;
        let tt = parse_table("team.toml", &t_str).ok()?;
        let mut team_try = builtin_table_ref().clone();
        merge_tables(&mut team_try, &tt, true).ok()?;
        Config::from_table(team_try).ok()?;
        Some(tt)
    }

    fn load_team_over_builtin(&self) -> Option<Config> {
        let t_str = self.team_text().ok().flatten()?;
        let tt = parse_table("team.toml", &t_str).ok()?;
        let mut team_try = builtin_table_ref().clone();
        merge_tables(&mut team_try, &tt, true).ok()?;
        Config::from_table(team_try).ok()
    }

    fn layer(&self, path: &str) -> Layer {
        let Ok(p) = SettingPath::parse(path) else {
            return Layer::Builtin;
        };
        if self.user_table_only().as_ref().is_some_and(|t| layer_decides(t, &p)) {
            Layer::User
        } else if self.valid_team_table().as_ref().is_some_and(|t| layer_decides(t, &p)) {
            Layer::Team
        } else {
            Layer::Builtin
        }
    }

    fn effective_table(&self) -> Result<toml::Table, String> {
        Ok(self.load_state()?.merged)
    }

    fn resolve(&self, path: &str) -> Option<Resolved<toml::Value>> {
        let p = SettingPath::parse(path).ok()?;
        let state = self.load_state().ok()?;
        let value = lookup(&state.merged, &p)?.clone();
        let from = if layer_decides(&state.user, &p) {
            Layer::User
        } else if state.team.as_ref().is_some_and(|t| layer_decides(t, &p)) {
            Layer::Team
        } else {
            Layer::Builtin
        };
        Some(Resolved { value, from })
    }

    fn effective_toml(&self) -> Result<String, String> {
        let table = self.effective_table()?;
        let body = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
        Ok(format!(
            "# Ather MemGuard settings in effect (built-in defaults, team.toml and settings.toml merged).\n\
             # Exported by Ather MemGuard {}. Import it with `memguard config import <file>`.\n\n{}",
            env!("CARGO_PKG_VERSION"),
            body
        ))
    }

    fn group_in_lower_layers(&self, name: &str) -> bool {
        find_group(builtin_table_ref(), name).is_some()
            || self.valid_team_table().as_ref().is_some_and(|t| find_group(t, name).is_some())
    }

    fn add_group(&self, name: &str, processes: &[String]) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() || name.contains('.') {
            return Err("a group needs a name without dots".into());
        }
        let procs: Vec<&str> = processes.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
        if procs.is_empty() {
            return Err("a group needs at least one process name".into());
        }
        let (cfg, _) = self.load()?;
        if cfg.groups.iter().any(|g| g.name.eq_ignore_ascii_case(name)) {
            return Err(format!("a group named \"{}\" already exists", name));
        }
        let mut doc = self.user_doc()?;
        let mut t = toml_edit::Table::new();
        t.insert("name", toml_edit::value(name));
        t.insert("processes", toml_edit::value(procs.iter().copied().collect::<toml_edit::Array>()));
        groups_mut(&mut doc)?.push(t);
        self.save_user(&doc)
    }

    fn remove_group(&self, name: &str) -> Result<(), String> {
        let mut doc = self.user_doc()?;
        if self.group_in_lower_layers(name) {
            user_group(&mut doc, name)?.insert("enabled", toml_edit::value(false));
        } else if let Some(i) = group_index(&doc, name) {
            groups_mut(&mut doc)?.remove(i);
        } else {
            return Err(format!("there is no group named \"{}\"", name));
        }
        self.save_user(&doc)
    }

    fn import(&self, path: &Path) -> Result<(), String> {
        let text = Self::read(path)?;
        self.check_user(&text).map_err(|e| format!("{} was not imported: {}", path.display(), e))?;
        let user = self.user_path();
        if user.exists() {
            let bak = self.dir.join("settings.toml.bak");
            fs::copy(&user, &bak).map_err(|e| format!("{}: {}", bak.display(), e))?;
        }
        write_atomic(&user, &text)
    }

    fn install_team(&self, path: &Path) -> Result<(), String> {
        let text = Self::read(path)?;
        config_from_layers(Some(&text), "").map_err(|e| format!("{} was not installed: {}", path.display(), e))?;
        write_atomic(&self.team_path(), &text)
    }
}

// ---------------------------------------------------------------- public editing API

/// `team.toml` in the data folder.
pub fn team_file() -> PathBuf {
    Store::real().team_path()
}

/// Whether `team.toml` exists and is valid (active policy layer).
pub fn has_valid_team_file() -> bool {
    Store::real().valid_team_table().is_some()
}

/// Effective settings (built-in < team.toml < settings.toml) and warnings worth showing:
/// a migrated 0.1 file, a team file that was skipped because it is broken, keys with no
/// effect. Errors come from settings.toml itself.
pub fn load_layered() -> Result<(Config, Vec<String>), String> {
    Store::real().load()
}

/// Set one value in settings.toml, keeping its comments. `path` is like `general.preset`,
/// `group.Browsers.trim_above_pct` or `group.Browsers.ue.trim_above_pct`. Setting one of
/// `trim_above_mb` / `trim_above_pct` removes the other from the same table. The file is
/// only written if the result is valid.
pub fn set_user_value(path: &str, value: toml_edit::Item) -> Result<(), String> {
    Store::real().set(path, value)
}

/// Remove the user's override at `path`, falling back to team.toml or the built-in value.
/// `group.<name>` removes the user's whole entry for that group.
pub fn reset_user_value(path: &str) -> Result<(), String> {
    Store::real().reset(path)
}

/// Which layer decides the value at `path`.
pub fn value_layer(path: &str) -> Layer {
    Store::real().layer(path)
}

/// The effective value at `path` and the layer it comes from. `None` if it isn't set in any
/// layer (for example the `trim_above_pct` of a table whose soft cap is set in MB).
pub fn resolve_value(path: &str) -> Option<Resolved<toml::Value>> {
    Store::real().resolve(path)
}

/// Add a new group to settings.toml.
pub fn add_group(name: &str, processes: &[String]) -> Result<(), String> {
    Store::real().add_group(name, processes)
}

/// Remove a group. Groups from the built-in defaults or team.toml can't be removed, so they
/// get `enabled = false` instead.
pub fn remove_group(name: &str) -> Result<(), String> {
    Store::real().remove_group(name)
}

/// The merged, effective settings as one TOML text.
pub fn effective_toml() -> Result<String, String> {
    Store::real().effective_toml()
}

/// Write the merged, effective settings to `path` as one TOML file.
pub fn export_to(path: &Path) -> Result<(), String> {
    write_atomic(path, &effective_toml()?)
}

/// Validate `path`, keep the current settings.toml as settings.toml.bak and replace it.
pub fn import_from(path: &Path) -> Result<(), String> {
    Store::real().import(path)
}

/// Validate `path` and install it as team.toml.
pub fn install_team_file(path: &Path) -> Result<(), String> {
    Store::real().install_team(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GB_MB: u64 = 1024;

    #[test]
    fn builtin_config_parses() {
        let c = Config::builtin();
        assert_eq!(c.groups.len(), 4);
        assert_eq!(c.groups[0].name, "Browsers");
        assert_eq!(c.groups[0].ue.as_ref().unwrap().trim_above_pct, Some(2.4));
        assert!(c.groups.iter().all(|g| g.enabled));
        assert!(!c.ue.worker_throttle);
        assert_eq!(c.general.preset, Preset::Balanced);
        assert!(c.general.auto_prime);
        assert!(c.general.check_ram_speed);
        assert!(General::default().check_ram_speed, "the struct default matches the built-in file");
        assert_eq!(c.history.keep_days, 7);
        assert_eq!(c.ue.editor_growth_alert_gb_per_hour, 6.0);
    }

    #[test]
    fn template_and_fixtures_parse() {
        let (cfg, w) = config_from_layers(None, DEFAULT_TOML).unwrap();
        assert!(w.is_empty());
        // The template only sets the preset, so it equals the built-in defaults.
        assert_eq!(cfg.groups.len(), 4);
        assert_eq!(cfg.groups[0].trim_above_pct, Some(4.8));
        for f in V01_DEFAULTS {
            assert!(Config::parse(f).is_ok());
            assert!(config_from_layers(None, f).is_ok());
            // 0.1 files never had the RAM check; it is on for them too.
            assert!(config_from_layers(None, f).unwrap().0.general.check_ram_speed);
        }
        // The template lists the RAM check, commented out; removing the # turns it off.
        assert!(cfg.general.check_ram_speed);
        assert!(DEFAULT_TOML.contains("# check_ram_speed = true"));
        let off = DEFAULT_TOML.replace("# check_ram_speed = true", "check_ram_speed = false");
        assert!(!config_from_layers(None, &off).unwrap().0.general.check_ram_speed);
        assert!(config_from_layers(None, "[general]\ncheck_ram_speed = \"yes\"").is_err(), "it is a bool");
    }

    #[test]
    fn matching() {
        assert!(name_matches("UnrealEditor*", "unrealeditor-cmd"));
        assert!(name_matches("cl", "CL"));
        assert!(!name_matches("cl", "clang-cl"));
        assert!(name_matches("Wispr Flow", "wispr flow"));
    }

    #[test]
    fn precompiled_patterns_match_like_name_matches() {
        let list: Vec<String> =
            ["UnrealEditor*", "CL", "Wispr Flow", "MsEdge", "*", "ShaderCompileWorker"].iter().map(|s| s.to_string()).collect();
        let pats = Patterns::new(&list);
        let names = ["unrealeditor-cmd", "UNREALEDITOR", "cl", "Cl", "clang-cl", "WISPR FLOW", "msedge", "MSEDGEwebview2", "", "shadercompileworker"];
        for n in names {
            assert_eq!(pats.matches(n), any_matches(&list, n), "{}", n);
            assert_eq!(pats.matches_lower(&n.to_ascii_lowercase()), any_matches(&list, n), "{}", n);
        }
        let mixed = Patterns::new(&["MsEdge".to_string(), "Discord*".to_string()]);
        assert!(mixed.matches("msedge") && mixed.matches("MSEDGE") && mixed.matches("discordPTB"));
        assert!(!mixed.matches("msedgewebview2") && !mixed.matches("Disc"));
        assert!(!Patterns::new(&[]).matches("anything"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("intervalsec = 5").is_err());
        assert!(config_from_layers(None, "intervalsec = 5").is_err());
        assert!(config_from_layers(None, "[general]\npresett = \"gentle\"").is_err());
    }

    #[test]
    fn disk_keys_from_a_newer_version_still_load() {
        let text = "[prime]\ncook = { ram_gb = 32, commit_gb = 48, disk_gb = 100 }\n\n[disk]\nenabled = true\ncritical_free_gb = 50\nwatch = [\"auto\"]\n";
        let (cfg, _) = config_from_layers(None, text).expect("a [disk] table and disk_gb must load");
        assert_eq!(cfg.prime.cook.ram_gb, 32.0);
        assert_eq!(cfg.prime.cook.disk_gb, Some(100.0));
        assert!(cfg.disk.is_some());
        // Typos elsewhere are still caught.
        assert!(config_from_layers(None, "[prime]\ncook = { ram_gb = 32, commit_gb = 48, dsk_gb = 100 }").is_err());
    }

    #[test]
    fn disk_settings_are_typed_with_defaults() {
        let b = Config::builtin();
        assert_eq!(b.disk(), DiskSettings::default(), "the built-in file matches the struct default");
        assert_eq!(b.prime.cook.disk_gb, Some(100.0));
        assert_eq!(b.prime.editor.disk_gb, Some(20.0));
        let (cfg, _) = config_from_layers(Some("[disk]\ncritical_free_gb = 40\n"), "[disk]\nproject_dirs = ['D:\\s2']\n").unwrap();
        let d = cfg.disk();
        assert_eq!(d.critical_free_gb, 40.0);
        assert_eq!(d.project_dirs, vec![r"D:\s2".to_string()]);
        assert_eq!(d.elevated_free_gb, 150.0);
        assert!(config_from_layers(None, "[disk]\ncritical_free_gbb = 40\n").is_err());
        assert!(config_from_layers(None, "[disk]\nelevated_free_pct = 140\n").is_err());
        assert!(config_from_layers(None, "[prime]\ncook = { ram_gb = 32, commit_gb = 48, disk_gb = -1 }").is_err());
        // What this build exports still has the shapes 0.2.1 accepts.
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("disk_gb = 100.0") && text.contains("[disk]"), "{}", text);
    }

    #[test]
    fn limit_resolution_mb_wins_and_pct_scales() {
        for (ram_gb, expect) in [(16u64, 786u64), (32, 1573), (64, 3146), (128, 6291)] {
            assert_eq!(limit_mb(None, Some(4.8), ram_gb * GB_MB), Some(expect), "{} GB", ram_gb);
            assert_eq!(limit_mb(Some(2000), Some(4.8), ram_gb * GB_MB), Some(2000));
        }
        assert_eq!(limit_mb(None, None, 32 * GB_MB), None);
        assert_eq!(limit_mb(None, Some(0.0), 32 * GB_MB), Some(0));
    }

    #[test]
    fn preset_scaling_and_floor() {
        assert_eq!(scale_soft_cap(0, Preset::Aggressive), 0);
        assert_eq!(scale_soft_cap(1000, Preset::Gentle), 2000);
        assert_eq!(scale_soft_cap(1000, Preset::Balanced), 1000);
        assert_eq!(scale_soft_cap(1000, Preset::Aggressive), 500);
        assert_eq!(scale_soft_cap(200, Preset::Aggressive), MIN_TRIM_MB);
        assert_eq!(scale_soft_cap(33, Preset::Balanced), MIN_TRIM_MB);
    }

    #[test]
    fn layering_merges_tables_and_groups_by_name() {
        let team = r#"
[general]
preset = "gentle"
[ue]
worker_throttle = true
[[group]]
name = "Browsers"
trim_above_mb = 4096
[[group]]
name = "Tools"
processes = ["obs64"]
priority = "low"
"#;
        let user = r#"
[general]
auto_prime = false
[[group]]
name = "Browsers"
processes = ["chrome"]
[group.ue]
trim_above_mb = 1000
[[group]]
name = "Chat"
enabled = false
"#;
        let (cfg, _) = config_from_layers(Some(team), user).unwrap();
        assert_eq!(cfg.general.preset, Preset::Gentle); // team
        assert!(!cfg.general.auto_prime); // user
        assert!(cfg.ue.worker_throttle); // team, rest of [ue] from built-in
        assert_eq!(cfg.ue.protect.len(), 29);
        assert_eq!(cfg.ue.workers.len(), 17);
        let names: Vec<&str> = cfg.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["Browsers", "Chat", "Agents", "Background", "Tools"]);
        let b = &cfg.groups[0];
        assert_eq!(b.processes, ["chrome"]);
        assert_eq!(b.priority, Some(MemPriority::Low)); // built-in key kept
        // Team set MB: it replaces the built-in percentage instead of sitting beside it.
        assert_eq!((b.trim_above_mb, b.trim_above_pct), (Some(4096), None));
        let ue = b.ue.as_ref().unwrap();
        assert_eq!((ue.trim_above_mb, ue.trim_above_pct), (Some(1000), None));
        assert_eq!(b.elevated.as_ref().unwrap().cpu, Some(Cpu::Below));
        assert!(!cfg.groups[1].enabled);
        assert_eq!(cfg.active_groups().count(), 4);
    }

    #[test]
    fn layering_reports_errors_by_layer() {
        let e = config_from_layers(Some("[general]\npreset = \"fast\""), "").unwrap_err();
        assert!(e.starts_with("team.toml"), "{}", e);
        let e = config_from_layers(None, "[[group]]\nname = \"New\"").unwrap_err();
        assert!(e.contains("processes"), "{}", e);
        let e = config_from_layers(None, "[[group]]\nprocesses = [\"x\"]").unwrap_err();
        assert!(e.contains("name"), "{}", e);
        let e = config_from_layers(None, "[[group]]\nname = \"Browsers\"\ntrim_above_pct = 150").unwrap_err();
        assert!(e.contains("between 0 and 100"), "{}", e);
        let (_, w) = config_from_layers(None, "[[group]]\nname = \"Chat\"\ntrim_above_mb = 1\ntrim_above_pct = 2").unwrap();
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn untouched_01_files_are_recognised() {
        for f in V01_DEFAULTS {
            assert!(is_untouched_v01(f));
            assert!(is_untouched_v01(&f.replace('\n', "\r\n")));
        }
        assert!(!is_untouched_v01(&V01_DEFAULTS[0].replace("interval_sec = 10", "interval_sec = 5")));
        assert!(!is_untouched_v01(DEFAULT_TOML));
        // The two 0.1 Ather texts differ only where the brief says they do.
        assert!(V01_DEFAULTS[1].contains("trim_cooldown_sec = 60"));
        assert!(V01_DEFAULTS[1].contains("trim_above_mb = 768"));
    }

    // ------------------------------------------------ store tests in temp folders

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "memguard-cfg-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Store { dir }
    }

    fn user(s: &Store) -> String {
        fs::read_to_string(s.user_path()).unwrap()
    }

    const USER_WITH_COMMENTS: &str = r#"# my notes about memguard
[general]
preset = "balanced" # keep it calm

# I like my browsers small
[[group]]
name = "Browsers"
trim_above_mb = 2048 # tuned by hand
"#;

    #[test]
    fn first_load_writes_template() {
        let s = temp_store();
        let (cfg, w) = s.load().unwrap();
        assert!(w.is_empty());
        assert_eq!(user(&s), DEFAULT_TOML);
        assert_eq!(cfg.groups.len(), 4);
    }

    #[test]
    fn migration_of_both_01_defaults() {
        for f in V01_DEFAULTS {
            let s = temp_store();
            fs::write(s.user_path(), f.replace('\n', "\r\n")).unwrap();
            let (_, w) = s.load().unwrap();
            assert_eq!(w.len(), 1, "{:?}", w);
            assert_eq!(user(&s), DEFAULT_TOML);
            assert_eq!(fs::read_to_string(s.dir.join("settings.toml.bak")).unwrap(), f.replace('\n', "\r\n"));
        }
        // An edited 0.1 file stays and layers on top.
        let s = temp_store();
        let edited = V01_DEFAULTS[0].replace("trim_above_mb = 6144", "trim_above_mb = 5000");
        fs::write(s.user_path(), &edited).unwrap();
        let (cfg, w) = s.load().unwrap();
        assert!(w.is_empty());
        assert_eq!(user(&s), edited);
        assert!(!s.dir.join("settings.toml.bak").exists());
        assert_eq!(cfg.groups[0].trim_above_mb, Some(5000));
        assert_eq!(cfg.groups[0].trim_above_pct, None);
        assert_eq!(cfg.trim_cooldown_sec, 120);
    }

    #[test]
    fn setters_keep_comments_and_round_trip() {
        let s = temp_store();
        fs::write(s.user_path(), USER_WITH_COMMENTS).unwrap();

        s.set("general.preset", toml_edit::value("aggressive")).unwrap();
        s.set("general.auto_prime", toml_edit::value(false)).unwrap();
        s.set("updates.manifest_url", toml_edit::value("https://example.com/latest.json")).unwrap();
        s.set("history.keep_days", toml_edit::value(14)).unwrap();
        s.set("ue.editor_growth_alert_gb_per_hour", toml_edit::value(3.5)).unwrap();
        s.set("prime.build.ram_gb", toml_edit::value(30.0)).unwrap();
        s.set("interval_sec", toml_edit::value(5)).unwrap();
        s.set("group.Browsers.trim_above_pct", toml_edit::value(3.0)).unwrap();
        s.set("group.Browsers.ue.trim_above_pct", toml_edit::value(1.5)).unwrap();
        s.set("group.Chat.priority", toml_edit::value("low")).unwrap();

        let text = user(&s);
        for c in ["# my notes about memguard", "# keep it calm", "# I like my browsers small"] {
            assert!(text.contains(c), "lost comment {:?} in\n{}", c, text);
        }
        assert!(!text.contains("trim_above_mb = 2048"), "MB twin should be replaced:\n{}", text);
        let (cfg, _) = s.load().unwrap();
        assert_eq!(cfg.general.preset, Preset::Aggressive);
        assert!(!cfg.general.auto_prime);
        assert_eq!(cfg.updates.manifest_url, "https://example.com/latest.json");
        assert_eq!(cfg.history.keep_days, 14);
        assert_eq!(cfg.ue.editor_growth_alert_gb_per_hour, 3.5);
        assert_eq!(cfg.prime.build.ram_gb, 30.0);
        assert_eq!(cfg.prime.build.commit_gb, 40.0); // rest of the inline table from built-in
        assert_eq!(cfg.interval_sec, 5);
        assert_eq!((cfg.groups[0].trim_above_mb, cfg.groups[0].trim_above_pct), (None, Some(3.0)));
        assert_eq!(cfg.groups[0].ue.as_ref().unwrap().trim_above_pct, Some(1.5));
        assert_eq!(cfg.groups[1].priority, Some(MemPriority::Low));
        assert_eq!(s.layer("group.Chat.priority"), Layer::User);
        assert_eq!(s.layer("group.Agents.priority"), Layer::Builtin);
        assert_eq!(s.resolve("general.preset").unwrap(), Resolved { value: toml::Value::String("aggressive".into()), from: Layer::User });

        // Reset everything back.
        for p in [
            "general.preset",
            "general.auto_prime",
            "updates.manifest_url",
            "history.keep_days",
            "ue.editor_growth_alert_gb_per_hour",
            "prime.build.ram_gb",
            "interval_sec",
            "group.Browsers.trim_above_pct",
            "group.Browsers.ue.trim_above_pct",
            "group.Chat.priority",
        ] {
            s.reset(p).unwrap();
            assert_eq!(s.layer(p), Layer::Builtin, "{}", p);
        }
        let text = user(&s);
        assert!(text.contains("# my notes about memguard") && text.contains("# I like my browsers small"), "{}", text);
        assert!(!text.contains("name = \"Chat\""), "empty Chat entry should be gone:\n{}", text);
        let (cfg, _) = s.load().unwrap();
        let builtin = Config::builtin();
        assert_eq!(cfg.general.preset, Preset::Balanced);
        assert_eq!(cfg.interval_sec, builtin.interval_sec);
        assert_eq!(cfg.prime.build.ram_gb, 24.0);
        assert_eq!(cfg.groups[0].trim_above_pct, Some(4.8));
        assert_eq!(cfg.groups[0].ue.as_ref().unwrap().trim_above_pct, Some(2.4));
        // Resetting something that isn't set is a no-op.
        let before = user(&s);
        s.reset("general.notifications").unwrap();
        assert_eq!(user(&s), before);
    }

    #[test]
    fn invalid_values_leave_the_file_untouched() {
        let s = temp_store();
        fs::write(s.user_path(), USER_WITH_COMMENTS).unwrap();
        let bad: Vec<(&str, toml_edit::Item)> = vec![
            ("general.preset", toml_edit::value("fast")),
            ("general.preset", toml_edit::value(3)),
            ("general.nope", toml_edit::value(true)),
            ("history.keep_days", toml_edit::value(0)),
            ("group.Browsers.trim_above_pct", toml_edit::value(250.0)),
            ("group.Browsers.ue.trim_above_pct", toml_edit::value(-1.0)),
            ("group.Browsers.cpu", toml_edit::value("turbo")),
            ("group.NewOne.priority", toml_edit::value("low")), // new group without processes
            ("group.Browsers.name", toml_edit::value("Other")),
            ("general.prime_hotkey", toml_edit::value("M")),
            ("ue.editor_growth_alert_gb_per_hour", toml_edit::value(-2.0)),
            ("general..preset", toml_edit::value("gentle")),
            ("group.Browsers", toml_edit::value(1)),
        ];
        for (p, v) in bad {
            assert!(s.set(p, v).is_err(), "{} should be rejected", p);
            assert_eq!(user(&s), USER_WITH_COMMENTS, "file changed by {}", p);
        }
        assert!(s.add_group("", &["x".into()]).is_err());
        assert!(s.add_group("Browsers", &["x".into()]).is_err());
        assert!(s.add_group("A.B", &["x".into()]).is_err());
        assert!(s.add_group("Empty", &[]).is_err());
        assert!(s.remove_group("Nope").is_err());
        assert_eq!(user(&s), USER_WITH_COMMENTS);
        assert!(!s.dir.join("settings.toml.tmp").exists());
    }

    #[test]
    fn groups_add_and_remove() {
        let s = temp_store();
        fs::write(s.user_path(), USER_WITH_COMMENTS).unwrap();
        s.add_group("Notes", &["Obsidian".into(), "Notion.exe".into()]).unwrap();
        s.set("group.Notes.trim_above_mb", toml_edit::value(1024)).unwrap();
        let (cfg, _) = s.load().unwrap();
        let notes = cfg.groups.iter().find(|g| g.name == "Notes").unwrap();
        assert_eq!(notes.processes, ["Obsidian", "Notion"]);
        assert_eq!(notes.trim_above_mb, Some(1024));
        assert_eq!(s.layer("group.Notes.processes"), Layer::User);

        s.remove_group("Notes").unwrap();
        s.remove_group("Chat").unwrap();
        let text = user(&s);
        assert!(!text.contains("Notes"));
        assert!(text.contains("# I like my browsers small"));
        let (cfg, _) = s.load().unwrap();
        assert!(cfg.groups.iter().all(|g| g.name != "Notes"));
        assert!(!cfg.groups.iter().find(|g| g.name == "Chat").unwrap().enabled);
        // Reset the whole entry: Chat is back.
        s.reset("group.Chat").unwrap();
        let (cfg, _) = s.load().unwrap();
        assert!(cfg.groups.iter().find(|g| g.name == "Chat").unwrap().enabled);
    }

    #[test]
    fn team_layer_and_value_layer() {
        let s = temp_store();
        fs::write(s.user_path(), DEFAULT_TOML).unwrap();
        let team_src = s.dir.join("incoming-team.toml");
        fs::write(&team_src, "[general]\nnotifications = false\n[[group]]\nname = \"Browsers\"\ntrim_above_mb = 3000\n").unwrap();
        s.install_team(&team_src).unwrap();
        assert!(s.team_path().exists());
        let (cfg, _) = s.load().unwrap();
        assert!(!cfg.general.notifications);
        assert_eq!(s.layer("general.notifications"), Layer::Team);
        assert_eq!(s.layer("general.preset"), Layer::User); // the template sets it
        assert_eq!(s.layer("group.Browsers.trim_above_mb"), Layer::Team);
        assert_eq!(s.layer("group.Browsers.trim_above_pct"), Layer::Team); // decided by its MB twin
        assert!(s.resolve("group.Browsers.trim_above_pct").is_none());

        s.set("group.Browsers.trim_above_pct", toml_edit::value(2.0)).unwrap();
        let (cfg, _) = s.load().unwrap();
        assert_eq!((cfg.groups[0].trim_above_mb, cfg.groups[0].trim_above_pct), (None, Some(2.0)));
        assert_eq!(s.layer("group.Browsers.trim_above_mb"), Layer::User);
        s.reset("group.Browsers.trim_above_pct").unwrap();
        assert_eq!(s.load().unwrap().0.groups[0].trim_above_mb, Some(3000));

        // A built-in group from the team layer can't be removed, only disabled.
        s.remove_group("Browsers").unwrap();
        assert!(!s.load().unwrap().0.groups[0].enabled);

        // Broken team files are refused, and a broken installed one is skipped with a warning.
        fs::write(&team_src, "[general]\npreset = 7\n").unwrap();
        assert!(s.install_team(&team_src).is_err());
        fs::write(s.team_path(), "[general]\npreset = 7\n").unwrap();
        let (cfg, w) = s.load().unwrap();
        assert!(cfg.general.notifications);
        assert!(w.iter().any(|w| w.contains("team settings ignored")), "{:?}", w);
    }

    #[test]
    fn export_and_import() {
        let s = temp_store();
        fs::write(s.user_path(), USER_WITH_COMMENTS).unwrap();
        s.set("general.preset", toml_edit::value("gentle")).unwrap();
        let exported = s.effective_toml().unwrap();
        // The export is a complete, standalone settings file.
        let alone = Config::parse(&exported).unwrap();
        assert_eq!(alone.general.preset, Preset::Gentle);
        assert_eq!(alone.groups.len(), 4);
        assert_eq!(alone.groups[0].trim_above_mb, Some(2048));

        let other = temp_store();
        fs::write(other.user_path(), "[general]\npreset = \"aggressive\"\n").unwrap();
        let file = other.dir.join("export.toml");
        fs::write(&file, &exported).unwrap();
        other.import(&file).unwrap();
        assert_eq!(fs::read_to_string(other.dir.join("settings.toml.bak")).unwrap(), "[general]\npreset = \"aggressive\"\n");
        assert_eq!(other.load().unwrap().0.general.preset, Preset::Gentle);

        fs::write(&file, "interval = 3").unwrap();
        let before = user(&other);
        assert!(other.import(&file).is_err());
        assert_eq!(user(&other), before);
    }

    #[test]
    fn empty_parse_defaults_and_avenue2_knobs() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.trim_cooldown_sec, 120);
        assert_eq!(cfg.foreground_grace_sec, 45);
        assert!(!cfg.general.elevated_helper);
        assert_eq!(cfg.pressure.hysteresis_pct, 2.0);
        assert_eq!(cfg.pressure.hysteresis_gb, 2.0);
        assert_eq!(cfg.doctor, DoctorSettings::default());

        let builtin = Config::builtin();
        for tool in ["UnrealHeaderTool", "UnrealLightmass", "UnrealPak", "UbaHost", "UbaAgent", "LiveCodingConsole", "dxc", "fxc", "ispc", "rc", "lib", "llvm-lib"] {
            assert!(builtin.ue.protect.iter().any(|p| p == tool), "protect missing {}", tool);
            assert!(builtin.ue.workers.iter().any(|w| w == tool), "workers missing {}", tool);
        }
        assert_eq!(builtin.groups[3].io_priority, Some(IoPriority::VeryLow));

        let s = temp_store();
        fs::write(s.user_path(), DEFAULT_TOML).unwrap();
        s.set("foreground_grace_sec", toml_edit::value(60)).unwrap();
        s.set("general.elevated_helper", toml_edit::value(true)).unwrap();
        s.set("pressure.hysteresis_pct", toml_edit::value(3.5)).unwrap();
        s.set("pressure.hysteresis_gb", toml_edit::value(4.0)).unwrap();
        s.set("group.Browsers.io_priority", toml_edit::value("low")).unwrap();
        s.set("group.Browsers.ws_cap_mb", toml_edit::value(2048)).unwrap();
        s.set("group.Browsers.ws_cap_per_process_mb", toml_edit::value(512)).unwrap();
        s.set("group.Browsers.cpu_rate_pct", toml_edit::value(30)).unwrap();
        s.set("doctor.keep", toml_edit::value(toml_edit::Array::from_iter(["Steam.exe"]))).unwrap();
        s.set("doctor.nudge_above_gb", toml_edit::value(6.5)).unwrap();

        let (loaded, _) = s.load().unwrap();
        assert_eq!(loaded.foreground_grace_sec, 60);
        assert!(loaded.general.elevated_helper);
        assert_eq!(loaded.pressure.hysteresis_pct, 3.5);
        assert_eq!(loaded.pressure.hysteresis_gb, 4.0);
        assert_eq!(loaded.groups[0].io_priority, Some(IoPriority::Low));
        assert_eq!(loaded.groups[0].ws_cap_mb, 2048);
        assert_eq!(loaded.groups[0].ws_cap_per_process_mb, 512);
        assert_eq!(loaded.groups[0].cpu_rate_pct, Some(30));
        assert_eq!(loaded.doctor.keep, vec!["Steam".to_string()]);
        assert_eq!(loaded.doctor.nudge_above_gb, 6.5);
        assert_eq!(s.resolve("doctor.ignore_below_mb").unwrap().from, Layer::Builtin);
        assert_eq!(s.resolve("doctor.keep").unwrap().from, Layer::User);

        assert!(s.set("group.Browsers.cpu_rate_pct", toml_edit::value(150)).is_err());
        assert!(s.set("foreground_grace_sec", toml_edit::value(4000)).is_err());
        assert!(s.set("pressure.hysteresis_pct", toml_edit::value(80.0)).is_err());
        assert!(s.set("doctor.nudge_above_gb", toml_edit::value(-1.0)).is_err());
    }

    #[test]
    fn compiled_patterns_and_write_atomic_bytes_work() {
        let cfg = Config::builtin();
        let compiled = cfg.compile_patterns();
        assert!(compiled.protect.matches("UnrealEditor-Win64-Development"));
        assert!(compiled.workers.matches("UbaAgent"));
        assert_eq!(compiled.first_group_lower("chrome"), Some(0));
        assert_eq!(compiled.first_group_lower("slack"), Some(1));
        assert_eq!(compiled.first_group_lower("codex"), Some(2));
        assert_eq!(compiled.first_group_lower("spotify"), Some(3));
        assert_eq!(compiled.first_group_lower("notepad"), None);

        // Subsumed prefix + exact matching.
        let p = Patterns::new(&["UnrealEditor*".into(), "Unreal*".into(), "UnrealBuildTool".into(), "cl".into()]);
        assert!(!p.is_empty());
        assert!(p.matches("UnrealBuildTool"));
        assert!(p.matches("cl"));
        assert!(!p.matches("clang-cl"));

        let s = temp_store();
        let target = s.dir.join("blob.bin");
        write_atomic_bytes(&target, b"first").unwrap();
        write_atomic_bytes(&target, b"second").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
        let leftover: Vec<_> = fs::read_dir(&s.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "leftover tmp files: {:?}", leftover);
    }

    #[test]
    fn invalid_team_toml_is_ignored_consistently_and_empty_doctor_exe_rejected() {
        let s = temp_store();
        fs::write(s.user_path(), "").unwrap();
        // Syntactically valid TOML, but invalid config (unknown preset + group missing processes).
        fs::write(
            s.team_path(),
            "[general]\npreset = \"fast\"\n\n[[group]]\nname = \"BrokenTeamGrp\"\n",
        )
        .unwrap();

        let resolved = s.resolve("general.preset").expect("resolve preset");
        assert_eq!(resolved.from, Layer::Builtin);
        assert_eq!(resolved.value.as_str(), Some("balanced"));
        assert_eq!(s.layer("general.preset"), Layer::Builtin);
        assert!(!s.group_in_lower_layers("BrokenTeamGrp"));
        let err = s.remove_group("BrokenTeamGrp").unwrap_err();
        assert!(err.contains("there is no group named"), "unexpected error: {}", err);

        // Empty or ".exe"-only entries in doctor lists are rejected after .exe stripping.
        assert!(Config::parse("[doctor]\nkeep = [\".exe\"]\n").is_err());
        assert!(Config::parse("[doctor]\nclose = [\"\"]\n").is_err());
        assert!(Config::parse("[doctor]\nclose_while_unreal = [\"  .EXE\"]\n").is_err());
    }
}

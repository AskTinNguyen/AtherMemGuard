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

/// The defaults compiled into the program. Every key is set here; the files on disk only
/// override. Soft caps are percentages of total RAM so they suit 16 GB and 128 GB PCs alike.
pub const BUILTIN_TOML: &str = r#"# Ather MemGuard built-in defaults.
interval_sec = 10
trim_cooldown_sec = 120

[general]
preset = "balanced"
notifications = true
auto_prime = true
prime_hotkey = "Ctrl+Alt+M"

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

[ue]
protect = [
    "UnrealEditor*", "ShaderCompileWorker", "UnrealBuildTool", "dotnet", "cl", "link",
    "clang-cl", "lld-link", "UbaAgent", "UbaCli", "UbaHost", "zenserver", "UnrealPak",
    "LiveCodingConsole", "UnrealTraceServer", "CrashReportClient*", "EpicWebHelper",
    "AutomationTool", "msbuild", "git", "git-lfs",
]
workers = ["cl", "clang-cl", "link", "lld-link", "ShaderCompileWorker"]
editor_restart_hint_gb = 48
editor_growth_alert_gb_per_hour = 6
worker_throttle = false
min_workers = 4

[prime]
editor = { ram_gb = 16, commit_gb = 24 }
build = { ram_gb = 24, commit_gb = 40 }
cook = { ram_gb = 32, commit_gb = 48 }

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
[group.critical]
trim_above_pct = 0.05

[[group]]
name = "Agents"
processes = ["claude", "codex"]
priority = "low"
[group.elevated]
trim_above_pct = 4.8
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

# [pressure]
# Percent of total RAM that is free / percent of the commit limit that is free.
# elevated_ram_free_pct = 10
# elevated_commit_free_pct = 12
# critical_ram_free_pct = 4
# critical_commit_free_pct = 6

# [ue]
# Never touched by any group, whatever the group lists say. A trailing * is a wildcard.
# protect = ["UnrealEditor*", "ShaderCompileWorker", "UnrealBuildTool", "dotnet", "cl", ...]
# Compile / shader workers, counted in status and used by worker_throttle.
# workers = ["cl", "clang-cl", "link", "lld-link", "ShaderCompileWorker"]
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
# editor = { ram_gb = 16, commit_gb = 24 }
# build = { ram_gb = 24, commit_gb = 40 }
# cook = { ram_gb = 32, commit_gb = 48 }

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
#   suspend         true freezes the apps completely until pressure drops. Use with care.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspend: Option<bool>,
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
    pub suspend: Option<bool>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pressure {
    pub elevated_ram_free_pct: f64,
    pub elevated_commit_free_pct: f64,
    pub critical_ram_free_pct: f64,
    pub critical_commit_free_pct: f64,
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
}

impl Default for General {
    fn default() -> Self {
        General { preset: Preset::Balanced, notifications: true, auto_prime: true, prime_hotkey: "Ctrl+Alt+M".into() }
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_interval")]
    pub interval_sec: u64,
    #[serde(default = "default_cooldown")]
    pub trim_cooldown_sec: u64,
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
    /// The checkup's own settings (keep / close lists, thresholds). `doctor.rs` reads and
    /// validates them itself; the table is carried here so the file still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor: Option<toml::Table>,
    /// Every group, including disabled ones (`enabled = false`); the engine skips those.
    #[serde(default, rename = "group")]
    pub groups: Vec<Group>,
}

fn default_interval() -> u64 {
    10
}
fn default_cooldown() -> u64 {
    60
}
fn default_pressure() -> Pressure {
    Config::builtin().pressure
}
fn default_ue() -> Ue {
    Config::builtin().ue
}
fn default_prime() -> Prime {
    Config::builtin().prime
}

impl Config {
    /// The defaults compiled into the program.
    pub fn builtin() -> Config {
        Config::parse(BUILTIN_TOML).expect("built-in default config is valid")
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
                if p.to_ascii_lowercase().ends_with(".exe") {
                    p.truncate(p.len() - 4);
                }
            }
        }
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), String> {
        let pct_ok = |v: Option<f64>| v.map_or(true, |p| p.is_finite() && (0.0..=100.0).contains(&p));
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
        let hk = self.general.prime_hotkey.trim();
        if !hk.is_empty() {
            let parts: Vec<&str> = hk.split('+').map(str::trim).collect();
            if parts.len() < 2 || parts.iter().any(|s| s.is_empty()) {
                return Err(format!("general.prime_hotkey \"{}\" should look like \"Ctrl+Alt+M\", or be empty", hk));
            }
        }
        let url = self.updates.manifest_url.trim();
        if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("updates.manifest_url must be empty or start with https://".into());
        }
        Ok(())
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

    /// Load settings, falling back to the built-in defaults if a file is broken.
    pub fn load_or_default() -> (Config, Option<String>) {
        match Config::load() {
            Ok(c) => (c, None),
            Err(e) => (Config::builtin(), Some(e)),
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

/// The merged table for built-in + the given layers, in order.
fn merge_layers(layers: &[(&str, &str)], warnings: &mut Vec<String>) -> Result<toml::Table, String> {
    let mut merged = parse_table("built-in defaults", BUILTIN_TOML)?;
    for (label, text) in layers {
        let t = parse_table(label, text)?;
        layer_warnings(label, &t, warnings);
        merge_tables(&mut merged, &t, true).map_err(|e| format!("{}: {}", label, e))?;
    }
    Ok(merged)
}

/// Pure layering: built-in, then `team` (if any), then `user`. Errors name the layer at fault.
pub fn config_from_layers(team: Option<&str>, user: &str) -> Result<(Config, Vec<String>), String> {
    let mut warnings = Vec::new();
    let mut layers: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = team {
        layers.push(("team.toml", t));
        // Check the team layer on its own first, so a mistake there is reported as such.
        let mut w = Vec::new();
        Config::from_table(merge_layers(&layers, &mut w)?).map_err(|e| format!("team.toml: {}", e))?;
    }
    layers.push(("settings.toml", user));
    let merged = merge_layers(&layers, &mut warnings)?;
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

/// Write `text` to `path` through a temp file and a rename, so readers never see half a file.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {}", path.display(), e)
    })
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

    fn load(&self) -> Result<(Config, Vec<String>), String> {
        let mut warnings: Vec<String> = self.ensure_user_file()?.into_iter().collect();
        let user = Self::read(&self.user_path())?;
        let team = match self.team_text() {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("{}; team settings ignored", e));
                None
            }
        };
        let result = match config_from_layers(team.as_deref(), &user) {
            Err(e) if team.is_some() && e.starts_with("team.toml") => {
                warnings.push(format!("{}; team settings ignored", e));
                config_from_layers(None, &user)
            }
            r => r,
        };
        let (cfg, w) = result.map_err(|e| e.replace("settings.toml:", &format!("{}:", self.user_path().display())))?;
        warnings.extend(w);
        Ok((cfg, warnings))
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

    fn layer_tables(&self) -> (Option<toml::Table>, Option<toml::Table>) {
        let user = self.user_text().ok().and_then(|t| t.parse::<toml::Table>().ok());
        let team = self.team_text().ok().flatten().and_then(|t| t.parse::<toml::Table>().ok());
        (team, user)
    }

    fn layer(&self, path: &str) -> Layer {
        let Ok(p) = SettingPath::parse(path) else {
            return Layer::Builtin;
        };
        let (team, user) = self.layer_tables();
        if user.as_ref().is_some_and(|t| layer_decides(t, &p)) {
            Layer::User
        } else if team.as_ref().is_some_and(|t| layer_decides(t, &p)) {
            Layer::Team
        } else {
            Layer::Builtin
        }
    }

    fn effective_table(&self) -> Result<toml::Table, String> {
        // Load first so the same rules apply (a broken team file is skipped).
        let (_, warnings) = self.load()?;
        let team_ok = !warnings.iter().any(|w| w.contains("team settings ignored"));
        let user = Self::read(&self.user_path())?;
        let team = if team_ok { self.team_text()? } else { None };
        let mut layers = Vec::new();
        if let Some(t) = team.as_deref() {
            layers.push(("team.toml", t));
        }
        layers.push(("settings.toml", user.as_str()));
        merge_layers(&layers, &mut Vec::new())
    }

    fn resolve(&self, path: &str) -> Option<Resolved<toml::Value>> {
        let p = SettingPath::parse(path).ok()?;
        let merged = self.effective_table().ok()?;
        let value = lookup(&merged, &p)?.clone();
        Some(Resolved { value, from: self.layer(path) })
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
        let builtin = parse_table("built-in defaults", BUILTIN_TOML).expect("built-in defaults parse");
        let (team, _) = self.layer_tables();
        find_group(&builtin, name).is_some() || team.as_ref().is_some_and(|t| find_group(t, name).is_some())
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
        }
    }

    #[test]
    fn matching() {
        assert!(name_matches("UnrealEditor*", "unrealeditor-cmd"));
        assert!(name_matches("cl", "CL"));
        assert!(!name_matches("cl", "clang-cl"));
        assert!(name_matches("Wispr Flow", "wispr flow"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("intervalsec = 5").is_err());
        assert!(config_from_layers(None, "intervalsec = 5").is_err());
        assert!(config_from_layers(None, "[general]\npresett = \"gentle\"").is_err());
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
        assert_eq!(cfg.ue.protect.len(), 21);
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
}

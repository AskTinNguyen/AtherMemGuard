use serde::{Deserialize, Serialize};
use std::fs;

/// Written to settings.toml on first run. Also the source of the built-in defaults.
pub const DEFAULT_TOML: &str = r#"# AtherMemGuard settings. Saved changes are picked up within one interval.
#
# How it decides what to do
#   mode      what Unreal is doing: idle, editor, build, cook (several can be true at once)
#   pressure  normal / elevated / critical, from free RAM and free commit (see [pressure])
#
# Each [[group]] is a set of apps. Its base settings always apply. The optional
# [group.ue], [group.elevated] and [group.critical] tables override them while
# Unreal is running, and while memory pressure is elevated or critical.
#
#   priority       normal | belownormal | medium | low | verylow
#                  Windows takes RAM from lower-priority apps first. No crash risk.
#   trim_above_mb  soft cap: when the group's RAM is above this, push its memory to the
#                  compressed store / page file. Frees RAM, not commit. 0 = off.
#   cpu            normal | below | idle | eco   (eco = Windows efficiency mode)
#   suspend        true freezes the apps completely until pressure drops. Use with care.
#   hard_cap_mb    hard cap on the group's committed memory (Job Object). Apps that hit it
#                  usually crash. Sandboxed browser/Electron helpers can't be capped. 0 = off.
#   hard_cap_per_process_mb   same, per process.

interval_sec = 10
# Minimum time between trims of the same group. It doubles (up to 16x) while a group keeps
# climbing straight back over its limit, and is ignored at critical pressure.
trim_cooldown_sec = 120

[pressure]
# Percent of total RAM that is free / percent of the commit limit that is free.
elevated_ram_free_pct = 10
elevated_commit_free_pct = 12
critical_ram_free_pct = 4
critical_commit_free_pct = 6

[ue]
# Never touched by any group, whatever the group lists say. A trailing * is a wildcard.
protect = [
    "UnrealEditor*", "ShaderCompileWorker", "UnrealBuildTool", "dotnet", "cl", "link",
    "clang-cl", "lld-link", "UbaAgent", "UbaCli", "UbaHost", "zenserver", "UnrealPak",
    "LiveCodingConsole", "UnrealTraceServer", "CrashReportClient*", "EpicWebHelper",
    "AutomationTool", "msbuild", "git", "git-lfs",
]
# Compile / shader workers, counted in status and used by worker_throttle.
workers = ["cl", "clang-cl", "link", "lld-link", "ShaderCompileWorker"]
# Suggest restarting the editor when one editor process commits more than this.
editor_restart_hint_gb = 48
# Opt-in. At critical pressure, pause the newest workers (never fewer than min_workers
# keep running) and resume them when pressure drops. Slows the build instead of
# letting it die with out-of-memory errors.
worker_throttle = false
min_workers = 4

[prime]
# Headroom `memguard prime --for X` and `memguard wait --for X` aim for. Starting points:
# tune them to your project.
editor = { ram_gb = 16, commit_gb = 24 }
build = { ram_gb = 24, commit_gb = 40 }
cook = { ram_gb = 32, commit_gb = 48 }

[[group]]
name = "Browsers"
processes = ["chrome", "msedge", "firefox", "brave", "msedgewebview2"]
priority = "low"
trim_above_mb = 6144
[group.ue]
trim_above_mb = 3072
[group.elevated]
trim_above_mb = 1024
cpu = "below"
[group.critical]
trim_above_mb = 256

[[group]]
name = "Chat"
processes = ["Slack", "ChatGPT", "Wispr Flow", "Discord", "Teams", "ms-teams", "Telegram", "Zoom"]
priority = "verylow"
trim_above_mb = 2048
[group.ue]
trim_above_mb = 1536
[group.elevated]
trim_above_mb = 512
cpu = "below"
[group.critical]
trim_above_mb = 64

[[group]]
name = "Agents"
# Claude and Codex run your agents: lower priority only, trimmed just under pressure,
# never suspended or hard capped.
processes = ["claude", "codex"]
priority = "low"
[group.elevated]
trim_above_mb = 6144
[group.critical]
trim_above_mb = 2048

[[group]]
name = "Background"
processes = [
    "steam", "steamwebhelper", "EpicGamesLauncher", "OneDrive", "copilotapp",
    "PhoneExperienceHost", "Widgets", "WidgetService", "Spotify",
]
priority = "verylow"
trim_above_mb = 512
cpu = "eco"
[group.critical]
trim_above_mb = 64
"#;

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

/// Overrides applied on top of a group's base settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub priority: Option<MemPriority>,
    pub trim_above_mb: Option<u64>,
    pub cpu: Option<Cpu>,
    pub suspend: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub name: String,
    pub processes: Vec<String>,
    #[serde(default)]
    pub priority: Option<MemPriority>,
    #[serde(default)]
    pub trim_above_mb: Option<u64>,
    #[serde(default)]
    pub cpu: Option<Cpu>,
    #[serde(default)]
    pub suspend: Option<bool>,
    #[serde(default)]
    pub hard_cap_mb: u64,
    #[serde(default)]
    pub hard_cap_per_process_mb: u64,
    #[serde(default)]
    pub ue: Option<Policy>,
    #[serde(default)]
    pub elevated: Option<Policy>,
    #[serde(default)]
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
    #[serde(default)]
    pub worker_throttle: bool,
    #[serde(default = "default_min_workers")]
    pub min_workers: usize,
}

fn default_min_workers() -> usize {
    4
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
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_interval")]
    pub interval_sec: u64,
    #[serde(default = "default_cooldown")]
    pub trim_cooldown_sec: u64,
    #[serde(default = "default_pressure")]
    pub pressure: Pressure,
    #[serde(default = "default_ue")]
    pub ue: Ue,
    #[serde(default = "default_prime")]
    pub prime: Prime,
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
        toml::from_str(DEFAULT_TOML).expect("built-in default config is valid")
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.interval_sec = cfg.interval_sec.clamp(2, 600);
        for g in &mut cfg.groups {
            for p in &mut g.processes {
                if p.to_ascii_lowercase().ends_with(".exe") {
                    p.truncate(p.len() - 4);
                }
            }
        }
        Ok(cfg)
    }

    /// Load settings.toml, writing the default file first if there is none.
    pub fn load() -> Result<Config, String> {
        let path = crate::paths::config_file();
        if !path.exists() {
            fs::create_dir_all(crate::paths::data_dir()).map_err(|e| e.to_string())?;
            fs::write(&path, DEFAULT_TOML).map_err(|e| e.to_string())?;
        }
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
        Config::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// Load settings, falling back to the built-in defaults if the file is broken.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_config_parses() {
        let c = Config::builtin();
        assert_eq!(c.groups.len(), 4);
        assert_eq!(c.groups[0].name, "Browsers");
        assert_eq!(c.groups[0].ue.as_ref().unwrap().trim_above_mb, Some(3072));
        assert!(!c.ue.worker_throttle);
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
    }
}

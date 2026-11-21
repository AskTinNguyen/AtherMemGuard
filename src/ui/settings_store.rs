//! What the settings window needs from the settings data (Part B), as a small trait.
//!
//! The window never calls `config::*` directly. The integrator implements `SettingsStore`
//! over Part B's layered API (`load_layered`, `set_user_value`, `reset_user_value`,
//! `value_layer`, `add_group`, ...); `FakeStore` is an in-memory implementation filled
//! with the mockup's values, used by snapshots, tests and `examples/ui_demo.rs`.
//!
//! **Paths** follow docs/DESIGN.md Part B: `"general.preset"`, `"ue.protect"`,
//! `"pressure.elevated_ram_free_pct"`, `"prime.build.ram_gb"`, `"history.keep_days"`,
//! `"updates.check_automatically"`, and for groups `"group.<Name>.<key>"` (base tier,
//! "Always") or `"group.<Name>.<tier>.<key>"` with tier `ue` | `elevated` | `critical`.
//! Group keys the window uses: `priority`, `trim_above_pct`, `cpu`, `suspend`
//! (`critical.suspend` only), `hard_cap_mb`.
//!
//! **Values** use the TOML type of the key: `Bool`, `Int` (whole numbers such as
//! `hard_cap_mb`, `keep_days`, `min_workers`), `Float` (GB, percentages), `Text` (enum
//! names exactly as in settings.toml: `"balanced"`, `"verylow"`, `"below"`, hotkey
//! `"Ctrl+Alt+M"`), `List` (process names).

#[cfg(any(test, feature = "ui-snapshots"))]
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Where an effective value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Builtin,
    Team,
    User,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    List(Vec<String>),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// Numbers of either kind as f64.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
}

/// One setting as the window sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    /// The effective value (built-in defaults, then team.toml, then settings.toml).
    pub value: Value,
    /// The layer `value` comes from.
    pub layer: Layer,
    /// What `reset` falls back to and where that comes from. `None` when resetting leaves
    /// the key unset everywhere (a tier override that then inherits the tier above).
    pub fallback: Option<(Value, Layer)>,
}

/// One `[[group]]` of the effective settings.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupInfo {
    pub name: String,
    pub processes: Vec<String>,
    /// `enabled = false` groups are ignored by the engine.
    pub enabled: bool,
    /// Defined by the built-in defaults: can be turned off, not renamed or removed.
    pub builtin: bool,
    /// The layer that defines the group (Team shows a TEAM tag on the card).
    pub layer: Layer,
    /// True for groups that are only ever slowed and held back (the built-in Agents group):
    /// the window hides Freeze and Hard cap for them.
    pub never_freeze: bool,
}

/// A running app offered by "+ Add app…".
#[derive(Debug, Clone, PartialEq)]
pub struct RunningApp {
    /// Process name without ".exe", as it goes into a group's `processes`.
    pub name: String,
    pub processes: usize,
    pub ram_gb: f64,
}

/// The team file currently installed as team.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamInfo {
    /// When it was loaded, ready to show ("12 Nov 2026").
    pub loaded: String,
    /// How many settings it sets.
    pub settings: usize,
}

/// The four soft-cap tiers of a group card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Always,
    Unreal,
    Tight,
    Critical,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Always, Tier::Unreal, Tier::Tight, Tier::Critical];

    /// The settings table of this tier inside a group, `None` for the group itself.
    pub fn key(self) -> Option<&'static str> {
        match self {
            Tier::Always => None,
            Tier::Unreal => Some("ue"),
            Tier::Tight => Some("elevated"),
            Tier::Critical => Some("critical"),
        }
    }
    /// The tier whose value this one inherits when it isn't set.
    pub fn above(self) -> Option<Tier> {
        match self {
            Tier::Always => None,
            Tier::Unreal => Some(Tier::Always),
            Tier::Tight => Some(Tier::Unreal),
            Tier::Critical => Some(Tier::Tight),
        }
    }
}

/// `"group.<name>.<key>"` or `"group.<name>.<tier>.<key>"`.
pub fn group_path(group: &str, tier: Tier, key: &str) -> String {
    match tier.key() {
        None => format!("group.{}.{}", group, key),
        Some(t) => format!("group.{}.{}.{}", group, t, key),
    }
}

/// Trim whitespace and strip a trailing `.exe` (ASCII case-insensitive).
pub(crate) fn strip_exe(s: &str) -> String {
    let p = s.trim();
    if p.len() >= 4 && p.get(p.len() - 4..).is_some_and(|ext| ext.eq_ignore_ascii_case(".exe")) {
        p[..p.len() - 4].to_string()
    } else {
        p.to_string()
    }
}

/// Everything the settings window reads and writes. All writes validate and either
/// succeed completely or return a message for an error toast, leaving settings as they were.
pub trait SettingsStore {
    /// The effective value of `path` and its layer; `None` when no layer sets it.
    /// Called often while painting: keep it cheap (cache the parsed layers).
    fn get(&self, path: &str) -> Option<Setting>;
    /// Write `value` to the user layer (settings.toml).
    fn set(&mut self, path: &str, value: Value) -> Result<(), String>;
    /// Remove the user-layer value of `path`, falling back to team / built-in.
    fn reset(&mut self, path: &str) -> Result<(), String>;

    /// The effective groups, in settings order.
    fn groups(&self) -> Vec<GroupInfo>;
    fn add_group(&mut self, name: &str, processes: &[String]) -> Result<(), String>;
    /// User groups are deleted; built-in and team groups get `enabled = false`.
    fn remove_group(&mut self, name: &str) -> Result<(), String>;
    fn rename_group(&mut self, from: &str, to: &str) -> Result<(), String>;
    fn set_group_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String>;
    fn add_process(&mut self, group: &str, process: &str) -> Result<(), String>;
    fn remove_process(&mut self, group: &str, process: &str) -> Result<(), String>;

    /// Write the merged, effective settings as one TOML file.
    fn export_to(&self, path: &Path) -> Result<(), String>;
    /// Validate, back up settings.toml, replace it.
    fn import_from(&mut self, path: &Path) -> Result<(), String>;
    /// Validate and copy to team.toml.
    fn load_team_file(&mut self, path: &Path) -> Result<(), String>;
    fn remove_team_file(&mut self) -> Result<(), String>;
    fn team_info(&self) -> Option<TeamInfo>;
    /// Back up settings.toml and start again from the template.
    fn reset_all(&mut self) -> Result<(), String>;

    /// The autostart registry value (not part of settings.toml).
    fn run_at_startup(&self) -> bool;
    fn set_run_at_startup(&mut self, on: bool) -> Result<(), String>;

    /// Running processes that are in no group and not protected, sorted by RAM, largest first.
    fn running_apps(&self) -> Vec<RunningApp>;
    /// settings.toml, for "Open file" and the "Your settings" hint.
    fn settings_file(&self) -> PathBuf;
    /// Changes whenever any layer changes on disk or through this store, so open windows
    /// re-read. A file-modified-time based counter is enough.
    fn revision(&self) -> u64;
}

// ---------------------------------------------------------------- FakeStore

/// In-memory store pre-filled with the mockup's values.
#[cfg(any(test, feature = "ui-snapshots"))]
#[derive(Debug, Clone)]
pub struct FakeStore {
    pub builtin: HashMap<String, Value>,
    pub team: HashMap<String, Value>,
    pub user: HashMap<String, Value>,
    pub group_list: Vec<GroupInfo>,
    pub startup: bool,
    pub team_loaded: Option<TeamInfo>,
    pub running: Vec<RunningApp>,
    pub rev: u64,
    /// Paths whose writes fail, to exercise the error toast.
    pub fail_writes: Vec<String>,
}

#[cfg(any(test, feature = "ui-snapshots"))]
fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}
#[cfg(any(test, feature = "ui-snapshots"))]
fn list(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| s.to_string()).collect())
}

#[cfg(any(test, feature = "ui-snapshots"))]
impl Default for FakeStore {
    fn default() -> FakeStore {
        FakeStore::mockup()
    }
}

#[cfg(any(test, feature = "ui-snapshots"))]
impl FakeStore {
    /// The values drawn in docs/design/mockup.html.
    pub fn mockup() -> FakeStore {
        let mut b: HashMap<String, Value> = HashMap::new();
        let mut put = |k: &str, v: Value| {
            b.insert(k.to_string(), v);
        };
        put("interval_sec", Value::Int(10));
        put("general.preset", t("balanced"));
        put("general.auto_prime", Value::Bool(true));
        put("general.notifications", Value::Bool(true));
        put("general.prime_hotkey", t("Ctrl+Alt+M"));
        put("general.check_ram_speed", Value::Bool(true));
        put("updates.check_automatically", Value::Bool(true));
        put("history.enabled", Value::Bool(true));
        put("history.keep_days", Value::Int(7));
        put(
            "ue.protect",
            list(&[
                "UnrealEditor*", "ShaderCompileWorker", "dotnet", "cl", "link", "UnrealBuildTool", "clang-cl", "lld-link", "UbaAgent", "UbaCli",
                "UbaHost", "zenserver", "UnrealPak", "LiveCodingConsole", "UnrealTraceServer", "CrashReportClient*", "EpicWebHelper",
                "AutomationTool", "msbuild", "git", "git-lfs", "UnrealInsights",
            ]),
        );
        put("ue.editor_restart_hint_gb", Value::Float(48.0));
        put("ue.editor_growth_alert_gb_per_hour", Value::Float(6.0));
        put("ue.worker_throttle", Value::Bool(false));
        put("ue.min_workers", Value::Int(4));
        put("pressure.elevated_ram_free_pct", Value::Float(10.0));
        put("pressure.elevated_commit_free_pct", Value::Float(12.0));
        put("pressure.critical_ram_free_pct", Value::Float(4.0));
        put("pressure.critical_commit_free_pct", Value::Float(6.0));
        for (w, ram, commit, disk) in [("editor", 16.0, 24.0, 20.0), ("build", 24.0, 40.0, 50.0), ("cook", 32.0, 48.0, 100.0)] {
            put(&format!("prime.{}.ram_gb", w), Value::Float(ram));
            put(&format!("prime.{}.commit_gb", w), Value::Float(commit));
            put(&format!("prime.{}.disk_gb", w), Value::Float(disk));
        }
        put("doctor.keep", list(&[]));
        put("doctor.close", list(&[]));
        put("doctor.close_while_unreal", list(&[]));
        put("doctor.ignore_below_mb", Value::Int(150));
        put("doctor.review_above_mb", Value::Int(1024));
        put("doctor.nudge_above_gb", Value::Float(4.0));
        put("disk.enabled", Value::Bool(true));
        put("disk.watch", list(&["auto"]));
        put("disk.elevated_free_gb", Value::Float(150.0));
        put("disk.elevated_free_pct", Value::Float(10.0));
        put("disk.critical_free_gb", Value::Float(50.0));
        put("disk.critical_free_pct", Value::Float(5.0));
        put("disk.cache_min_free_gb", Value::Float(20.0));
        put("disk.project_dirs", list(&[]));
        put("disk.lane_dirs", list(&[]));
        put("disk.scan_depth", Value::Int(3));
        put("disk.scan_budget_sec", Value::Int(60));
        put("disk.scan_threads", Value::Int(4));
        put("disk.fresh_sec", Value::Int(300));
        put("disk.min_age_hours", Value::Float(2.0));
        put("disk.hold_hours", Value::Float(24.0));
        put("disk.keep", list(&[]));
        put("disk.check_dev_drive", Value::Bool(true));
        // Built-in groups (percent of RAM, Part B's v2 defaults).
        put("group.Browsers.priority", t("low"));
        put("group.Browsers.trim_above_pct", Value::Float(4.8));
        put("group.Browsers.ue.trim_above_pct", Value::Float(2.4));
        put("group.Browsers.elevated.trim_above_pct", Value::Float(0.8));
        put("group.Browsers.elevated.cpu", t("below"));
        put("group.Browsers.critical.trim_above_pct", Value::Float(0.2));
        put("group.Chat.priority", t("verylow"));
        put("group.Chat.trim_above_pct", Value::Float(1.6));
        put("group.Chat.ue.trim_above_pct", Value::Float(1.2));
        put("group.Chat.elevated.trim_above_pct", Value::Float(0.4));
        put("group.Chat.elevated.cpu", t("below"));
        put("group.Chat.critical.trim_above_pct", Value::Float(0.1));
        put("group.Background.priority", t("verylow"));
        put("group.Background.trim_above_pct", Value::Float(0.4));
        put("group.Background.cpu", t("eco"));
        put("group.Background.critical.trim_above_pct", Value::Float(0.1));

        let mut team: HashMap<String, Value> = HashMap::new();
        team.insert("group.Agents.priority".into(), t("low"));
        team.insert("group.Agents.elevated.trim_above_pct".into(), Value::Float(4.8));
        team.insert("group.Agents.critical.trim_above_pct".into(), Value::Float(1.6));
        team.insert("history.keep_days".into(), Value::Int(14));

        let mut user: HashMap<String, Value> = HashMap::new();
        user.insert("general.prime_hotkey".into(), t("Ctrl+Alt+M"));

        let group = |name: &str, procs: &[&str], builtin: bool, layer: Layer, never_freeze: bool| GroupInfo {
            name: name.to_string(),
            processes: procs.iter().map(|s| s.to_string()).collect(),
            enabled: true,
            builtin,
            layer,
            never_freeze,
        };
        let app = |name: &str, processes: usize, ram_gb: f64| RunningApp { name: name.to_string(), processes, ram_gb };
        FakeStore {
            builtin: b,
            team,
            user,
            group_list: vec![
                group("Browsers", &["chrome", "msedge", "firefox", "brave", "msedgewebview2"], true, Layer::Builtin, false),
                group("Chat", &["Slack", "ChatGPT", "Wispr Flow", "Discord", "Teams", "ms-teams", "Telegram", "Zoom"], true, Layer::Builtin, false),
                group("Agents", &["claude", "codex"], false, Layer::Team, true),
                group(
                    "Background",
                    &["steam", "steamwebhelper", "EpicGamesLauncher", "OneDrive", "copilotapp", "PhoneExperienceHost", "Widgets", "WidgetService", "Spotify"],
                    true,
                    Layer::Builtin,
                    false,
                ),
            ],
            startup: true,
            team_loaded: Some(TeamInfo { loaded: "12 Nov 2026".into(), settings: 4 }),
            running: vec![
                app("Figma", 9, 2.3),
                app("Code", 14, 1.8),
                app("obs64", 1, 0.9),
                app("Notion", 6, 0.7),
                app("PowerToys", 11, 0.4),
            ],
            rev: 1,
            fail_writes: Vec::new(),
        }
    }

    fn check(&self, path: &str) -> Result<(), String> {
        if self.fail_writes.iter().any(|p| p == path) {
            return Err(format!("Couldn't save {}: the file is read-only.", path));
        }
        Ok(())
    }

    fn group_mut(&mut self, name: &str) -> Result<&mut GroupInfo, String> {
        self.group_list.iter_mut().find(|g| g.name == name).ok_or_else(|| format!("There is no group called {}.", name))
    }

    fn bump(&mut self) {
        self.rev += 1;
    }
}

#[cfg(any(test, feature = "ui-snapshots"))]
impl SettingsStore for FakeStore {
    fn get(&self, path: &str) -> Option<Setting> {
        let layers = [(Layer::User, &self.user), (Layer::Team, &self.team), (Layer::Builtin, &self.builtin)];
        let mut found = layers.iter().filter_map(|(l, m)| m.get(path).map(|v| (v.clone(), *l)));
        let (value, layer) = found.next()?;
        Some(Setting { value, layer, fallback: found.next() })
    }

    fn set(&mut self, path: &str, value: Value) -> Result<(), String> {
        self.check(path)?;
        self.user.insert(path.to_string(), value);
        self.bump();
        Ok(())
    }

    fn reset(&mut self, path: &str) -> Result<(), String> {
        self.check(path)?;
        self.user.remove(path);
        self.bump();
        Ok(())
    }

    fn groups(&self) -> Vec<GroupInfo> {
        self.group_list.clone()
    }

    fn add_group(&mut self, name: &str, processes: &[String]) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() || name.contains('.') {
            return Err("A group needs a name without dots.".into());
        }
        if self.group_list.iter().any(|g| g.name.eq_ignore_ascii_case(name)) {
            return Err(format!("There is already a group called {}.", name));
        }
        let mut procs: Vec<String> = Vec::new();
        for p in processes {
            let n = strip_exe(p);
            if !n.is_empty() && !procs.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                procs.push(n);
            }
        }
        if procs.is_empty() {
            return Err("a group needs at least one process name".into());
        }
        self.group_list.push(GroupInfo {
            name: name.to_string(),
            processes: procs,
            enabled: true,
            builtin: false,
            layer: Layer::User,
            never_freeze: false,
        });
        self.bump();
        Ok(())
    }

    fn remove_group(&mut self, name: &str) -> Result<(), String> {
        let g = self.group_mut(name)?;
        if g.layer == Layer::User {
            self.group_list.retain(|g| g.name != name);
            let prefix = format!("group.{}.", name);
            self.user.retain(|k, _| !k.starts_with(&prefix));
        } else {
            g.enabled = false;
        }
        self.bump();
        Ok(())
    }

    fn rename_group(&mut self, from: &str, to: &str) -> Result<(), String> {
        let to = to.trim();
        if to.is_empty() || to.contains('.') {
            return Err("A group needs a name without dots.".into());
        }
        if self.group_list.iter().any(|g| g.name.eq_ignore_ascii_case(to) && g.name != from) {
            return Err(format!("There is already a group called {}.", to));
        }
        let g = self.group_mut(from)?;
        if g.builtin {
            return Err(format!("{} is a built-in group: it can be turned off, not renamed.", from));
        }
        g.name = to.to_string();
        let (old, new) = (format!("group.{}.", from), format!("group.{}.", to));
        for m in [&mut self.user, &mut self.team] {
            let keys: Vec<String> = m.keys().filter(|k| k.starts_with(&old)).cloned().collect();
            for k in keys {
                let v = m.remove(&k).unwrap();
                m.insert(format!("{}{}", new, &k[old.len()..]), v);
            }
        }
        self.bump();
        Ok(())
    }

    fn set_group_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        self.group_mut(name)?.enabled = enabled;
        self.bump();
        Ok(())
    }

    fn add_process(&mut self, group: &str, process: &str) -> Result<(), String> {
        let p = strip_exe(process);
        if p.is_empty() {
            return Err("Type the app's process name, for example chrome.".into());
        }
        let g = self.group_mut(group)?;
        if !g.processes.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
            g.processes.push(p);
        }
        self.bump();
        Ok(())
    }

    fn remove_process(&mut self, group: &str, process: &str) -> Result<(), String> {
        self.group_mut(group)?.processes.retain(|p| p != process);
        self.bump();
        Ok(())
    }

    fn export_to(&self, path: &Path) -> Result<(), String> {
        let mut keys: Vec<&String> = self.builtin.keys().chain(self.team.keys()).chain(self.user.keys()).collect();
        keys.sort();
        keys.dedup();
        let mut out = String::from("# Ather MemGuard settings (fake export)\n");
        for k in keys {
            out.push_str(&format!("# {} = {:?}\n", k, self.get(k).map(|s| s.value)));
        }
        std::fs::write(path, out).map_err(|e| e.to_string())
    }

    fn import_from(&mut self, path: &Path) -> Result<(), String> {
        std::fs::metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        self.user.clear();
        self.bump();
        Ok(())
    }

    fn load_team_file(&mut self, path: &Path) -> Result<(), String> {
        std::fs::metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        self.team_loaded = Some(TeamInfo { loaded: "today".into(), settings: self.team.len() });
        self.bump();
        Ok(())
    }

    fn remove_team_file(&mut self) -> Result<(), String> {
        self.team.clear();
        self.team_loaded = None;
        for g in &mut self.group_list {
            if g.layer == Layer::Team {
                g.layer = Layer::User;
            }
        }
        self.bump();
        Ok(())
    }

    fn team_info(&self) -> Option<TeamInfo> {
        self.team_loaded.clone()
    }

    fn reset_all(&mut self) -> Result<(), String> {
        self.user.clear();
        self.group_list.retain(|g| g.layer != Layer::User);
        for g in &mut self.group_list {
            g.enabled = true;
        }
        self.bump();
        Ok(())
    }

    fn run_at_startup(&self) -> bool {
        self.startup
    }

    fn set_run_at_startup(&mut self, on: bool) -> Result<(), String> {
        self.startup = on;
        self.bump();
        Ok(())
    }

    fn running_apps(&self) -> Vec<RunningApp> {
        let mut v = self.running.clone();
        v.retain(|a| !self.group_list.iter().any(|g| g.processes.iter().any(|p| p.eq_ignore_ascii_case(&a.name))));
        v.sort_by(|a, b| b.ram_gb.partial_cmp(&a.ram_gb).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    fn settings_file(&self) -> PathBuf {
        PathBuf::from(r"C:\Users\you\AppData\Roaming\AtherMemGuard\settings.toml")
    }

    fn revision(&self) -> u64 {
        self.rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_and_fallbacks() {
        let s = FakeStore::mockup();
        let hk = s.get("general.prime_hotkey").unwrap();
        assert_eq!(hk.layer, Layer::User);
        assert_eq!(hk.fallback, Some((Value::Text("Ctrl+Alt+M".into()), Layer::Builtin)));
        let days = s.get("history.keep_days").unwrap();
        assert_eq!((days.value.clone(), days.layer), (Value::Int(14), Layer::Team));
        assert_eq!(days.fallback, Some((Value::Int(7), Layer::Builtin)));
        assert!(s.get("group.Browsers.critical.cpu").is_none());
        assert_eq!(group_path("Browsers", Tier::Unreal, "trim_above_pct"), "group.Browsers.ue.trim_above_pct");
        assert_eq!(group_path("Browsers", Tier::Always, "cpu"), "group.Browsers.cpu");
    }

    #[test]
    fn round_trip() {
        let mut s = FakeStore::mockup();
        let r0 = s.revision();
        s.set("general.preset", Value::Text("aggressive".into())).unwrap();
        let p = s.get("general.preset").unwrap();
        assert_eq!(p.value, Value::Text("aggressive".into()));
        assert_eq!(p.layer, Layer::User);
        assert!(s.revision() > r0);
        s.reset("general.preset").unwrap();
        let p = s.get("general.preset").unwrap();
        assert_eq!((p.value, p.layer), (Value::Text("balanced".into()), Layer::Builtin));

        // A tier override appears and disappears again.
        let path = group_path("Browsers", Tier::Critical, "cpu");
        s.set(&path, Value::Text("idle".into())).unwrap();
        assert_eq!(s.get(&path).unwrap().fallback, None);
        s.reset(&path).unwrap();
        assert!(s.get(&path).is_none());

        // Groups: add, add process, rename, remove.
        assert!(s.add_group("Empty", &[]).is_err(), "empty process list rejected");
        assert!(s.add_group("Bad.Name", &["Figma".into()]).is_err(), "dot in name rejected");
        s.add_group("Design", &["Figma".into()]).unwrap();
        assert!(s.add_group("design", &["Other".into()]).is_err());
        assert!(s.rename_group("Design", "Bad.Name").is_err());
        s.add_process("Design", "Photoshop.EXE").unwrap();
        s.set(&group_path("Design", Tier::Always, "trim_above_pct"), Value::Float(2.0)).unwrap();
        s.rename_group("Design", "Art").unwrap();
        assert_eq!(s.get("group.Art.trim_above_pct").unwrap().value, Value::Float(2.0));
        let art = s.groups().into_iter().find(|g| g.name == "Art").unwrap();
        assert_eq!(art.processes, vec!["Figma".to_string(), "Photoshop".to_string()]);
        assert!(s.running_apps().iter().all(|a| a.name != "Figma"));
        s.remove_group("Art").unwrap();
        assert!(s.groups().iter().all(|g| g.name != "Art"));
        // Built-in groups are only turned off.
        assert!(s.rename_group("Browsers", "Web").is_err());
        s.remove_group("Browsers").unwrap();
        assert!(!s.groups()[0].enabled);

        s.fail_writes.push("general.notifications".into());
        assert!(s.set("general.notifications", Value::Bool(false)).is_err());
        assert_eq!(s.get("general.notifications").unwrap().value, Value::Bool(true));
    }
}

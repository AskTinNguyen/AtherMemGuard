//! The real `SettingsStore`: the settings window over Part B's layered settings API
//! (`config::set_user_value`, `reset_user_value`, `add_group`, ...), the autostart switch
//! (`install::is_run_at_startup` / `set_run_at_startup`) and the live process list.
//!
//! Reads come from a cache of the three parsed layers (built-in defaults, team.toml,
//! settings.toml). The window reads every value once a second, so the cache is refreshed only
//! when a file's modified time changes (checked at most every 250 ms) or after a write
//! through this store.
//!
//! Soft caps set in MB (`trim_above_mb`) are shown as the `trim_above_pct` they mean on this
//! PC, because the window only knows percentages. Tier keys that no layer sets are `None`, so
//! the window shows the inherited value.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::settings_store::{GroupInfo, Layer, RunningApp, Setting, SettingsStore, TeamInfo, Value};
use crate::config;

const RECHECK: Duration = Duration::from_millis(250);
const MB: u64 = 1024 * 1024;

/// The autostart switch, injectable so tests never touch the registry.
#[derive(Clone, Copy)]
pub struct Startup {
    pub get: fn() -> bool,
    pub set: fn(bool) -> Result<(), String>,
}

impl Startup {
    pub fn registry() -> Startup {
        Startup { get: crate::install::is_run_at_startup, set: crate::install::set_run_at_startup }
    }
}

/// One process for "+ Add app…": name without ".exe" and RAM in bytes.
pub type ProcessLister = fn() -> Vec<(String, u64)>;

fn live_processes() -> Vec<(String, u64)> {
    // Processes we can't open (services, elevated apps) aren't ones MemGuard can manage.
    crate::win::list_processes().into_iter().filter(|p| p.queryable).map(|p| (p.name, p.ram)).collect()
}

type Stamp = (Option<SystemTime>, Option<SystemTime>);

struct Cache {
    checked: Option<Instant>,
    stamp: Option<Stamp>,
    builtin: toml::Table,
    team: Option<toml::Table>,
    user: toml::Table,
    revision: u64,
}

pub struct RealStore {
    cache: RefCell<Cache>,
    ram_total_mb: u64,
    startup: Startup,
    processes: ProcessLister,
}

fn stamp() -> Stamp {
    let m = |p: PathBuf| fs::metadata(p).and_then(|m| m.modified()).ok();
    (m(crate::paths::config_file()), m(config::team_file()))
}

impl Default for RealStore {
    fn default() -> Self {
        RealStore::new()
    }
}

impl RealStore {
    /// The store for the running app: the real data folder, registry and process list.
    pub fn new() -> RealStore {
        let ram = crate::win::sys_info().ram_total / MB;
        RealStore::with_parts(ram, Startup::registry(), live_processes)
    }

    /// A store with injected parts (tests).
    pub fn with_parts(ram_total_mb: u64, startup: Startup, processes: ProcessLister) -> RealStore {
        // Creates settings.toml from the template if there is none yet (same as the engine).
        let _ = config::load_layered();
        let builtin = config::BUILTIN_TOML.parse::<toml::Table>().expect("built-in defaults parse");
        let store = RealStore {
            cache: RefCell::new(Cache { checked: None, stamp: None, builtin, team: None, user: toml::Table::new(), revision: 0 }),
            ram_total_mb: ram_total_mb.max(1),
            startup,
            processes,
        };
        store.refresh(true);
        store
    }

    /// Re-read the layers if a file changed (or always, with `force`).
    fn refresh(&self, force: bool) {
        let mut c = self.cache.borrow_mut();
        if !force && c.checked.is_some_and(|t| t.elapsed() < RECHECK) {
            return;
        }
        c.checked = Some(Instant::now());
        let now = stamp();
        if !force && c.stamp == Some(now) {
            return;
        }
        c.stamp = Some(now);
        c.user = fs::read_to_string(crate::paths::config_file()).ok().and_then(|t| t.parse::<toml::Table>().ok()).unwrap_or_default();
        // A team file the engine would skip (broken) is not shown either.
        c.team = fs::read_to_string(config::team_file())
            .ok()
            .filter(|t| config::config_from_layers(Some(t), "").is_ok())
            .and_then(|t| t.parse::<toml::Table>().ok());
        c.revision += 1;
    }

    fn after_write(&self, r: Result<(), String>) -> Result<(), String> {
        self.refresh(true);
        r
    }

    /// The value of `path` in one layer's table.
    fn value_in(&self, t: &toml::Table, path: &str) -> Option<Value> {
        let (parent, key) = parent_of(t, path)?;
        if key == "trim_above_pct" {
            // MB wins over percent within one table; show it as the percent it means here.
            if let Some(mb) = parent.get("trim_above_mb").and_then(toml::Value::as_integer) {
                let pct = mb as f64 / self.ram_total_mb as f64 * 100.0;
                return Some(Value::Float((pct * 100.0).round() / 100.0));
            }
        }
        to_value(parent.get(key)?)
    }

    fn groups_now(&self) -> Vec<GroupInfo> {
        self.refresh(false);
        let c = self.cache.borrow();
        let layers: [(Layer, Option<&toml::Table>); 3] = [(Layer::Builtin, Some(&c.builtin)), (Layer::Team, c.team.as_ref()), (Layer::User, Some(&c.user))];
        let mut out: Vec<GroupInfo> = Vec::new();
        for (layer, table) in layers {
            let Some(table) = table else { continue };
            for g in group_tables(table) {
                let Some(name) = g.get("name").and_then(toml::Value::as_str) else { continue };
                let entry = match out.iter_mut().position(|x| x.name == name) {
                    Some(i) => &mut out[i],
                    None => {
                        out.push(GroupInfo {
                            name: name.to_string(),
                            processes: Vec::new(),
                            enabled: true,
                            builtin: layer == Layer::Builtin,
                            layer,
                            never_freeze: layer == Layer::Builtin && name == "Agents",
                        });
                        out.last_mut().unwrap()
                    }
                };
                if let Some(p) = g.get("processes").and_then(toml::Value::as_array) {
                    entry.processes = p.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
                }
                if let Some(e) = g.get("enabled").and_then(toml::Value::as_bool) {
                    entry.enabled = e;
                }
            }
        }
        out
    }

    fn group(&self, name: &str) -> Result<GroupInfo, String> {
        self.groups_now().into_iter().find(|g| g.name == name).ok_or_else(|| format!("There is no group called {}.", name))
    }

    fn set_processes(&mut self, group: &str, list: Vec<String>) -> Result<(), String> {
        let arr: toml_edit::Array = list.iter().map(String::as_str).collect();
        self.after_write(config::set_user_value(&format!("group.{}.processes", group), toml_edit::value(arr)))
    }
}

// ---------------------------------------------------------------- pure helpers

fn group_tables(t: &toml::Table) -> impl Iterator<Item = &toml::Table> {
    t.get("group").and_then(toml::Value::as_array).into_iter().flatten().filter_map(toml::Value::as_table)
}

/// The table holding the last key of `path`, and that key. `group.<name>.<keys>` looks the
/// group up by name.
fn parent_of<'a>(t: &'a toml::Table, path: &'a str) -> Option<(&'a toml::Table, &'a str)> {
    let segs: Vec<&str> = path.split('.').collect();
    let (mut cur, keys) = if segs.first() == Some(&"group") {
        let name = *segs.get(1)?;
        (group_tables(t).find(|g| g.get("name").and_then(toml::Value::as_str) == Some(name))?, &segs[2..])
    } else {
        (t, &segs[..])
    };
    let (last, init) = keys.split_last()?;
    for k in init {
        cur = cur.get(*k)?.as_table()?;
    }
    Some((cur, last))
}

fn to_value(v: &toml::Value) -> Option<Value> {
    Some(match v {
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::String(s) => Value::Text(s.clone()),
        toml::Value::Array(a) => Value::List(a.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?),
        _ => return None,
    })
}

fn to_item(v: Value) -> toml_edit::Item {
    match v {
        Value::Bool(b) => toml_edit::value(b),
        Value::Int(i) => toml_edit::value(i),
        // Slider steps like 0.30000000000000004 are written as 0.3.
        Value::Float(f) => toml_edit::value((f * 10_000.0).round() / 10_000.0),
        Value::Text(s) => toml_edit::value(s),
        Value::List(l) => toml_edit::value(l.iter().map(String::as_str).collect::<toml_edit::Array>()),
    }
}

/// How many values a team file sets (group names don't count).
fn count_settings(t: &toml::Table) -> usize {
    t.iter()
        .map(|(k, v)| match v {
            toml::Value::Table(sub) => count_settings(sub),
            toml::Value::Array(a) if k == "group" => a.iter().filter_map(toml::Value::as_table).map(|g| count_settings(g) - usize::from(g.contains_key("name"))).sum(),
            _ => 1,
        })
        .sum()
}

/// "12 Nov 2026" for a time (UTC date).
fn date_words(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {}", d, MONTHS[(m - 1) as usize], y)
}

/// Windows processes that are never worth offering as an app.
const SYSTEM: [&str; 30] = [
    "System", "Idle", "Registry", "Memory Compression", "svchost", "csrss", "wininit", "winlogon", "services", "lsass", "smss", "dwm",
    "fontdrvhost", "MsMpEng", "explorer", "sihost", "ctfmon", "RuntimeBroker", "SearchHost", "StartMenuExperienceHost",
    "ShellExperienceHost", "TextInputHost", "audiodg", "conhost", "dllhost", "WmiPrvSE", "spoolsv", "taskhostw", "SecurityHealthService",
    "NisSrv",
];

/// Running processes grouped by name, without the ones already in a group, protected or
/// system ones, and small ones; largest first, at most 25.
pub fn running_apps_from(procs: &[(String, u64)], groups: &[GroupInfo], protect: &[String]) -> Vec<RunningApp> {
    let mut by_name: HashMap<String, (String, usize, u64)> = HashMap::new();
    for (name, ram) in procs {
        let e = by_name.entry(name.to_ascii_lowercase()).or_insert_with(|| (name.clone(), 0, 0));
        e.1 += 1;
        e.2 += ram;
    }
    let mut out: Vec<RunningApp> = by_name
        .into_values()
        .filter(|(name, _, ram)| {
            *ram >= 30 * MB
                && !SYSTEM.iter().any(|s| s.eq_ignore_ascii_case(name))
                && !name.to_ascii_lowercase().contains("memguard")
                && !config::any_matches(protect, name)
                && !groups.iter().any(|g| config::any_matches(&g.processes, name))
        })
        .map(|(name, processes, ram)| RunningApp { name, processes, ram_gb: (ram as f64 / (1024.0 * MB as f64) * 10.0).round() / 10.0 })
        .collect();
    out.sort_by(|a, b| b.ram_gb.partial_cmp(&a.ram_gb).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.name.cmp(&b.name)));
    out.truncate(25);
    out
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {}", path.display(), e)
    })
}

// ---------------------------------------------------------------- SettingsStore

impl SettingsStore for RealStore {
    fn get(&self, path: &str) -> Option<Setting> {
        self.refresh(false);
        let c = self.cache.borrow();
        let layers: [(Layer, Option<&toml::Table>); 3] = [(Layer::User, Some(&c.user)), (Layer::Team, c.team.as_ref()), (Layer::Builtin, Some(&c.builtin))];
        let mut found = layers.iter().filter_map(|(l, t)| t.and_then(|t| self.value_in(t, path)).map(|v| (v, *l)));
        let (value, layer) = found.next()?;
        Some(Setting { value, layer, fallback: found.next() })
    }

    fn set(&mut self, path: &str, value: Value) -> Result<(), String> {
        self.after_write(config::set_user_value(path, to_item(value)))
    }

    fn reset(&mut self, path: &str) -> Result<(), String> {
        self.after_write(config::reset_user_value(path))
    }

    fn groups(&self) -> Vec<GroupInfo> {
        self.groups_now()
    }

    fn add_group(&mut self, name: &str, processes: &[String]) -> Result<(), String> {
        self.after_write(config::add_group(name, processes))
    }

    fn remove_group(&mut self, name: &str) -> Result<(), String> {
        self.after_write(config::remove_group(name))
    }

    fn rename_group(&mut self, from: &str, to: &str) -> Result<(), String> {
        let to = to.trim();
        if to.is_empty() || to.contains('.') {
            return Err("A group needs a name without dots.".into());
        }
        let g = self.group(from)?;
        if g.layer != Layer::User {
            return Err(format!("{} comes from the defaults: it can be turned off, not renamed.", from));
        }
        if to != from && self.groups_now().iter().any(|x| x.name.eq_ignore_ascii_case(to) && x.name != from) {
            return Err(format!("There is already a group called {}.", to));
        }
        let path = crate::paths::config_file();
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| format!("settings.toml: {}", e))?;
        let groups = doc.get_mut("group").and_then(|g| g.as_array_of_tables_mut()).ok_or("settings.toml has no groups")?;
        let entry = groups.iter_mut().find(|t| t.get("name").and_then(|n| n.as_str()) == Some(from)).ok_or_else(|| format!("There is no group called {}.", from))?;
        entry.insert("name", toml_edit::value(to));
        let new_text = doc.to_string();
        let team = fs::read_to_string(config::team_file()).ok().filter(|t| config::config_from_layers(Some(t), "").is_ok());
        config::config_from_layers(team.as_deref(), &new_text)?;
        self.after_write(write_atomic(&path, &new_text))
    }

    fn set_group_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        let path = format!("group.{}.enabled", name);
        if !enabled {
            return self.after_write(config::set_user_value(&path, toml_edit::value(false)));
        }
        self.after_write(config::reset_user_value(&path))?;
        if !self.group(name)?.enabled {
            // The team file turned it off: switch it back on in the user layer.
            return self.after_write(config::set_user_value(&path, toml_edit::value(true)));
        }
        Ok(())
    }

    fn add_process(&mut self, group: &str, process: &str) -> Result<(), String> {
        let p = process.trim();
        let p = if p.to_ascii_lowercase().ends_with(".exe") { &p[..p.len() - 4] } else { p };
        if p.is_empty() {
            return Err("Type the app's process name, for example chrome.".into());
        }
        let mut list = self.group(group)?.processes;
        if list.iter().any(|x| x.eq_ignore_ascii_case(p)) {
            return Ok(());
        }
        list.push(p.to_string());
        self.set_processes(group, list)
    }

    fn remove_process(&mut self, group: &str, process: &str) -> Result<(), String> {
        let mut list = self.group(group)?.processes;
        list.retain(|x| x != process);
        if list.is_empty() {
            return Err("A group needs at least one app. Turn the group off instead.".into());
        }
        self.set_processes(group, list)
    }

    fn export_to(&self, path: &Path) -> Result<(), String> {
        config::export_to(path)
    }

    fn import_from(&mut self, path: &Path) -> Result<(), String> {
        self.after_write(config::import_from(path))
    }

    fn load_team_file(&mut self, path: &Path) -> Result<(), String> {
        self.after_write(config::install_team_file(path))
    }

    fn remove_team_file(&mut self) -> Result<(), String> {
        let path = config::team_file();
        let r = match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        };
        self.after_write(r)
    }

    fn team_info(&self) -> Option<TeamInfo> {
        self.refresh(false);
        let c = self.cache.borrow();
        let team = c.team.as_ref()?;
        let loaded = c.stamp.and_then(|s| s.1).map(date_words).unwrap_or_default();
        Some(TeamInfo { loaded, settings: count_settings(team) })
    }

    fn reset_all(&mut self) -> Result<(), String> {
        let path = crate::paths::config_file();
        let r = (|| {
            if path.exists() {
                let bak = path.with_file_name("settings.toml.bak");
                fs::copy(&path, &bak).map_err(|e| format!("{}: {}", bak.display(), e))?;
            }
            write_atomic(&path, config::DEFAULT_TOML)
        })();
        self.after_write(r)
    }

    fn run_at_startup(&self) -> bool {
        (self.startup.get)()
    }

    fn set_run_at_startup(&mut self, on: bool) -> Result<(), String> {
        let r = (self.startup.set)(on);
        // Not a settings file change, but open windows should re-read.
        self.cache.borrow_mut().revision += 1;
        r
    }

    fn running_apps(&self) -> Vec<RunningApp> {
        let protect: Vec<String> = match self.get("ue.protect").map(|s| s.value) {
            Some(Value::List(l)) => l,
            _ => Vec::new(),
        };
        running_apps_from(&(self.processes)(), &self.groups_now(), &protect)
    }

    fn settings_file(&self) -> PathBuf {
        crate::paths::config_file()
    }

    fn revision(&self) -> u64 {
        self.refresh(false);
        self.cache.borrow().revision
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};

    /// Tests that point ATHER_MEMGUARD_DIR somewhere take this lock first.
    pub(crate) fn data_dir_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh, empty data folder in the temp directory, set as ATHER_MEMGUARD_DIR.
    pub(crate) fn temp_data_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("amg-test-{}-{}-{}", tag, std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ATHER_MEMGUARD_DIR", &dir);
        dir
    }

    static STARTUP: AtomicBool = AtomicBool::new(false);
    fn fake_get() -> bool {
        STARTUP.load(Ordering::SeqCst)
    }
    fn fake_set(on: bool) -> Result<(), String> {
        STARTUP.store(on, Ordering::SeqCst);
        Ok(())
    }
    fn fake_procs() -> Vec<(String, u64)> {
        vec![
            ("chrome".into(), 900 * MB),
            ("Figma".into(), 1500 * MB),
            ("Figma".into(), 900 * MB),
            ("UnrealEditor".into(), 30_000 * MB),
            ("svchost".into(), 800 * MB),
            ("tiny".into(), 5 * MB),
            ("Code".into(), 1200 * MB),
        ]
    }

    /// 128 GB of RAM, so 1% is 1310.72 MB.
    fn store() -> RealStore {
        RealStore::with_parts(131_072, Startup { get: fake_get, set: fake_set }, fake_procs)
    }

    /// The checkup writes a `[doctor]` table into settings.toml; the store and the settings
    /// window must read past it, and edits must keep it.
    #[test]
    fn doctor_table_from_the_checkup_is_kept() {
        let _g = data_dir_lock();
        let dir = temp_data_dir("doctor");
        let file = dir.join("settings.toml");
        let text = format!("{}\n[doctor]\nkeep = [\"x\"]\nclose_while_unreal = [\"y\"]\n", crate::config::DEFAULT_TOML);
        fs::write(&file, &text).unwrap();
        crate::config::load_layered().expect("the engine loads it");

        let mut s = store();
        assert_eq!(s.get("general.preset").unwrap().value, Value::Text("balanced".into()));
        let groups_before = s.groups().len();
        assert!(groups_before > 0 && !s.groups().iter().any(|g| g.name.eq_ignore_ascii_case("doctor")));
        s.set("history.keep_days", Value::Int(30)).unwrap();
        s.set("general.notifications", Value::Bool(false)).unwrap();
        assert_eq!(s.get("history.keep_days").unwrap().value, Value::Int(30));
        assert_eq!(s.groups().len(), groups_before);

        let after = fs::read_to_string(&file).unwrap();
        assert!(after.contains("keep_days = 30"));
        let ds = crate::doctor::DoctorSettings::load();
        assert_eq!((ds.keep, ds.close_while_unreal), (vec!["x".to_string()], vec!["y".to_string()]), "{}", after);

        // The settings window opens and paints on it.
        let mut w = crate::ui::settings::SettingsWindow::new(crate::ui::settings::SettingsHost {
            store: Box::new(store()),
            status: Box::new(crate::ui::status_source::FakeStatus::default()),
            commands: Box::new(|_| {}),
            app_version: "0.2.0".into(),
            update_state: Box::new(|| crate::ui::settings::UpdateState::Unknown),
        });
        let c = crate::ui::snapshots::render_settings(&mut w, crate::ui::theme::Scale(1.0), 900, Some(760));
        assert_eq!(c.width(), 900);
        w.set_query("keep");
        crate::ui::snapshots::render_settings(&mut w, crate::ui::theme::Scale(1.0), 900, Some(760));
    }

    #[test]
    fn get_set_reset_and_layers() {
        let _g = data_dir_lock();
        let dir = temp_data_dir("store");
        let mut s = store();
        assert!(dir.join("settings.toml").exists(), "the template is written on first use");

        let preset = s.get("general.preset").unwrap();
        // The template sets the preset itself.
        assert_eq!(preset.value, Value::Text("balanced".into()));
        let days = s.get("history.keep_days").unwrap();
        assert_eq!((days.value.clone(), days.layer, days.fallback.clone()), (Value::Int(7), Layer::Builtin, None));

        let r0 = s.revision();
        s.set("history.keep_days", Value::Int(30)).unwrap();
        let days = s.get("history.keep_days").unwrap();
        assert_eq!((days.value.clone(), days.layer), (Value::Int(30), Layer::User));
        assert_eq!(days.fallback, Some((Value::Int(7), Layer::Builtin)));
        assert!(s.revision() > r0);
        assert!(fs::read_to_string(dir.join("settings.toml")).unwrap().contains("keep_days = 30"));

        // A bad value is refused and nothing changes.
        assert!(s.set("history.keep_days", Value::Int(0)).is_err());
        assert_eq!(s.get("history.keep_days").unwrap().value, Value::Int(30));

        // Team layer in between.
        fs::write(dir.join("team.toml"), "[history]\nkeep_days = 14\n[ue]\neditor_restart_hint_gb = 40\n").unwrap();
        let s2 = store();
        let days = s2.get("history.keep_days").unwrap();
        assert_eq!(days.layer, Layer::User);
        assert_eq!(days.fallback, Some((Value::Int(14), Layer::Team)));
        let hint = s2.get("ue.editor_restart_hint_gb").unwrap();
        assert_eq!((hint.value, hint.layer), (Value::Int(40), Layer::Team));
        assert_eq!(s2.team_info().unwrap().settings, 2);

        s.reset("history.keep_days").unwrap();
        let days = s.get("history.keep_days").unwrap();
        assert_eq!((days.value, days.layer), (Value::Int(14), Layer::Team));

        s.remove_team_file().unwrap();
        assert!(s.team_info().is_none());
        assert_eq!(s.get("history.keep_days").unwrap().layer, Layer::Builtin);

        // Text, bool and list values.
        s.set("general.prime_hotkey", Value::Text("Ctrl+Shift+F9".into())).unwrap();
        assert_eq!(s.get("general.prime_hotkey").unwrap().value, Value::Text("Ctrl+Shift+F9".into()));
        s.set("general.notifications", Value::Bool(false)).unwrap();
        assert_eq!(s.get("general.notifications").unwrap().value, Value::Bool(false));
        let ram = s.get("general.check_ram_speed").unwrap();
        assert_eq!((ram.value, ram.layer), (Value::Bool(true), Layer::Builtin));
        s.set("general.check_ram_speed", Value::Bool(false)).unwrap();
        assert_eq!(s.get("general.check_ram_speed").unwrap().value, Value::Bool(false));
        assert!(!crate::config::load_layered().unwrap().0.general.check_ram_speed, "the engine sees it");
        s.reset("general.check_ram_speed").unwrap();
        assert_eq!(s.get("general.check_ram_speed").unwrap().layer, Layer::Builtin);
        assert!(matches!(s.get("ue.protect").unwrap().value, Value::List(l) if l.contains(&"git".to_string())));
        assert!(s.get("no.such.key").is_none());

        s.reset_all().unwrap();
        assert!(dir.join("settings.toml.bak").exists());
        assert_eq!(s.get("general.notifications").unwrap().layer, Layer::Builtin);
    }

    #[test]
    fn tiers_and_megabyte_caps() {
        let _g = data_dir_lock();
        let dir = temp_data_dir("tiers");
        let mut s = store();
        // Built-in percent values, and an unset tier is None.
        let always = s.get("group.Browsers.trim_above_pct").unwrap();
        assert_eq!((always.value, always.layer), (Value::Float(4.8), Layer::Builtin));
        assert!(s.get("group.Agents.trim_above_pct").is_none(), "Agents has no base cap");
        assert!(s.get("group.Agents.ue.trim_above_pct").is_none());
        assert!(s.get("group.Browsers.critical.cpu").is_none());

        // A cap set in MB reads as the percent it means on this PC.
        fs::write(
            dir.join("settings.toml"),
            "[general]\npreset = \"balanced\"\n\n[[group]]\nname = \"Browsers\"\ntrim_above_mb = 6554\n[group.ue]\ntrim_above_mb = 1311\n",
        )
        .unwrap();
        let s2 = store();
        let pct = s2.get("group.Browsers.trim_above_pct").unwrap();
        assert_eq!((pct.value, pct.layer), (Value::Float(5.0), Layer::User));
        assert_eq!(pct.fallback, Some((Value::Float(4.8), Layer::Builtin)));
        assert_eq!(s2.get("group.Browsers.ue.trim_above_pct").unwrap().value, Value::Float(1.0));

        // Writing a percent replaces the MB value; a tier override comes and goes.
        s.set("group.Browsers.trim_above_pct", Value::Float(0.1 + 0.2)).unwrap();
        let text = fs::read_to_string(dir.join("settings.toml")).unwrap();
        assert!(text.contains("trim_above_pct = 0.3") && !text.contains("trim_above_mb = 6554"), "{}", text);
        s.set("group.Agents.ue.trim_above_pct", Value::Float(3.0)).unwrap();
        let ue = s.get("group.Agents.ue.trim_above_pct").unwrap();
        assert_eq!((ue.value, ue.layer, ue.fallback), (Value::Float(3.0), Layer::User, None));
        s.reset("group.Agents.ue.trim_above_pct").unwrap();
        assert!(s.get("group.Agents.ue.trim_above_pct").is_none());
        s.set("group.Browsers.critical.cpu", Value::Text("idle".into())).unwrap();
        assert_eq!(s.get("group.Browsers.critical.cpu").unwrap().value, Value::Text("idle".into()));
    }

    #[test]
    fn groups_and_processes() {
        let _g = data_dir_lock();
        let dir = temp_data_dir("groups");
        let mut s = store();
        let names: Vec<String> = s.groups().into_iter().map(|g| g.name).collect();
        assert_eq!(names, ["Browsers", "Chat", "Agents", "Background"]);
        let agents = s.groups().into_iter().find(|g| g.name == "Agents").unwrap();
        assert!(agents.builtin && agents.never_freeze && agents.enabled && agents.layer == Layer::Builtin);

        s.add_group("Design", &["Figma".into()]).unwrap();
        assert!(s.add_group("design", &["x".into()]).is_err());
        s.add_process("Design", "Photoshop.exe").unwrap();
        s.add_process("Design", "figma").unwrap(); // already there
        let d = s.groups().into_iter().find(|g| g.name == "Design").unwrap();
        assert_eq!(d.processes, ["Figma", "Photoshop"]);
        assert_eq!((d.layer, d.builtin, d.never_freeze), (Layer::User, false, false));
        s.remove_process("Design", "Photoshop").unwrap();
        assert!(s.remove_process("Design", "Figma").is_err(), "the last app stays");

        s.set("group.Design.trim_above_pct", Value::Float(2.0)).unwrap();
        s.rename_group("Design", "Art").unwrap();
        assert_eq!(s.get("group.Art.trim_above_pct").unwrap().value, Value::Float(2.0));
        assert!(s.rename_group("Browsers", "Web").is_err());
        assert!(s.rename_group("Art", "Chat").is_err());

        // Built-in groups are turned off and on; user groups are removed.
        s.set_group_enabled("Chat", false).unwrap();
        assert!(!s.groups().into_iter().find(|g| g.name == "Chat").unwrap().enabled);
        s.set_group_enabled("Chat", true).unwrap();
        assert!(s.groups().into_iter().find(|g| g.name == "Chat").unwrap().enabled);
        s.remove_group("Background").unwrap();
        assert!(!s.groups().into_iter().find(|g| g.name == "Background").unwrap().enabled);
        s.remove_group("Art").unwrap();
        assert!(s.groups().iter().all(|g| g.name != "Art"));

        // Builtin processes replaced in the user layer.
        s.add_process("Browsers", "vivaldi").unwrap();
        assert!(s.groups()[0].processes.contains(&"vivaldi".to_string()));

        // Team groups carry the team layer.
        fs::write(dir.join("team.toml"), "[[group]]\nname = \"Tools\"\nprocesses = [\"obs64\"]\n").unwrap();
        let s2 = store();
        let tools = s2.groups().into_iter().find(|g| g.name == "Tools").unwrap();
        assert_eq!((tools.layer, tools.builtin), (Layer::Team, false));

        // Export and import.
        let out = dir.join("export.toml");
        s2.export_to(&out).unwrap();
        assert!(fs::read_to_string(&out).unwrap().contains("Tools"));
        let bad = dir.join("bad.toml");
        fs::write(&bad, "[history]\nkeep_days = 0\n").unwrap();
        assert!(s.import_from(&bad).is_err());
    }

    #[test]
    fn startup_switch_and_running_apps() {
        let _g = data_dir_lock();
        temp_data_dir("startup");
        let mut s = store();
        STARTUP.store(false, Ordering::SeqCst);
        assert!(!s.run_at_startup());
        let r = s.revision();
        s.set_run_at_startup(true).unwrap();
        assert!(s.run_at_startup() && STARTUP.load(Ordering::SeqCst));
        assert!(s.revision() > r);

        let apps = s.running_apps();
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        // chrome is in Browsers, UnrealEditor is protected, svchost is system, tiny is small.
        assert_eq!(names, ["Figma", "Code"]);
        assert_eq!((apps[0].processes, apps[0].ram_gb), (2, 2.3));
    }

    #[test]
    fn cache_notices_edits_on_disk() {
        let _g = data_dir_lock();
        let dir = temp_data_dir("cache");
        let s = store();
        let r = s.revision();
        assert_eq!(s.get("history.keep_days").unwrap().value, Value::Int(7));
        std::thread::sleep(Duration::from_millis(20));
        fs::write(dir.join("settings.toml"), "[history]\nkeep_days = 9\n").unwrap();
        // Within the recheck window the cached value stays...
        std::thread::sleep(RECHECK + Duration::from_millis(30));
        // ...after it, the new file is read.
        assert_eq!(s.get("history.keep_days").unwrap().value, Value::Int(9));
        assert!(s.revision() > r);
    }

    #[test]
    fn helpers() {
        assert_eq!(date_words(SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_960_000)), "18 Nov 2026");
        assert_eq!(date_words(SystemTime::UNIX_EPOCH), "1 Jan 1970");
        let t: toml::Table = "[general]\npreset = \"gentle\"\n[[group]]\nname = \"A\"\nprocesses = [\"a\"]\n[group.ue]\ncpu = \"idle\"\n".parse().unwrap();
        assert_eq!(count_settings(&t), 3);
        assert!(matches!(to_item(Value::Float(0.1 + 0.2)).as_value().and_then(|v| v.as_float()), Some(f) if f == 0.3));
    }
}

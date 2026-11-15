//! Moving caches off the system drive (proposal 4 F): point the tools at a new folder with
//! user environment variables. It never moves data, so the old cache stays where it is and a
//! new one grows at the new place; once the new place has been written to, the old one shows
//! up in the disk checkup as "Old cache, no longer used".
//!
//! Handled: `UE-LocalDataCachePath`, `NUGET_PACKAGES`, `npm_config_cache`, `PIP_CACHE_DIR`,
//! `VCPKG_DEFAULT_BINARY_CACHE`. Never `CARGO_HOME` (it holds `cargo.exe`, on PATH),
//! `GRADLE_USER_HOME` (settings and credentials) or `%TEMP%`. Zen's data path variable is not
//! confirmed, so Zen is not moved.
//!
//! The registry writes are behind `EnvStore`. The real one is `HKCU\Environment` plus a
//! `WM_SETTINGCHANGE` broadcast; `ATHER_MEMGUARD_ENV_KEY` points it at a test key, which must
//! be under `HKCU\Software\AtherMemGuard-test\`, and then nothing is broadcast.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::DiskSettings;
use crate::disk_space::{gb2, norm};
use crate::engine::Level;

pub const ENV_KEY_VAR: &str = "ATHER_MEMGUARD_ENV_KEY";
pub const TEST_KEY_PREFIX: &str = r"Software\AtherMemGuard-test\";
/// The folder made on the destination drive.
pub const DEST_DIR: &str = "DevCaches";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvValue {
    pub value: String,
    /// `REG_EXPAND_SZ` (holds `%VARS%`) rather than `REG_SZ`.
    pub expand: bool,
}

/// Where user environment variables are read and written.
pub trait EnvStore {
    fn get(&self, name: &str) -> Result<Option<EnvValue>, String>;
    fn set(&self, name: &str, v: &EnvValue) -> Result<(), String>;
    fn remove(&self, name: &str) -> Result<(), String>;
    /// Tell running programs the environment changed (only the real store does).
    fn broadcast(&self);
    /// "HKCU\Environment", or the test key: a record is only undone in the store it was made in.
    fn describe(&self) -> String;
}

/// `HKCU\Environment`, or a test key under `HKCU\Software\AtherMemGuard-test\`.
pub struct RegistryEnv {
    subkey: String,
    real: bool,
}

impl RegistryEnv {
    /// The store `memguard disk relocate` uses: the test key named by `ATHER_MEMGUARD_ENV_KEY`
    /// if set (refused unless it is under the test prefix), else the user's environment.
    pub fn from_env() -> Result<RegistryEnv, String> {
        match std::env::var(ENV_KEY_VAR) {
            Ok(k) if !k.trim().is_empty() => RegistryEnv::test_key(k.trim()),
            _ => Ok(RegistryEnv { subkey: "Environment".into(), real: true }),
        }
    }

    pub fn test_key(subkey: &str) -> Result<RegistryEnv, String> {
        if !subkey.to_ascii_lowercase().starts_with(&TEST_KEY_PREFIX.to_ascii_lowercase()) || subkey.contains("..") {
            return Err(format!("{} must name a key under HKCU\\{}", ENV_KEY_VAR, TEST_KEY_PREFIX));
        }
        Ok(RegistryEnv { subkey: subkey.to_string(), real: false })
    }

    pub fn is_real(&self) -> bool {
        self.real
    }
}

fn hkcu() -> winreg::RegKey {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
}

fn utf16_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect()
}

impl EnvStore for RegistryEnv {
    fn get(&self, name: &str) -> Result<Option<EnvValue>, String> {
        use winreg::enums::RegType;
        let key = match hkcu().open_subkey(&self.subkey) {
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        match key.get_raw_value(name) {
            Ok(v) => {
                let units: Vec<u16> = v.bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let n = units.iter().position(|&c| c == 0).unwrap_or(units.len());
                Ok(Some(EnvValue { value: String::from_utf16_lossy(&units[..n]), expand: v.vtype == RegType::REG_EXPAND_SZ }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, name: &str, v: &EnvValue) -> Result<(), String> {
        use winreg::enums::RegType;
        let (key, _) = hkcu().create_subkey(&self.subkey).map_err(|e| e.to_string())?;
        let rv = winreg::RegValue { bytes: utf16_bytes(&v.value), vtype: if v.expand { RegType::REG_EXPAND_SZ } else { RegType::REG_SZ } };
        key.set_raw_value(name, &rv).map_err(|e| e.to_string())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        let key = match hkcu().open_subkey_with_flags(&self.subkey, winreg::enums::KEY_SET_VALUE) {
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        match key.delete_value(name) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn broadcast(&self) {
        if self.real {
            crate::disk_win::broadcast_environment_change();
        }
    }

    fn describe(&self) -> String {
        format!(r"HKCU\{}", self.subkey)
    }
}

// ---------------------------------------------------------------- what moves

/// A cache a variable can move.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheVar {
    pub name: &'static str,
    pub what: &'static str,
    /// Where the tool keeps it when the variable is unset.
    pub default: Option<PathBuf>,
    /// The folder under the destination.
    pub sub: &'static str,
}

pub fn cache_vars() -> Vec<CacheVar> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let l = |p: &str| local.as_ref().map(|b| b.join(p));
    vec![
        CacheVar { name: "UE-LocalDataCachePath", what: "Unreal's shared cache (DDC)", default: l(r"UnrealEngine\Common\DerivedDataCache"), sub: r"UnrealEngine\DerivedDataCache" },
        CacheVar { name: "NUGET_PACKAGES", what: "NuGet packages", default: home.as_ref().map(|h| h.join(r".nuget\packages")), sub: r"NuGet\packages" },
        CacheVar { name: "npm_config_cache", what: "npm cache", default: l("npm-cache"), sub: "npm-cache" },
        CacheVar { name: "PIP_CACHE_DIR", what: "pip cache", default: l(r"pip\cache"), sub: r"pip\cache" },
        CacheVar { name: "VCPKG_DEFAULT_BINARY_CACHE", what: "vcpkg binary cache", default: l(r"vcpkg\archives"), sub: r"vcpkg\archives" },
    ]
}

/// Never relocated, and why (for the plan's notes).
pub const NEVER: &[(&str, &str)] = &[
    ("CARGO_HOME", "it also holds cargo.exe, which is on PATH"),
    ("GRADLE_USER_HOME", "it holds gradle.properties and credentials, not only caches"),
    ("TEMP", "Windows and every program use it"),
];

/// `%NAME%` filled in from this process's environment.
pub fn expand_vars(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('%') {
        out.push_str(&rest[..a]);
        let after = &rest[a + 1..];
        match after.find('%') {
            Some(b) => {
                let name = &after[..b];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[b + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One variable as it is now on this PC.
#[derive(Debug, Clone, PartialEq)]
pub struct VarState {
    pub name: String,
    pub what: String,
    pub sub: String,
    pub current: Option<EnvValue>,
    /// Where the cache is now (the variable's value, else the default).
    pub location: String,
    pub exists: bool,
    pub volume: Option<String>,
    pub size: Option<u64>,
    /// The count stopped before the whole folder was read: `size` is a lower bound.
    pub size_partial: bool,
}

/// A drive the caches could go to.
#[derive(Debug, Clone, PartialEq)]
pub struct DestVolume {
    pub guid: String,
    pub name: String,
    pub root: String,
    pub avail: u64,
    pub total: u64,
    /// A fixed, local, non-rotating disk.
    pub suitable: bool,
    /// A fixed local disk (not removable, not a network or optical drive).
    pub fixed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub name: String,
    pub what: String,
    pub old_location: String,
    pub new_location: Option<String>,
    pub size: Option<u64>,
    /// `size` is a lower bound: the count stopped before the whole folder was read.
    #[serde(default)]
    pub size_partial: bool,
    /// "set" or "skip".
    pub action: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelocatePlan {
    pub destination: Option<String>,
    pub entries: Vec<PlanEntry>,
    /// Bytes the moved caches hold now: what the destination should have room for.
    pub need: u64,
    /// `need` is a lower bound: at least one cache was too big to count in time.
    #[serde(default)]
    pub need_partial: bool,
    pub dest_free_now: Option<u64>,
    pub dest_free_after: Option<u64>,
    /// Why nothing will be set.
    pub refused: Option<String>,
    pub notes: Vec<String>,
}

/// The plan, pure. `to`: a folder the person named (and its volume); otherwise the best other
/// drive. A destination is refused when the caches would leave it below `elevated`.
pub fn plan(vars: &[VarState], system: Option<&str>, vols: &[DestVolume], to: Option<(&str, &str)>, s: &DiskSettings) -> RelocatePlan {
    let mut entries = Vec::new();
    let mut need = 0u64;
    let mut need_partial = false;
    for v in vars {
        let on_system = v.volume.as_deref().zip(system).is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
        let (action, reason) = if !v.exists {
            ("skip", Some("not used on this PC (the folder doesn't exist)".to_string()))
        } else if !on_system {
            ("skip", Some("already off the system drive".to_string()))
        } else {
            need += v.size.unwrap_or(0);
            need_partial |= v.size_partial || v.size.is_none();
            ("set", None)
        };
        entries.push(PlanEntry { name: v.name.clone(), what: v.what.clone(), old_location: v.location.clone(), new_location: None, size: v.size, size_partial: v.size_partial, action: action.into(), reason });
    }
    let mut notes: Vec<String> = NEVER.iter().map(|(n, why)| format!("{} is never moved: {}.", n, why)).collect();
    notes.push("Zen's cache isn't moved: its data path setting isn't confirmed yet.".into());
    notes.push("Nothing is copied or deleted. The old caches stay where they are until you remove them, so total use goes up at first.".into());
    notes.push("NuGet needs a restore afterwards (dotnet restore, or a build in Visual Studio), which downloads the packages again over the network.".into());
    notes.push("vcpkg, npm, pip, NuGet and Unreal fail or start over if the new drive is missing (unplugged, or its letter changed): keep it connected, or undo with memguard disk relocate --undo.".into());
    notes.push("A cache moved here can still be cleaned: the disk checkup lists Unreal's shared cache at its new place (the place recorded by this relocation), under the same safety rules.".into());
    let mut p = RelocatePlan { destination: None, entries, need, need_partial, dest_free_now: None, dest_free_after: None, refused: None, notes };
    if !p.entries.iter().any(|e| e.action == "set") {
        p.refused = Some("Nothing to move: no cache this covers is on the system drive.".into());
        return p;
    }
    // A size that is only a lower bound must not pass the room check silently: the destination
    // has to keep the "normal" level with room for twice the counted size.
    let room = if p.need_partial { need.saturating_mul(2) } else { need };
    if p.need_partial {
        p.notes.push(format!(
            "Some caches were too big to count in time, so they hold at least {:.1} GB; the destination must stay above the \"getting full\" level with room for twice that ({:.1} GB).",
            gb2(need),
            gb2(room)
        ));
    }
    let fits = |v: &DestVolume| v.avail > room && crate::disk::level_for(v.avail - room, v.total, s) == Level::Normal;
    let dest: Result<(&DestVolume, String), String> = match to {
        Some((folder, _)) if to_path_problem(folder).is_some() => Err(to_path_problem(folder).unwrap_or_default()),
        Some((folder, guid)) => match vols.iter().find(|v| v.guid.eq_ignore_ascii_case(guid)) {
            None => Err(format!("{} isn't on a drive Ather MemGuard can see.", folder)),
            Some(v) if !v.fixed => Err(format!("{} is on a removable or network drive. Pick a folder on a fixed drive inside this PC.", folder)),
            Some(v) if system.is_some_and(|sy| sy.eq_ignore_ascii_case(&v.guid)) => Err(format!("{} is on the system drive, where the caches already are.", folder)),
            Some(v) if !fits(v) => Err(format!(
                "{} has {:.0} GB free; the caches need room for {}{:.0} GB, which would leave it below {:.0} GB (the \"getting full\" level). Pick another drive.",
                v.name,
                gb2(v.avail),
                if p.need_partial { "at least " } else { "" },
                gb2(room),
                gb2(crate::disk::threshold(s.elevated_free_gb, s.elevated_free_pct, v.total))
            )),
            Some(v) => Ok((v, folder.trim_end_matches('\\').to_string())),
        },
        None => vols
            .iter()
            .filter(|v| v.suitable && !system.is_some_and(|sy| sy.eq_ignore_ascii_case(&v.guid)))
            .filter(|v| fits(v))
            .max_by_key(|v| v.avail)
            .map(|v| (v, format!("{}{}", v.root.trim_end_matches('\\').to_string() + "\\", DEST_DIR)))
            .ok_or_else(|| {
                if p.need_partial {
                    format!("No other drive has room for twice the counted size ({:.1} GB; some caches were too big to count in time) and enough left over (it would drop below the \"getting full\" level).", gb2(room))
                } else {
                    "No other drive has room for these caches and enough left over (it would drop below the \"getting full\" level).".to_string()
                }
            }),
    };
    match dest {
        Err(why) => p.refused = Some(why),
        Ok((v, folder)) => {
            p.destination = Some(folder.clone());
            p.dest_free_now = Some(v.avail);
            p.dest_free_after = Some(v.avail.saturating_sub(need));
            let mut entries = std::mem::take(&mut p.entries);
            for (e, var) in entries.iter_mut().zip(vars) {
                if e.action == "set" {
                    e.new_location = Some(format!("{}\\{}", folder, var.sub));
                }
            }
            p.entries = entries;
        }
    }
    p
}

/// What is wrong with a `--to` folder, if anything: it must be an absolute path with a drive
/// letter (`E:\Caches`), never relative, a network path (`\server\share`) or a device path.
pub fn to_path_problem(folder: &str) -> Option<String> {
    let b = folder.as_bytes();
    let lettered = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    if folder.starts_with(r"\\") || folder.starts_with("//") {
        return Some(format!(r"{} is a network or device path. Pick a folder on a fixed drive inside this PC, like E:\DevCaches.", folder));
    }
    if !lettered || folder.contains("..") {
        return Some(format!(r"{} isn't a full path. Give the whole path with its drive letter, like E:\DevCaches.", folder));
    }
    None
}

/// Read this PC: every variable's current value (from `store`), where its cache is, how big.
pub fn read_vars(store: &dyn EnvStore, sizer: &dyn Fn(&str) -> Option<(u64, bool)>) -> Vec<VarState> {
    cache_vars()
        .into_iter()
        .map(|cv| {
            let current = store.get(cv.name).ok().flatten();
            let location = current
                .as_ref()
                .map(|v| expand_vars(&v.value))
                .or_else(|| std::env::var(cv.name).ok().filter(|v| !v.trim().is_empty()))
                .or_else(|| cv.default.as_ref().map(|d| d.display().to_string()))
                .unwrap_or_default();
            let exists = !location.is_empty() && Path::new(&location).is_dir();
            let volume = if location.is_empty() { None } else { existing_volume(&location) };
            let counted = if exists { sizer(&location) } else { None };
            let (size, size_partial) = match counted {
                Some((bytes, complete)) => (Some(bytes), !complete),
                None => (None, false),
            };
            VarState { name: cv.name.into(), what: cv.what.into(), sub: cv.sub.into(), current, location, exists, volume, size, size_partial }
        })
        .collect()
}

fn existing_volume(p: &str) -> Option<String> {
    let e = Path::new(p).ancestors().find(|a| a.exists())?;
    crate::disk_win::volume_of(&e.display().to_string()).map(|(_, g)| g)
}

/// The drives on this PC, as destinations.
pub fn dest_volumes() -> Vec<DestVolume> {
    crate::disk_win::volumes()
        .into_iter()
        .filter(|f| f.total > 0 && !f.paths.is_empty())
        .map(|f| {
            let name = f.paths.iter().find(|p| p.len() <= 3).cloned().unwrap_or_else(|| f.paths[0].clone());
            DestVolume {
                suitable: f.drive_type == 3 && f.seek_penalty != Some(true),
                fixed: f.drive_type == 3,
                guid: f.guid.clone(),
                name: name.trim_end_matches('\\').to_string(),
                root: name,
                avail: f.avail,
                total: f.total,
            }
        })
        .collect()
}

/// Size a folder (enumeration only, no file opened): allocated bytes, and whether every folder
/// was read. A cache can hold hundreds of thousands of files (a DDC here: 693,000), so the count
/// gets 3 minutes; when it stops early the bytes are a lower bound and say so.
pub fn quick_size(path: &str) -> Option<(u64, bool)> {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};
    let vs = crate::disk_win::VolumeScan {
        cluster: 4096,
        block_cloning: false,
        level2: false,
        threads: 8,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(180),
        stop: Arc::new(AtomicBool::new(false)),
        started_ft: u64::MAX,
        multi: Mutex::new(std::collections::HashMap::new()),
        progress_files: Arc::new(AtomicU64::new(0)),
    };
    let (st, _) = crate::disk_win::walk_item(&vs, 0, &[path.to_string()], &[]);
    Some((st.allocated, !st.cut_short && st.dir_failures == 0))
}

// ---------------------------------------------------------------- apply and undo

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordEntry {
    pub name: String,
    pub previous: Option<EnvValue>,
    pub new_value: String,
    pub old_location: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelocateRecord {
    pub time: String,
    pub unix: u64,
    pub store: String,
    pub entries: Vec<RecordEntry>,
    #[serde(default)]
    pub undone: bool,
}

/// Set the variables of a plan, saving the previous values first. Makes the new folders
/// (empty: nothing is copied). Refused while an earlier relocation isn't undone.
pub fn apply(p: &RelocatePlan, store: &dyn EnvStore, files: &crate::disk_clean::DiskFiles) -> Result<RelocateRecord, String> {
    if let Some(why) = &p.refused {
        return Err(why.clone());
    }
    if files.relocate_record().is_some_and(|r| !r.undone) {
        return Err("Caches were already moved. Undo that first (memguard disk relocate --undo).".into());
    }
    let mut rec = RelocateRecord { time: crate::win::local_time_string(), unix: crate::disk_clean::unix_now(), store: store.describe(), entries: Vec::new(), undone: false };
    for e in p.entries.iter().filter(|e| e.action == "set") {
        let Some(new) = &e.new_location else { continue };
        std::fs::create_dir_all(new).map_err(|err| format!("couldn't make {}: {}", new, err))?;
        let previous = store.get(&e.name)?;
        rec.entries.push(RecordEntry { name: e.name.clone(), previous, new_value: new.clone(), old_location: e.old_location.clone() });
        // Save the record before each write, so a failure part-way can still be undone.
        files.save_relocate_record(&rec)?;
        store.set(&e.name, &EnvValue { value: new.clone(), expand: false })?;
    }
    store.broadcast();
    crate::log::write(&format!("disk: relocated caches in {}: {}", rec.store, rec.entries.iter().map(|e| format!("{}={}", e.name, e.new_value)).collect::<Vec<_>>().join(", ")));
    Ok(rec)
}

/// Put the previous values back. A variable changed since (by someone else) is left alone.
pub fn undo(store: &dyn EnvStore, files: &crate::disk_clean::DiskFiles) -> Result<Vec<String>, String> {
    let mut rec = files.relocate_record().filter(|r| !r.undone).ok_or("Nothing to undo: no cache relocation is waiting.")?;
    if !rec.store.eq_ignore_ascii_case(&store.describe()) {
        return Err(format!("That relocation was made in {}, not {}.", rec.store, store.describe()));
    }
    let mut said = Vec::new();
    for e in &rec.entries {
        let now = store.get(&e.name)?;
        if now.as_ref().map(|v| v.value.as_str()) != Some(e.new_value.as_str()) {
            said.push(format!("{} was changed since, so it was left as it is.", e.name));
            continue;
        }
        match &e.previous {
            Some(v) => store.set(&e.name, v)?,
            None => store.remove(&e.name)?,
        }
        said.push(match &e.previous {
            Some(v) => format!("{} is back to {}.", e.name, v.value),
            None => format!("{} is unset again.", e.name),
        });
    }
    store.broadcast();
    rec.undone = true;
    files.save_relocate_record(&rec)?;
    crate::log::write(&format!("disk: undid the cache relocation in {}", rec.store));
    Ok(said)
}

/// Is `old` the place variable `name` keeps its cache when unset (one of `vars`' defaults,
/// compared normalised)? Only that place is ever offered as an old cache: a relocation record
/// edited by hand could otherwise name any folder in the profile.
pub fn old_location_known(name: &str, old: &str, vars: &[CacheVar]) -> bool {
    let old = norm(old);
    !old.is_empty()
        && vars
            .iter()
            .filter(|v| v.name.eq_ignore_ascii_case(name))
            .filter_map(|v| v.default.as_ref())
            .any(|d| norm(&d.display().to_string()) == old)
}

/// Old caches no longer used: the new folder exists and was written after the relocation.
pub fn old_caches(rec: &RelocateRecord) -> Vec<(String, String)> {
    old_caches_with(rec, &cache_vars())
}

/// `old_caches`, with the variables (and their defaults) given.
pub fn old_caches_with(rec: &RelocateRecord, vars: &[CacheVar]) -> Vec<(String, String)> {
    if rec.undone {
        return Vec::new();
    }
    let after = |p: &Path| -> bool {
        let t = |m: std::fs::Metadata| m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let own = std::fs::metadata(p).map(t).unwrap_or(0);
        let child = std::fs::read_dir(p).map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(t).max().unwrap_or(0)).unwrap_or(0);
        own.max(child) > rec.unix && std::fs::read_dir(p).map(|mut d| d.next().is_some()).unwrap_or(false)
    };
    rec.entries
        .iter()
        .filter(|e| old_location_known(&e.name, &e.old_location, vars))
        .filter(|e| Path::new(&e.old_location).is_dir() && norm(&e.old_location) != norm(&e.new_value) && after(Path::new(&e.new_value)))
        .map(|e| (e.name.clone(), e.old_location.clone()))
        .collect()
}

/// The plan as JSON.
pub fn plan_json(p: &RelocatePlan, store: &str) -> Value {
    json!({
        "store": store,
        "destination": p.destination,
        "need_gb": gb2(p.need),
        "need_is_lower_bound": p.need_partial,
        "dest_free_now_gb": p.dest_free_now.map(gb2),
        "dest_free_after_gb": p.dest_free_after.map(gb2),
        "refused": p.refused,
        "entries": p.entries.iter().map(|e| json!({ "variable": e.name, "what": e.what, "now": e.old_location, "new": e.new_location,
            "size_gb": e.size.map(gb2), "size_is_lower_bound": e.size_partial, "action": e.action, "reason": e.reason })).collect::<Vec<_>>(),
        "notes": p.notes,
    })
}

/// The plan as text.
pub fn render_plan(p: &RelocatePlan) -> String {
    let mut s = String::new();
    match &p.destination {
        Some(d) => s.push_str(&format!("Move caches off the system drive to {} (sets your user environment variables; copies nothing):\n", d)),
        None => s.push_str("Caches that could move off the system drive:\n"),
    }
    for e in &p.entries {
        let size = match e.size {
            Some(b) if e.size_partial => format!("{:.1}+ GB", gb2(b)),
            Some(b) => format!("{:.1} GB", gb2(b)),
            None => "-".into(),
        };
        match (&e.action[..], &e.new_location) {
            ("set", Some(n)) => s.push_str(&format!("  {:<28} {:>9}  {}\n  {:<28} {:>9}  -> {}\n", e.name, size, e.old_location, "", "", n)),
            ("set", None) => s.push_str(&format!("  {:<28} {:>9}  {}\n", e.name, size, e.old_location)),
            _ => s.push_str(&format!("  {:<28} {:>9}  {} (skipped: {})\n", e.name, size, e.old_location, e.reason.as_deref().unwrap_or(""))),
        }
    }
    if let (Some(now), Some(after)) = (p.dest_free_now, p.dest_free_after) {
        let held = if p.need_partial { format!("at least {:.1} GB", gb2(p.need)) } else { format!("{:.1} GB", gb2(p.need)) };
        let left = if p.need_partial { format!("less than {:.0} GB", gb2(after)) } else { format!("about {:.0} GB", gb2(after)) };
        s.push_str(&format!("The caches hold {} now. {} has {:.0} GB free; {} if they grow back to that size there.\n", held, p.destination.as_deref().unwrap_or(""), gb2(now), left));
        if p.need_partial {
            s.push_str("A size ending in + is where counting stopped before the whole folder was read: the cache holds at least that much.\n");
        }
    }
    if let Some(r) = &p.refused {
        s.push_str(&format!("Not offered: {}\n", r));
    }
    for n in &p.notes {
        s.push_str(&format!("- {}\n", n));
    }
    if p.refused.is_none() {
        s.push_str("Apply with: memguard disk relocate --apply   Undo with: memguard disk relocate --undo\n");
        s.push_str("Programs started after that use the new places; restart open terminals and Unreal to pick them up.\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const G: u64 = 1 << 30;

    #[derive(Default)]
    struct MemEnv {
        vals: RefCell<HashMap<String, EnvValue>>,
        broadcasts: RefCell<u32>,
    }

    impl EnvStore for MemEnv {
        fn get(&self, n: &str) -> Result<Option<EnvValue>, String> {
            Ok(self.vals.borrow().get(n).cloned())
        }
        fn set(&self, n: &str, v: &EnvValue) -> Result<(), String> {
            self.vals.borrow_mut().insert(n.into(), v.clone());
            Ok(())
        }
        fn remove(&self, n: &str) -> Result<(), String> {
            self.vals.borrow_mut().remove(n);
            Ok(())
        }
        fn broadcast(&self) {
            *self.broadcasts.borrow_mut() += 1;
        }
        fn describe(&self) -> String {
            "memory".into()
        }
    }

    fn var(name: &str, loc: &str, vol: &str, size_gb: u64, exists: bool) -> VarState {
        VarState { name: name.into(), what: name.into(), sub: name.into(), current: None, location: loc.into(), exists, volume: Some(vol.into()), size: Some(size_gb * G), size_partial: false }
    }

    fn vol(guid: &str, avail_gb: u64, total_gb: u64, suitable: bool) -> DestVolume {
        DestVolume { guid: guid.into(), name: format!("{}:", guid), root: format!("{}:\\", guid), avail: avail_gb * G, total: total_gb * G, suitable, fixed: true }
    }

    #[test]
    fn a_count_that_stopped_early_is_shown_as_a_lower_bound() {
        // Found live: a 112.8 GB DDC (693,000 files) hit the count's deadline and printed as
        // "1.4 GB", as if that were all of it.
        let s = DiskSettings::default();
        let mut ddc = var("UE-LocalDataCachePath", r"C:\u\ddc", "C", 1, true);
        ddc.size_partial = true;
        let vars = vec![ddc, var("npm_config_cache", r"C:\u\npm", "C", 3, true)];
        let vols = vec![vol("C", 298, 1903, true), vol("E", 1739, 1863, true)];
        let p = plan(&vars, Some("C"), &vols, None, &s);
        assert!(p.need_partial);
        assert!(p.entries[0].size_partial && !p.entries[1].size_partial);
        let text = render_plan(&p);
        assert!(text.contains("1.0+ GB"), "{}", text);
        assert!(text.contains("The caches hold at least 4.0 GB now"), "{}", text);
        assert!(text.contains("less than"), "{}", text);
        let j = plan_json(&p, "memory");
        assert_eq!(j["need_is_lower_bound"], true);
        assert_eq!(j["entries"][0]["size_is_lower_bound"], true);
        assert_eq!(j["entries"][1]["size_is_lower_bound"], false);
        // A complete count reads as before.
        let p = plan(&[var("npm_config_cache", r"C:\u\npm", "C", 3, true)], Some("C"), &vols, None, &s);
        assert!(!p.need_partial);
        assert!(render_plan(&p).contains("The caches hold 3.0 GB now"));
    }

    #[test]
    fn plan_picks_a_drive_with_room_and_never_the_tight_one() {
        let s = DiskSettings::default();
        let vars = vec![var("UE-LocalDataCachePath", r"C:\u\ddc", "C", 107, true), var("npm_config_cache", r"C:\u\npm", "C", 3, true), var("PIP_CACHE_DIR", r"E:\pip", "E", 3, true), var("NUGET_PACKAGES", r"C:\u\nuget", "C", 0, false)];
        // D: is the Dev Drive with 320 GB free: 110 GB would leave 210 (> 150): fits, but E: has more.
        let vols = vec![vol("C", 298, 1903, true), vol("D", 320, 1907, true), vol("E", 1739, 1863, true), vol("G", 6172, 7451, false)];
        let p = plan(&vars, Some("C"), &vols, None, &s);
        assert_eq!(p.refused, None);
        assert_eq!(p.destination.as_deref(), Some(r"E:\DevCaches"));
        assert_eq!(p.need, 110 * G);
        assert_eq!(p.entries[0].new_location.as_deref(), Some(r"E:\DevCaches\UE-LocalDataCachePath"));
        assert_eq!(p.entries[2].action, "skip");
        assert_eq!(p.entries[2].reason.as_deref(), Some("already off the system drive"));
        assert_eq!(p.entries[3].action, "skip");
        assert!(p.notes.iter().any(|n| n.starts_with("CARGO_HOME is never moved")));
        assert!(p.notes.iter().any(|n| n.starts_with("GRADLE_USER_HOME is never moved")));
        assert!(!p.entries.iter().any(|e| e.name == "CARGO_HOME" || e.name == "GRADLE_USER_HOME" || e.name == "TEMP"));
        // Naming D: when it would drop below elevated (150 GB): refused.
        let tight = vec![vol("C", 298, 1903, true), vol("D", 200, 1907, true)];
        let p = plan(&vars, Some("C"), &tight, Some((r"D:\Caches", "D")), &s);
        assert!(p.refused.as_deref().unwrap().contains("below 150 GB"), "{:?}", p.refused);
        // Auto with no room anywhere: refused, nothing set.
        let p = plan(&vars, Some("C"), &tight, None, &s);
        assert!(p.refused.is_some() && p.destination.is_none());
        // Never the system drive.
        let p = plan(&vars, Some("C"), &vols, Some((r"C:\Other", "C")), &s);
        assert!(p.refused.unwrap().contains("system drive"));
        // An HDD is never picked on its own.
        let p = plan(&vars, Some("C"), &[vol("C", 298, 1903, true), vol("G", 6172, 7451, false)], None, &s);
        assert!(p.refused.is_some());
    }

    /// Found in review: with a size that is only a lower bound, the room check passed silently.
    #[test]
    fn a_lower_bound_size_needs_twice_the_room() {
        let s = DiskSettings::default();
        let mut ddc = var("UE-LocalDataCachePath", r"C:\u\ddc", "C", 100, true);
        ddc.size_partial = true;
        // D: has 320 GB free of 1907: 100 GB counted would leave 220 (fits), twice leaves 120 (< 150).
        let vols = vec![vol("C", 298, 1903, true), vol("D", 320, 1907, true)];
        let p = plan(std::slice::from_ref(&ddc), Some("C"), &vols, None, &s);
        assert!(p.destination.is_none(), "{:?}", p);
        assert!(p.refused.as_deref().unwrap().contains("twice the counted size"), "{:?}", p.refused);
        assert!(p.notes.iter().any(|n| n.contains("room for twice that")));
        let p = plan(std::slice::from_ref(&ddc), Some("C"), &vols, Some((r"D:\Caches", "D")), &s);
        assert!(p.refused.as_deref().unwrap().contains("at least 200 GB"), "{:?}", p.refused);
        // A complete count of the same size fits.
        ddc.size_partial = false;
        let p = plan(&[ddc], Some("C"), &vols, None, &s);
        assert_eq!(p.destination.as_deref(), Some(r"D:\DevCaches"));
    }

    #[test]
    fn to_must_be_a_full_path_on_a_fixed_local_drive() {
        let s = DiskSettings::default();
        let vars = vec![var("npm_config_cache", r"C:\u\npm", "C", 3, true)];
        let mut usb = vol("F", 900, 1000, false);
        usb.fixed = false;
        let vols = vec![vol("C", 298, 1903, true), vol("E", 1739, 1863, true), usb];
        for bad in ["Caches", r"..\Caches", r"E:Caches", r"\\server\share\caches", r"\\?\E:\caches", r"E:\x\..\..\y"] {
            let p = plan(&vars, Some("C"), &vols, Some((bad, "E")), &s);
            assert!(p.refused.is_some() && p.destination.is_none(), "{} must be refused: {:?}", bad, p);
        }
        assert!(plan(&vars, Some("C"), &vols, Some((r"F:\Caches", "F")), &s).refused.unwrap().contains("removable or network"));
        assert!(plan(&vars, Some("C"), &vols, Some((r"C:\Caches", "C")), &s).refused.unwrap().contains("system drive"));
        let ok = plan(&vars, Some("C"), &vols, Some((r"E:\Caches", "E")), &s);
        assert_eq!(ok.refused, None);
        assert_eq!(ok.destination.as_deref(), Some(r"E:\Caches"));
        // The notes say what moving costs.
        assert!(ok.notes.iter().any(|n| n.contains("NuGet needs a restore")));
        assert!(ok.notes.iter().any(|n| n.contains("new drive is missing")));
        // Removable drives are never picked on their own either.
        let p = plan(&vars, Some("C"), &[vol("C", 298, 1903, true), { let mut u = vol("F", 900, 1000, false); u.fixed = false; u }], None, &s);
        assert!(p.refused.is_some());
    }

    #[test]
    fn apply_and_undo_restore_exactly_what_was_there() {
        let dir = std::env::temp_dir().join(format!("amg-reloc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = crate::disk_clean::DiskFiles::in_dir(dir.join("data"));
        let env = MemEnv::default();
        env.set("npm_config_cache", &EnvValue { value: r"%LOCALAPPDATA%\npm-old".into(), expand: true }).unwrap();
        let s = DiskSettings::default();
        let vars = vec![var("UE-LocalDataCachePath", r"C:\u\ddc", "C", 10, true), var("npm_config_cache", r"C:\u\npm", "C", 1, true)];
        let dest = dir.join("E").display().to_string();
        let vols = vec![vol("C", 298, 1903, true), vol("E", 1739, 1863, true)];
        let p = plan(&vars, Some("C"), &vols, Some((&dest, "E")), &s);
        let rec = apply(&p, &env, &files).unwrap();
        assert_eq!(rec.entries.len(), 2);
        assert_eq!(*env.broadcasts.borrow(), 1);
        assert_eq!(env.get("UE-LocalDataCachePath").unwrap().unwrap().value, format!(r"{}\UE-LocalDataCachePath", dest));
        assert!(Path::new(&format!(r"{}\npm_config_cache", dest)).is_dir(), "the new folder is made (empty)");
        assert!(apply(&p, &env, &files).unwrap_err().contains("already moved"));
        // No old-cache item until the new place is written to.
        assert!(old_caches(&files.relocate_record().unwrap()).is_empty());
        // Someone changed one variable since: undo leaves that one alone.
        env.set("UE-LocalDataCachePath", &EnvValue { value: r"X:\mine".into(), expand: false }).unwrap();
        let said = undo(&env, &files).unwrap();
        assert!(said[0].contains("changed since"));
        assert_eq!(env.get("UE-LocalDataCachePath").unwrap().unwrap().value, r"X:\mine");
        assert_eq!(env.get("npm_config_cache").unwrap().unwrap(), EnvValue { value: r"%LOCALAPPDATA%\npm-old".into(), expand: true });
        assert!(undo(&env, &files).is_err(), "only once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_cache_becomes_an_item_once_the_new_one_is_written() {
        let dir = std::env::temp_dir().join(format!("amg-reloc-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old")).unwrap();
        std::fs::create_dir_all(dir.join("new")).unwrap();
        let rec = RelocateRecord {
            time: String::new(),
            unix: crate::disk_clean::unix_now() - 60,
            store: "memory".into(),
            entries: vec![RecordEntry { name: "PIP_CACHE_DIR".into(), previous: None, new_value: dir.join("new").display().to_string(), old_location: dir.join("old").display().to_string() }],
            undone: false,
        };
        // The test's own folder stands in for pip's default cache.
        let vars = vec![CacheVar { name: "PIP_CACHE_DIR", what: "pip cache", default: Some(dir.join("old")), sub: r"pip\cache" }];
        assert!(old_caches_with(&rec, &vars).is_empty(), "empty new folder: not written to yet");
        std::fs::write(dir.join(r"new\wheel.bin"), b"x").unwrap();
        assert_eq!(old_caches_with(&rec, &vars), vec![("PIP_CACHE_DIR".to_string(), dir.join("old").display().to_string())]);
        let mut undone = rec.clone();
        undone.undone = true;
        assert!(old_caches_with(&undone, &vars).is_empty());
        // A forged record naming another folder (here one that exists, with a written new
        // place) isn't offered as an old cache.
        std::fs::create_dir_all(dir.join("Documents")).unwrap();
        let mut forged = rec.clone();
        forged.entries[0].old_location = dir.join("Documents").display().to_string();
        assert!(old_caches_with(&forged, &vars).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Found in review: a forged disk-relocate.json could list any profile folder as "Old
    /// cache, no longer used". Only the variable's own default place counts.
    #[test]
    fn old_location_must_be_the_variables_default() {
        let vars = vec![
            CacheVar { name: "PIP_CACHE_DIR", what: "pip cache", default: Some(PathBuf::from(r"C:\Users\me\AppData\Local\pip\cache")), sub: r"pip\cache" },
            CacheVar { name: "npm_config_cache", what: "npm cache", default: Some(PathBuf::from(r"C:\Users\me\AppData\Local\npm-cache")), sub: "npm-cache" },
            CacheVar { name: "NUGET_PACKAGES", what: "NuGet packages", default: None, sub: r"NuGet\packages" },
        ];
        assert!(old_location_known("PIP_CACHE_DIR", r"C:\Users\me\AppData\Local\pip\cache", &vars));
        // After normalisation: case, slashes, a trailing separator.
        assert!(old_location_known("pip_cache_dir", r"c:/users/ME/appdata/local/pip/cache\", &vars));
        assert!(old_location_known("PIP_CACHE_DIR", r"\\?\C:\Users\me\AppData\Local\pip\cache", &vars));
        // Another folder, a folder inside or above the default, another variable's default.
        assert!(!old_location_known("PIP_CACHE_DIR", r"C:\Users\me\Documents", &vars));
        assert!(!old_location_known("PIP_CACHE_DIR", r"C:\Users\me\AppData\Local\pip", &vars));
        assert!(!old_location_known("PIP_CACHE_DIR", r"C:\Users\me\AppData\Local\pip\cache\wheels", &vars));
        assert!(!old_location_known("PIP_CACHE_DIR", r"C:\Users\me\AppData\Local\npm-cache", &vars));
        // No default known, an unknown variable, nothing at all.
        assert!(!old_location_known("NUGET_PACKAGES", r"C:\Users\me\.nuget\packages", &vars));
        assert!(!old_location_known("CARGO_HOME", r"C:\Users\me\.cargo", &vars));
        assert!(!old_location_known("PIP_CACHE_DIR", "", &vars));
        // The real defaults relocation uses are the ones checked.
        let real = cache_vars();
        for v in &real {
            if let Some(d) = &v.default {
                assert!(old_location_known(v.name, &d.display().to_string(), &real), "{}", v.name);
            }
        }
    }

    #[test]
    fn test_key_must_be_under_the_test_prefix() {
        assert!(RegistryEnv::test_key(r"Environment").is_err());
        assert!(RegistryEnv::test_key(r"Software\Other\Environment").is_err());
        assert!(RegistryEnv::test_key(r"Software\AtherMemGuard-test\..\..\Environment").is_err());
        let k = RegistryEnv::test_key(r"Software\AtherMemGuard-test\Environment").unwrap();
        assert!(!k.is_real());
        assert_eq!(k.describe(), r"HKCU\Software\AtherMemGuard-test\Environment");
    }

    /// The registry store against a throwaway key under HKCU\Software\AtherMemGuard-test,
    /// which this test creates and deletes. The real HKCU\Environment is never opened.
    #[test]
    fn registry_store_round_trip_on_a_test_key() {
        let sub = format!(r"Software\AtherMemGuard-test\unit-{}", std::process::id());
        let k = RegistryEnv::test_key(&sub).unwrap();
        assert_eq!(k.get("PIP_CACHE_DIR").unwrap(), None);
        k.set("PIP_CACHE_DIR", &EnvValue { value: r"E:\DevCaches\pip".into(), expand: false }).unwrap();
        k.set("npm_config_cache", &EnvValue { value: r"%LOCALAPPDATA%\npm".into(), expand: true }).unwrap();
        assert_eq!(k.get("PIP_CACHE_DIR").unwrap().unwrap().value, r"E:\DevCaches\pip");
        assert!(k.get("npm_config_cache").unwrap().unwrap().expand);
        k.remove("PIP_CACHE_DIR").unwrap();
        k.remove("PIP_CACHE_DIR").unwrap();
        assert_eq!(k.get("PIP_CACHE_DIR").unwrap(), None);
        let _ = hkcu().delete_subkey_all(&sub);
        let parent = hkcu().open_subkey(r"Software\AtherMemGuard-test").ok();
        if parent.is_some_and(|p| p.enum_keys().next().is_none() && p.enum_values().next().is_none()) {
            let _ = hkcu().delete_subkey(r"Software\AtherMemGuard-test");
        }
        assert!(hkcu().open_subkey(&sub).is_err());
    }

    #[test]
    fn expands_variables() {
        std::env::set_var("AMG_TEST_X", r"C:\x");
        assert_eq!(expand_vars(r"%AMG_TEST_X%\npm"), r"C:\x\npm");
        assert_eq!(expand_vars(r"%AMG_NOPE_Y%\a"), r"%AMG_NOPE_Y%\a");
        assert_eq!(expand_vars("100%"), "100%");
    }
}

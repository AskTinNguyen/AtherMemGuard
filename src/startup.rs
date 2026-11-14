//! "Starts with Windows" entries: the Run keys and the Startup folders, with the on/off
//! state Task Manager shows (stored under `Explorer\StartupApproved`).
//!
//! Reading covers HKCU Run, HKLM Run (64- and 32-bit), the user's Startup folder and the
//! common Startup folder. Switching covers only the per-user places (HKCU Run and the
//! user's Startup folder): the machine-wide ones need an administrator.
//!
//! Switching an entry off never deletes it. It writes the same 12-byte `StartupApproved`
//! value Task Manager writes, so it shows as "Disabled" there and can be switched back on
//! from either place. No `unsafe` here: registry access goes through `winreg`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_BINARY};
use winreg::types::FromRegValue;
use winreg::{RegKey, RegValue};

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN32_KEY: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
pub const APPROVED_RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
pub const APPROVED_RUN32: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32";
pub const APPROVED_FOLDER: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";

/// Where a startup entry lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    /// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
    HkcuRun,
    /// `HKLM\Software\Microsoft\Windows\CurrentVersion\Run`
    HklmRun,
    /// `HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run`
    HklmRun32,
    /// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`
    UserFolder,
    /// `%PROGRAMDATA%\Microsoft\Windows\Start Menu\Programs\StartUp`
    CommonFolder,
}

impl Location {
    /// Short id used in finding ids: `startup:<id>:<name>`.
    pub fn id(self) -> &'static str {
        match self {
            Location::HkcuRun => "hkcu",
            Location::HklmRun => "hklm",
            Location::HklmRun32 => "hklm32",
            Location::UserFolder => "folder",
            Location::CommonFolder => "commonfolder",
        }
    }

    pub fn from_id(id: &str) -> Option<Location> {
        [Location::HkcuRun, Location::HklmRun, Location::HklmRun32, Location::UserFolder, Location::CommonFolder]
            .into_iter()
            .find(|l| l.id() == id)
    }

    /// Can this user switch it without an administrator?
    pub fn switchable(self) -> bool {
        matches!(self, Location::HkcuRun | Location::UserFolder)
    }

    /// Plain words for where it is.
    pub fn label(self) -> &'static str {
        match self {
            Location::HkcuRun => "your Run key",
            Location::HklmRun | Location::HklmRun32 => "the machine Run key",
            Location::UserFolder => "your Startup folder",
            Location::CommonFolder => "the shared Startup folder",
        }
    }

    /// The `StartupApproved` key (and its hive) that holds this location's on/off switches.
    fn approved_key(self) -> (RegKey, &'static str) {
        match self {
            Location::HkcuRun => (RegKey::predef(HKEY_CURRENT_USER), APPROVED_RUN),
            Location::HklmRun => (RegKey::predef(HKEY_LOCAL_MACHINE), APPROVED_RUN),
            Location::HklmRun32 => (RegKey::predef(HKEY_LOCAL_MACHINE), APPROVED_RUN32),
            Location::UserFolder => (RegKey::predef(HKEY_CURRENT_USER), APPROVED_FOLDER),
            Location::CommonFolder => (RegKey::predef(HKEY_LOCAL_MACHINE), APPROVED_FOLDER),
        }
    }
}

/// One startup entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub location: Location,
    /// Registry value name, or the file name in a Startup folder (`Steam.lnk`).
    pub name: String,
    /// The command line (Run keys) or the shortcut's target (folders).
    pub command: String,
    /// Executable name without `.exe`, as process names are compared.
    pub exe: String,
    /// On, as Task Manager shows it.
    pub enabled: bool,
}

// ---------------------------------------------------------------- StartupApproved values

/// Encode a `StartupApproved` value. Byte 0 is 0x02 for on and 0x03 for off; bytes 4..12
/// hold the FILETIME the entry was switched off (zero when on). Task Manager writes the same.
pub fn encode_approved(enabled: bool, when_filetime: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    if enabled {
        b[0] = 0x02;
    } else {
        b[0] = 0x03;
        b[4..12].copy_from_slice(&when_filetime.to_le_bytes());
    }
    b
}

/// Decode a `StartupApproved` value: `Some(true)` = on, `Some(false)` = off, `None` = not a
/// value we understand (treated as on, which is what Windows does). Windows uses even first
/// bytes (0x02, 0x06) for on and odd ones (0x01, 0x03, 0x07) for off.
pub fn decode_approved(bytes: &[u8]) -> Option<bool> {
    let first = *bytes.first()?;
    if first == 0 {
        return None;
    }
    Some(first & 1 == 0)
}

/// The FILETIME stored in an "off" value, if any.
pub fn approved_time(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 12 {
        return None;
    }
    let t = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
    (t != 0).then_some(t)
}

// ---------------------------------------------------------------- resolving names

/// Executable name (without `.exe`) from a Run-key command line such as
/// `"C:\Program Files\Steam\steam.exe" -silent` or `C:\Tools\x.exe /background`.
pub fn exe_from_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    let path = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next().unwrap_or(rest).to_string()
    } else {
        let lower = cmd.to_ascii_lowercase();
        match lower.find(".exe") {
            Some(i) => cmd[..i + 4].to_string(),
            None => cmd.split_whitespace().next().unwrap_or("").to_string(),
        }
    };
    exe_stem(&path)
}

/// `C:\a\Steam.exe` -> `Steam`; `Steam.lnk` -> `Steam`.
pub fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path).trim();
    let lower = file.to_ascii_lowercase();
    for ext in [".exe", ".lnk", ".bat", ".cmd"] {
        if lower.ends_with(ext) {
            return file[..file.len() - ext.len()].to_string();
        }
    }
    file.to_string()
}

fn read_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn c_string(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..)?;
    let end = s.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&s[..end]).into_owned())
}

fn c_wstring(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..)?;
    let units: Vec<u16> = s.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

/// The target path of a shell link (`.lnk`), parsed from the file format (MS-SHLLINK):
/// the LinkInfo local base path if there is one, else the relative path. `None` for links
/// that only point at a shell item (Store apps) or files we can't read.
pub fn lnk_target(b: &[u8]) -> Option<String> {
    const HAS_ID_LIST: u32 = 0x1;
    const HAS_LINK_INFO: u32 = 0x2;
    const HAS_NAME: u32 = 0x4;
    const HAS_RELATIVE_PATH: u32 = 0x8;
    const IS_UNICODE: u32 = 0x80;
    if read_u32(b, 0)? != 0x4C {
        return None;
    }
    let flags = read_u32(b, 20)?;
    let mut at = 0x4C;
    if flags & HAS_ID_LIST != 0 {
        at += 2 + read_u16(b, at)? as usize;
    }
    if flags & HAS_LINK_INFO != 0 {
        let size = read_u32(b, at)? as usize;
        let header = read_u32(b, at + 4)? as usize;
        let info_flags = read_u32(b, at + 8)?;
        if info_flags & 1 != 0 {
            if header >= 0x24 {
                let off = read_u32(b, at + 28)? as usize;
                if off != 0 {
                    if let Some(s) = c_wstring(b, at + off).filter(|s| !s.is_empty()) {
                        return Some(s);
                    }
                }
            }
            let off = read_u32(b, at + 16)? as usize;
            if let Some(s) = c_string(b, at + off).filter(|s| !s.is_empty()) {
                return Some(s);
            }
        }
        at += size;
    }
    let unicode = flags & IS_UNICODE != 0;
    let read_string = |at: &mut usize| -> Option<String> {
        let count = read_u16(b, *at)? as usize;
        *at += 2;
        let s = if unicode {
            let bytes = b.get(*at..*at + count * 2)?;
            *at += count * 2;
            String::from_utf16_lossy(&bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>())
        } else {
            let bytes = b.get(*at..*at + count)?;
            *at += count;
            String::from_utf8_lossy(bytes).into_owned()
        };
        Some(s)
    };
    if flags & HAS_NAME != 0 {
        read_string(&mut at)?;
    }
    if flags & HAS_RELATIVE_PATH != 0 {
        return read_string(&mut at).filter(|s| !s.is_empty());
    }
    None
}

// ---------------------------------------------------------------- reading

fn approved_values(location: Location) -> Vec<(String, Vec<u8>)> {
    let (hive, path) = location.approved_key();
    let Ok(key) = hive.open_subkey(path) else { return Vec::new() };
    key.enum_values().filter_map(Result::ok).map(|(name, v)| (name, v.bytes)).collect()
}

fn enabled_in(approved: &[(String, Vec<u8>)], name: &str) -> bool {
    approved
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .and_then(|(_, b)| decode_approved(b))
        .unwrap_or(true)
}

fn run_entries(location: Location, hive: RegKey, path: &str, out: &mut Vec<Entry>) {
    let Ok(key) = hive.open_subkey(path) else { return };
    let approved = approved_values(location);
    for (name, value) in key.enum_values().filter_map(Result::ok) {
        let Ok(command) = String::from_reg_value(&value) else { continue };
        if name.is_empty() {
            continue; // the key's (Default) value
        }
        out.push(Entry {
            location,
            exe: exe_from_command(&command),
            enabled: enabled_in(&approved, &name),
            name,
            command,
        });
    }
}

pub fn user_startup_folder() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

pub fn common_startup_folder() -> Option<PathBuf> {
    std::env::var_os("PROGRAMDATA").map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs\StartUp"))
}

fn folder_entries(location: Location, dir: Option<PathBuf>, out: &mut Vec<Entry>) {
    let Some(dir) = dir else { return };
    let Ok(read) = fs::read_dir(&dir) else { return };
    let approved = approved_values(location);
    for item in read.filter_map(Result::ok) {
        let path = item.path();
        if !path.is_file() {
            continue;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let target = if name.to_ascii_lowercase().ends_with(".lnk") {
            fs::read(&path).ok().and_then(|b| lnk_target(&b))
        } else {
            None
        };
        let command = target.clone().unwrap_or_else(|| path.display().to_string());
        let exe = exe_stem(target.as_deref().unwrap_or(&name));
        out.push(Entry { location, enabled: enabled_in(&approved, &name), exe, command, name });
    }
}

/// Every startup entry this user has, with its on/off state. Read-only.
pub fn list() -> Vec<Entry> {
    let mut out = Vec::new();
    run_entries(Location::HkcuRun, RegKey::predef(HKEY_CURRENT_USER), RUN_KEY, &mut out);
    run_entries(Location::HklmRun, RegKey::predef(HKEY_LOCAL_MACHINE), RUN_KEY, &mut out);
    run_entries(Location::HklmRun32, RegKey::predef(HKEY_LOCAL_MACHINE), RUN32_KEY, &mut out);
    folder_entries(Location::UserFolder, user_startup_folder(), &mut out);
    folder_entries(Location::CommonFolder, common_startup_folder(), &mut out);
    out
}

// ---------------------------------------------------------------- switching (per-user only)

/// The raw `StartupApproved` value for an entry, if one exists.
pub fn approved_value(location: Location, name: &str) -> Option<Vec<u8>> {
    let (hive, path) = location.approved_key();
    let key = hive.open_subkey(path).ok()?;
    key.get_raw_value(name).ok().map(|v| v.bytes)
}

fn check_switchable(location: Location, name: &str) -> Result<(), String> {
    if !location.switchable() {
        return Err(format!("{} is in {}; changing it needs an administrator.", name, location.label()));
    }
    if name.is_empty() || name.contains('\\') {
        return Err(format!("\"{}\" is not a startup entry name.", name));
    }
    Ok(())
}

/// Does this per-user entry still exist (Run value or Startup-folder file)?
pub fn exists(location: Location, name: &str) -> bool {
    match location {
        Location::HkcuRun => RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .and_then(|k| k.get_raw_value(name))
            .is_ok(),
        Location::UserFolder => user_startup_folder().map_or(false, |d| d.join(name).is_file()),
        _ => false,
    }
}

/// Switch a per-user entry on or off. Returns the `StartupApproved` value it had before
/// (`None` = there was none), which is what `restore` needs to undo it exactly.
pub fn set_enabled(location: Location, name: &str, enabled: bool) -> Result<Option<Vec<u8>>, String> {
    check_switchable(location, name)?;
    if !exists(location, name) {
        return Err(format!("The startup entry \"{}\" is gone.", name));
    }
    let previous = approved_value(location, name);
    let (hive, path) = location.approved_key();
    let (key, _) = hive.create_subkey(path).map_err(|e| format!("could not open {}: {}", path, e))?;
    let bytes = encode_approved(enabled, crate::win::now_filetime()).to_vec();
    key.set_raw_value(name, &RegValue { bytes, vtype: REG_BINARY })
        .map_err(|e| format!("could not switch \"{}\": {}", name, e))?;
    Ok(previous)
}

/// Put a per-user entry's `StartupApproved` value back to what it was (`None` deletes it).
pub fn restore(location: Location, name: &str, previous: Option<&[u8]>) -> Result<(), String> {
    check_switchable(location, name)?;
    let (hive, path) = location.approved_key();
    match previous {
        Some(bytes) => {
            let (key, _) = hive.create_subkey(path).map_err(|e| format!("could not open {}: {}", path, e))?;
            key.set_raw_value(name, &RegValue { bytes: bytes.to_vec(), vtype: REG_BINARY })
                .map_err(|e| format!("could not restore \"{}\": {}", name, e))
        }
        None => match hive.open_subkey_with_flags(path, winreg::enums::KEY_SET_VALUE) {
            Ok(key) => match key.delete_value(name) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("could not restore \"{}\": {}", name, e)),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("could not open {}: {}", path, e)),
        },
    }
}

/// Is `path` inside one of the Startup folders? (Used to describe entries.)
pub fn in_startup_folder(path: &Path) -> bool {
    [user_startup_folder(), common_startup_folder()].into_iter().flatten().any(|d| path.starts_with(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_round_trip() {
        let on = encode_approved(true, 123);
        assert_eq!(on, [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_approved(&on), Some(true));
        assert_eq!(approved_time(&on), None);
        let ft = 0x01DB_1234_5678_9ABCu64;
        let off = encode_approved(false, ft);
        assert_eq!(off[0], 3);
        assert_eq!(&off[1..4], &[0, 0, 0]);
        assert_eq!(decode_approved(&off), Some(false));
        assert_eq!(approved_time(&off), Some(ft));
    }

    #[test]
    fn approved_variants_windows_writes() {
        assert_eq!(decode_approved(&[6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), Some(true));
        assert_eq!(decode_approved(&[1, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]), Some(false));
        assert_eq!(decode_approved(&[7, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]), Some(false));
        assert_eq!(decode_approved(&[]), None);
        assert_eq!(decode_approved(&[0; 12]), None);
        assert!(enabled_in(&[("Steam".into(), encode_approved(false, 5).to_vec())], "steam") == false);
        assert!(enabled_in(&[("Other".into(), encode_approved(false, 5).to_vec())], "steam"));
        assert!(enabled_in(&[("Steam".into(), vec![])], "Steam"));
    }

    #[test]
    fn exe_names_from_commands() {
        assert_eq!(exe_from_command(r#""C:\Program Files (x86)\Steam\steam.exe" -silent"#), "steam");
        assert_eq!(exe_from_command(r"C:\Program Files\Foo\Bar App.exe /background"), "Bar App");
        assert_eq!(exe_from_command(r"C:\Windows\system32\rundll32.exe C:\x\y.dll,Run"), "rundll32");
        assert_eq!(exe_from_command(r"%LOCALAPPDATA%\Discord\Update.EXE --processStart Discord.exe"), "Update");
        assert_eq!(exe_from_command("ctfmon"), "ctfmon");
        assert_eq!(exe_from_command(r#"  "C:\a\b.exe""#), "b");
        assert_eq!(exe_from_command(""), "");
        assert_eq!(exe_stem("Spotify.lnk"), "Spotify");
        assert_eq!(exe_stem(r"C:\x\run.cmd"), "run");
    }

    fn lnk(flags: u32, id_list: Option<&[u8]>, link_info_path: Option<&str>, name: Option<&str>, rel: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 0x4C];
        b[0..4].copy_from_slice(&0x4Cu32.to_le_bytes());
        b[20..24].copy_from_slice(&flags.to_le_bytes());
        if let Some(ids) = id_list {
            b.extend_from_slice(&(ids.len() as u16).to_le_bytes());
            b.extend_from_slice(ids);
        }
        if let Some(p) = link_info_path {
            let header = 0x1Cu32;
            let mut info = Vec::new();
            let path_off = header;
            let mut path_bytes = p.as_bytes().to_vec();
            path_bytes.push(0);
            let size = header + path_bytes.len() as u32;
            info.extend_from_slice(&size.to_le_bytes());
            info.extend_from_slice(&header.to_le_bytes());
            info.extend_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
            info.extend_from_slice(&0u32.to_le_bytes()); // VolumeIDOffset (not parsed)
            info.extend_from_slice(&path_off.to_le_bytes());
            info.extend_from_slice(&0u32.to_le_bytes());
            info.extend_from_slice(&0u32.to_le_bytes());
            info.extend_from_slice(&path_bytes);
            b.extend_from_slice(&info);
        }
        let mut push = |s: &str| {
            let units: Vec<u16> = s.encode_utf16().collect();
            b.extend_from_slice(&(units.len() as u16).to_le_bytes());
            for u in units {
                b.extend_from_slice(&u.to_le_bytes());
            }
        };
        if let Some(n) = name {
            push(n);
        }
        if let Some(r) = rel {
            push(r);
        }
        b
    }

    #[test]
    fn lnk_targets() {
        // ID list + link info with a local base path.
        let b = lnk(0x1 | 0x2 | 0x80, Some(&[1, 2, 3, 4]), Some(r"C:\Program Files\Spotify\Spotify.exe"), None, None);
        assert_eq!(lnk_target(&b).as_deref(), Some(r"C:\Program Files\Spotify\Spotify.exe"));
        // No link info: falls back to the relative path, after the name string.
        let b = lnk(0x4 | 0x8 | 0x80, None, None, Some("Launch Foo"), Some(r"..\..\Foo\foo.exe"));
        assert_eq!(lnk_target(&b).as_deref(), Some(r"..\..\Foo\foo.exe"));
        assert_eq!(exe_stem(&lnk_target(&b).unwrap()), "foo");
        // Only an ID list (a Store app): nothing to resolve.
        assert_eq!(lnk_target(&lnk(0x1, Some(&[9, 9]), None, None, None)), None);
        // Not a shell link at all, or truncated.
        assert_eq!(lnk_target(b"hello"), None);
        assert_eq!(lnk_target(&b[..0x50]), None);
    }

    #[test]
    fn only_per_user_entries_are_switchable() {
        assert!(Location::HkcuRun.switchable());
        assert!(Location::UserFolder.switchable());
        assert!(!Location::HklmRun.switchable());
        assert!(!Location::HklmRun32.switchable());
        assert!(!Location::CommonFolder.switchable());
        assert!(set_enabled(Location::HklmRun, "Anything", false).unwrap_err().contains("administrator"));
        assert!(restore(Location::CommonFolder, "x.lnk", None).is_err());
        for l in [Location::HkcuRun, Location::HklmRun, Location::HklmRun32, Location::UserFolder, Location::CommonFolder] {
            assert_eq!(Location::from_id(l.id()), Some(l));
        }
    }

    #[test]
    fn listing_is_read_only_and_works() {
        // Reads the real startup entries; never writes.
        let entries = list();
        for e in &entries {
            assert!(!e.name.is_empty());
        }
    }
}

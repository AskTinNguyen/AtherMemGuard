//! Per-user install: write both executables, start at sign-in, Start menu shortcut,
//! Settings > Apps entry, optionally add to PATH. Also decides what the GUI exe does when
//! started without arguments (`launch_mode`).
//!
//! Release builds (`--features embed-cli`) carry `memguard.exe` inside `AtherMemGuard.exe`,
//! so the one setup exe is all anyone downloads. Dev builds copy it from next to the exe.

use std::borrow::Cow;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_SZ};

use crate::paths;
use crate::win::{self, RegHive, RegKey};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Ask the running copy to exit and wait for it.
pub fn stop_running() -> bool {
    if !win::signal_event(paths::exit_event()) {
        return false;
    }
    for _ in 0..80 {
        if !win::instance_running(paths::mutex()) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

fn from_wide_bytes(b: &[u8]) -> String {
    let wide: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&wide)
}

/// Add or remove the install folder on the user's PATH, keeping the value's registry type.
fn update_user_path(dir: &std::path::Path, add: bool) -> Result<bool, String> {
    let dir = dir.display().to_string();
    let env = RegKey::open_write(RegHive::Hkcu, "Environment").map_err(|e| e.to_string())?;
    let (current, vtype) = match env.get_raw("Path") {
        Ok((t, bytes)) => (from_wide_bytes(&bytes), t),
        Err(_) => (String::new(), REG_EXPAND_SZ),
    };
    let mut parts: Vec<&str> = current.split(';').filter(|p| !p.is_empty()).collect();
    let present = parts.iter().any(|p| p.eq_ignore_ascii_case(&dir));
    if add == present {
        return Ok(false);
    }
    if add {
        parts.push(&dir);
    } else {
        parts.retain(|p| !p.eq_ignore_ascii_case(&dir));
    }
    let expand = vtype != REG_SZ;
    env.set_string("Path", &parts.join(";"), expand).map_err(|e| e.to_string())?;
    win::broadcast_environment_change();
    Ok(true)
}

/// AtherMemGuard used to be called MemCap. Stop that version, remove its autostart, PATH
/// entry and program folder, and carry its settings over. Returns true if it was found.
fn migrate_from_memcap() -> bool {
    let local = match std::env::var_os("LOCALAPPDATA") {
        Some(v) => PathBuf::from(v),
        None => return false,
    };
    let old_program = local.join("Programs").join("MemCap");
    let old_settings = local.join("MemCap").join("memcap.toml");
    let mut found = false;

    if win::signal_event("Local\\MemCap-Exit") {
        found = true;
        for _ in 0..80 {
            if !win::instance_running("Local\\MemCap-Running") {
                break;
            }
            sleep(Duration::from_millis(100));
        }
    }
    if let Ok(run) = RegKey::open_write(RegHive::Hkcu, paths::RUN_KEY) {
        if run.get_raw("MemCap").is_ok() {
            found = true;
            let _ = run.delete_value("MemCap");
        }
    }
    if old_program.exists() {
        found = true;
        let _ = update_user_path(&old_program, false);
        for _ in 0..20 {
            if fs::remove_dir_all(&old_program).is_ok() {
                break;
            }
            sleep(Duration::from_millis(250));
        }
    }
    let new_settings = paths::config_file();
    if old_settings.exists() && !new_settings.exists() {
        found = true;
        if fs::create_dir_all(paths::data_dir()).is_ok() && fs::copy(&old_settings, &new_settings).is_ok() {
            crate::log::write("carried settings over from MemCap");
        }
    }
    found
}

/// Is the tray app set to start when the user signs in?
pub fn is_run_at_startup() -> bool {
    RegKey::open_read(RegHive::Hkcu, paths::RUN_KEY)
        .map(|k| k.get_raw(paths::RUN_VALUE).is_ok())
        .unwrap_or(false)
}

/// Turn start-at-sign-in on or off. On points at the installed tray app if there is one,
/// otherwise at the copy that is running.
pub fn set_run_at_startup(on: bool) -> Result<(), String> {
    let run = RegKey::create(RegHive::Hkcu, paths::RUN_KEY).map_err(|e| e.to_string())?;
    if !on {
        let _ = run.delete_value(paths::RUN_VALUE);
        return Ok(());
    }
    let installed = paths::install_dir().join(paths::TRAY_EXE);
    let exe = if installed.exists() {
        installed
    } else {
        std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(paths::TRAY_EXE)
    };
    run.set_string(paths::RUN_VALUE, &format!("\"{}\"", exe.display()), false)
        .map_err(|e| e.to_string())
}


/// What Settings > Apps runs to remove the app.
fn uninstall_command(dir: &Path) -> String {
    format!("\"{}\" --uninstall", dir.join(paths::TRAY_EXE).display())
}

/// The entry in Settings > Apps, so the app can be removed the usual way.
fn write_uninstall_entry(dir: &Path) -> Result<(), String> {
    let key = RegKey::create(RegHive::Hkcu, paths::UNINSTALL_KEY).map_err(|e| e.to_string())?;
    let set = |name: &str, value: &str| key.set_string(name, value, false).map_err(|e| e.to_string());
    set("DisplayName", paths::PRODUCT_NAME)?;
    set("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    set("Publisher", paths::PUBLISHER)?;
    set("DisplayIcon", &dir.join(paths::TRAY_EXE).display().to_string())?;
    set("InstallLocation", &dir.display().to_string())?;
    set("UninstallString", &uninstall_command(dir))?;
    let size_kb: u64 = [paths::TRAY_EXE, paths::CLI_EXE]
        .iter()
        .filter_map(|n| fs::metadata(dir.join(n)).ok())
        .map(|m| m.len() / 1024 + 1)
        .sum();
    key.set_dword("EstimatedSize", size_kb.min(u32::MAX as u64) as u32).map_err(|e| e.to_string())?;
    key.set_dword("NoModify", 1).map_err(|e| e.to_string())?;
    key.set_dword("NoRepair", 1).map_err(|e| e.to_string())?;
    Ok(())
}

/// After an update: if this is the installed copy and it is registered under Settings > Apps,
/// bring the entry up to date (version, size, uninstall command). Portable copies register
/// nothing, so nothing happens for them.
pub fn refresh_install_record() {
    let dir = paths::install_dir();
    let Ok(here) = std::env::current_exe() else { return };
    if !same_path(&here, &dir.join(paths::TRAY_EXE)) {
        return;
    }
    let registered = RegKey::open_read(RegHive::Hkcu, paths::UNINSTALL_KEY).is_ok();
    if registered {
        if let Err(e) = write_uninstall_entry(&dir) {
            crate::log::write(&format!("could not refresh the Settings > Apps entry: {}", e));
        }
    }
}

// ---------------------------------------------------------------- files

/// `memguard.exe`, carried inside `AtherMemGuard.exe` by release builds (`embed-cli`).
#[cfg(feature = "embed-cli")]
const EMBEDDED_CLI: Option<&[u8]> = Some(include_bytes!(env!("MEMGUARD_CLI_EMBED")));
#[cfg(not(feature = "embed-cli"))]
const EMBEDDED_CLI: Option<&[u8]> = None;

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Case-insensitive path comparison, ignoring a trailing separator (Windows paths).
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.display().to_string().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

static REPLACE_NONCE: AtomicU64 = AtomicU64::new(0);
const EMBEDDED_CLI_MAGIC: &[u8; 4] = b"AMGZ";
const MAX_EMBEDDED_CLI_BYTES: usize = 64 * 1024 * 1024;

/// Write `bytes` to `dest`. If `dest` is a program that is running (an agent waiting on
/// `memguard wait`, say), it can't be overwritten but it can be renamed: move it aside to
/// `<dest>.old` and put the new file in its place.
fn replace_file(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    if fs::read(dest).map(|cur| cur == bytes).unwrap_or(false) {
        return Ok(());
    }
    let nonce = REPLACE_NONCE.fetch_add(1, Ordering::Relaxed);
    let fresh = with_suffix(dest, &format!(".{}.{}.new", std::process::id(), nonce));
    let old = with_suffix(dest, ".old");
    let mut last = String::new();
    for _ in 0..20 {
        let attempt = (|| -> std::io::Result<()> {
            fs::write(&fresh, bytes)?;
            if fs::rename(&fresh, dest).is_err() && dest.exists() {
                let _ = fs::remove_file(&old);
                fs::rename(dest, &old)?;
                if let Err(e) = fs::rename(&fresh, dest) {
                    let _ = fs::rename(&old, dest);
                    return Err(e);
                }
                let _ = fs::remove_file(&old); // fails while the old one still runs; cleaned up next time
            }
            Ok(())
        })();
        match attempt {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        sleep(Duration::from_millis(250));
    }
    let _ = fs::remove_file(&fresh);
    Err(format!("Couldn't write {} ({}).", dest.display(), last))
}

/// Compress `raw` into the `[b"AMGZ", uncompressed_len: u32 LE, lznt1_payload...]` container.
pub fn encode_embedded_cli(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.is_empty() || raw.len() > MAX_EMBEDDED_CLI_BYTES {
        return Err(format!("embedded CLI size {} out of range (1..={})", raw.len(), MAX_EMBEDDED_CLI_BYTES));
    }
    let compressed = win::compress_lznt1(raw)?;
    let mut out = Vec::with_capacity(8 + compressed.len());
    out.extend_from_slice(EMBEDDED_CLI_MAGIC);
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// Decode an embedded CLI payload (`AMGZ` LZNT1 container, or raw `MZ` executable fallback).
pub fn decode_embedded_cli(bytes: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    if bytes.starts_with(EMBEDDED_CLI_MAGIC) {
        if bytes.len() < 8 {
            return Err("embedded CLI header is truncated".into());
        }
        let uncompressed_len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if uncompressed_len == 0 || uncompressed_len > MAX_EMBEDDED_CLI_BYTES {
            return Err(format!("embedded CLI uncompressed size {} is invalid", uncompressed_len));
        }
        let payload = &bytes[8..];
        if payload.is_empty() {
            return Err("embedded CLI compressed payload is empty".into());
        }
        let out = win::decompress_lznt1(payload, uncompressed_len)?;
        if out.len() != uncompressed_len {
            return Err(format!(
                "embedded CLI decompressed to {} bytes, expected {}",
                out.len(),
                uncompressed_len
            ));
        }
        return Ok(Cow::Owned(out));
    }
    if bytes.starts_with(b"MZ") {
        return Ok(Cow::Borrowed(bytes));
    }
    Err("embedded CLI has an unrecognized format".into())
}

/// Decode and atomically write an embedded CLI payload into `dir\memguard.exe`.
pub fn write_embedded_cli_bytes(dir: &Path, bytes: &[u8]) -> Result<(), String> {
    let decoded = decode_embedded_cli(bytes)?;
    replace_file(&dir.join(paths::CLI_EXE), &decoded)
}

/// Write the CLI that this exe carries into `dir`. Returns false (and does nothing) when this
/// build carries none, as in dev builds where `memguard.exe` is built beside it.
pub fn write_embedded_cli(dir: &Path) -> Result<bool, String> {
    match EMBEDDED_CLI {
        Some(bytes) => write_embedded_cli_bytes(dir, bytes).map(|_| true),
        None => Ok(false),
    }
}

/// Where each installed file comes from, given the running exe. The GUI exe may have any name
/// (`AtherMemGuard-Setup-0.2.0.exe`); `memguard.exe` is the CLI.
#[derive(Debug, PartialEq, Eq)]
struct Sources {
    tray: PathBuf,
    /// None: write the embedded copy.
    cli: Option<PathBuf>,
}

fn install_sources(here: &Path, embedded_cli: bool) -> Sources {
    let running_cli = here.file_name().is_some_and(|n| n.eq_ignore_ascii_case(paths::CLI_EXE));
    let tray = if running_cli { here.with_file_name(paths::TRAY_EXE) } else { here.to_path_buf() };
    let cli = if running_cli {
        Some(here.to_path_buf())
    } else if embedded_cli {
        None
    } else {
        Some(here.with_file_name(paths::CLI_EXE))
    };
    Sources { tray, cli }
}

/// Put `AtherMemGuard.exe` and `memguard.exe` into `dir`.
fn write_binaries(dir: &Path) -> Result<(), String> {
    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    let src = install_sources(&here, EMBEDDED_CLI.is_some());
    let copy = |from: &Path, name: &str| -> Result<(), String> {
        let to = dir.join(name);
        if same_path(from, &to) {
            return Ok(());
        }
        let bytes = fs::read(from).map_err(|_| format!("{} is missing next to {}; keep both files together.", name, file_name(&here)))?;
        replace_file(&to, &bytes)
    };
    copy(&src.tray, paths::TRAY_EXE)?;
    match &src.cli {
        Some(cli) => copy(cli, paths::CLI_EXE),
        None => write_embedded_cli(dir).map(|_| ()),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ---------------------------------------------------------------- Start menu shortcut

/// A Windows shortcut (.lnk, [MS-SHLLINK]) to `target`, built by hand so no COM is needed:
/// a header, the item ID list, a LinkInfo with the target's local path (ANSI and Unicode),
/// and the description, working folder and icon as Unicode strings.
fn shortcut_bytes(target: &Path, description: &str) -> Vec<u8> {
    const HAS_LINK_TARGET_ID_LIST: u32 = 0x01;
    const HAS_LINK_INFO: u32 = 0x02;
    const HAS_NAME: u32 = 0x04;
    const HAS_WORKING_DIR: u32 = 0x10;
    const HAS_ICON_LOCATION: u32 = 0x40;
    const IS_UNICODE: u32 = 0x80;
    const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
    const SW_SHOWNORMAL: u32 = 1;
    const DRIVE_FIXED: u32 = 3;

    let target_s = target.display().to_string();
    let workdir = target.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect() };
    let mut out = Vec::with_capacity(512);
    let u32le = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());

    // ShellLinkHeader, 76 bytes.
    u32le(&mut out, 0x4C);
    out.extend_from_slice(&[0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46]);
    let ids = id_list(target);
    let id_flag = if ids.is_some() { HAS_LINK_TARGET_ID_LIST } else { 0 };
    u32le(&mut out, id_flag | HAS_LINK_INFO | HAS_NAME | HAS_WORKING_DIR | HAS_ICON_LOCATION | IS_UNICODE);
    u32le(&mut out, FILE_ATTRIBUTE_ARCHIVE);
    out.extend_from_slice(&[0u8; 24]); // creation, access, write times
    u32le(&mut out, 0); // file size
    u32le(&mut out, 0); // icon index
    u32le(&mut out, SW_SHOWNORMAL);
    out.extend_from_slice(&[0u8; 2 + 2 + 4 + 4]); // hotkey, reserved

    if let Some(ids) = ids {
        out.extend_from_slice(&(ids.len() as u16).to_le_bytes());
        out.extend_from_slice(&ids);
    }

    // LinkInfo with the Unicode offsets (header size 0x24).
    let mut volume = Vec::new();
    u32le(&mut volume, 0x11); // VolumeIDSize: 16 + an empty label
    u32le(&mut volume, DRIVE_FIXED);
    u32le(&mut volume, 0); // serial number
    u32le(&mut volume, 0x10); // label offset
    volume.push(0);
    let ansi: Vec<u8> = target_s.chars().map(|c| if c.is_ascii() { c as u8 } else { b'?' }).chain([0]).collect();
    let mut unicode = utf16(&target_s);
    unicode.extend_from_slice(&[0, 0]);
    let header = 0x24u32;
    let volume_off = header;
    let base_off = volume_off + volume.len() as u32;
    let suffix_off = base_off + ansi.len() as u32;
    let base_off_u = suffix_off + 1;
    let suffix_off_u = base_off_u + unicode.len() as u32;
    let size = suffix_off_u + 2;
    for v in [size, header, 1 /* VolumeIDAndLocalBasePath */, volume_off, base_off, 0, suffix_off, base_off_u, suffix_off_u] {
        u32le(&mut out, v);
    }
    out.extend_from_slice(&volume);
    out.extend_from_slice(&ansi);
    out.push(0); // empty common path suffix
    out.extend_from_slice(&unicode);
    out.extend_from_slice(&[0, 0]);

    // StringData: description, working folder, icon (in that order).
    for s in [description, workdir.as_str(), target_s.as_str()] {
        let n = s.encode_utf16().count().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend(utf16(s).into_iter().take(n as usize * 2));
    }
    u32le(&mut out, 0); // terminal block
    out
}

/// The shell's item ID list for a path like `C:\dir\file.exe`: This PC, the drive, then one
/// file-system item per component, each with its Unicode name in a 0xBEEF0004 extension block
/// (the layout Windows itself writes). None for paths without a drive letter.
fn id_list(target: &Path) -> Option<Vec<u8>> {
    let s = target.display().to_string().replace('/', "\\");
    let b = s.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        return None;
    }
    let mut list = Vec::new();
    // This PC: {20D04FE0-3AEA-1069-A2D8-08002B30309D}
    list.extend_from_slice(&[
        0x14, 0x00, 0x1F, 0x50, 0xE0, 0x4F, 0xD0, 0x20, 0xEA, 0x3A, 0x69, 0x10, 0xA2, 0xD8, 0x08, 0x00, 0x2B, 0x30, 0x30, 0x9D,
    ]);
    let mut drive = vec![0x19, 0x00, 0x2F, b[0].to_ascii_uppercase(), b':', b'\\'];
    drive.resize(0x19, 0);
    list.extend_from_slice(&drive);
    let parts: Vec<&str> = s[3..].split('\\').filter(|p| !p.is_empty()).collect();
    for (i, name) in parts.iter().enumerate() {
        let is_file = i + 1 == parts.len();
        let mut item = vec![0u8, 0, if is_file { 0x32 } else { 0x31 }, 0];
        item.extend_from_slice(&[0u8; 4 + 2 + 2]); // size, DOS date and time
        item.extend_from_slice(&(if is_file { 0x20u16 } else { 0x10u16 }).to_le_bytes());
        item.extend(name.chars().map(|c| if c.is_ascii() { c as u8 } else { b'_' }));
        item.push(0);
        if item.len() % 2 == 1 {
            item.push(0);
        }
        let ext_offset = item.len() as u16;
        let mut long: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        long.extend_from_slice(&[0, 0]);
        let ext_size = (46 + long.len() + 2) as u16;
        item.extend_from_slice(&ext_size.to_le_bytes());
        item.extend_from_slice(&9u16.to_le_bytes()); // version
        item.extend_from_slice(&0xBEEF_0004u32.to_le_bytes());
        item.extend_from_slice(&[0u8; 8]); // created, accessed
        item.extend_from_slice(&0x2Eu16.to_le_bytes());
        item.extend_from_slice(&[0u8; 2 + 8 + 8 + 2 + 4 + 4]); // file reference, sizes, reserved
        item.extend_from_slice(&long);
        item.extend_from_slice(&ext_offset.to_le_bytes());
        let cb = item.len() as u16;
        item[0..2].copy_from_slice(&cb.to_le_bytes());
        list.extend_from_slice(&item);
    }
    list.extend_from_slice(&[0, 0]);
    Some(list)
}

fn create_shortcut(target: &Path, lnk: &Path) -> Result<(), String> {
    let bytes = shortcut_bytes(target, "Keeps memory free for Unreal Engine work");
    crate::config::write_atomic_bytes(lnk, &bytes)
}

// ---------------------------------------------------------------- launch mode

/// What the GUI exe does when started without arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Installed, or portable: run the tray app.
    Tray,
    /// Started from Downloads or anywhere else: offer to install.
    Setup,
}

fn decide_launch_mode(exe: &Path, install_dir: &Path, portable_marker: bool) -> LaunchMode {
    let in_install_dir = exe.parent().is_some_and(|d| same_path(d, install_dir));
    if in_install_dir || portable_marker {
        LaunchMode::Tray
    } else {
        LaunchMode::Setup
    }
}

/// Tray when the running exe is in the install folder or next to an `AtherMemGuard.portable`
/// file, Setup otherwise.
pub fn launch_mode() -> LaunchMode {
    let Ok(exe) = std::env::current_exe() else { return LaunchMode::Tray };
    let marker = exe.with_file_name(paths::PORTABLE_MARKER).exists();
    decide_launch_mode(&exe, &paths::install_dir(), marker)
}

// ---------------------------------------------------------------- install / uninstall

pub struct InstallResult {
    pub dir: PathBuf,
    pub stopped_previous: bool,
    pub migrated_from_memcap: bool,
    pub path_changed: bool,
    pub started: bool,
}

/// What `install` should set up besides copying the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallOptions {
    /// Put the install folder on the user PATH so `memguard` works in any terminal.
    pub add_to_path: bool,
    /// Start the tray app when the user signs in. Off removes an existing entry.
    pub start_with_windows: bool,
    /// Start the tray app right after installing.
    pub start: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions { add_to_path: true, start_with_windows: true, start: true }
    }
}

pub fn install(opts: InstallOptions) -> Result<InstallResult, String> {
    let InstallOptions { add_to_path, start_with_windows, start } = opts;
    let stopped_previous = stop_running();
    let migrated_from_memcap = migrate_from_memcap();
    let dir = paths::install_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write_binaries(&dir)?;

    crate::config::Config::load()?; // writes the default settings file if there is none

    let tray = dir.join(paths::TRAY_EXE);
    set_run_at_startup(start_with_windows)?;
    write_uninstall_entry(&dir)?;
    if let Err(e) = create_shortcut(&tray, &paths::start_menu_shortcut()) {
        crate::log::write(&format!("could not add the Start menu shortcut: {}", e));
    }

    let path_changed = if add_to_path { update_user_path(&paths::install_dir(), true)? } else { false };
    // ShellExecute starts it without inheriting our handles. A plain spawn would pass on
    // the caller's output pipe, and a script or agent reading it would wait forever.
    let started = start && win::launch_detached(&tray, "", false);
    crate::log::write(&format!("installed {} to {}", env!("CARGO_PKG_VERSION"), dir.display()));
    Ok(InstallResult { dir, stopped_previous, migrated_from_memcap, path_changed, started })
}

// ---------------------------------------------------------------- opt-in elevated helper task

fn elevated_helper_exe() -> Result<PathBuf, String> {
    let installed = paths::install_dir().join(paths::TRAY_EXE);
    if installed.exists() {
        return Ok(installed);
    }
    Ok(std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(paths::TRAY_EXE))
}

pub fn elevated_helper_create_args(exe: &Path) -> Vec<String> {
    let arg = if exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case(paths::CLI_EXE)) {
        "flush-cache"
    } else {
        "--flush-cache"
    };
    vec![
        "/Create".to_string(),
        "/F".to_string(),
        "/TN".to_string(),
        crate::ops::ELEVATED_HELPER_TASK.to_string(),
        "/TR".to_string(),
        format!("\"{}\" {}", exe.display(), arg),
        "/SC".to_string(),
        "ONCE".to_string(),
        "/ST".to_string(),
        "00:00".to_string(),
        "/RL".to_string(),
        "HIGHEST".to_string(),
    ]
}

pub fn elevated_helper_delete_args() -> [&'static str; 4] {
    ["/Delete", "/F", "/TN", crate::ops::ELEVATED_HELPER_TASK]
}

pub fn elevated_helper_query_args() -> [&'static str; 3] {
    ["/Query", "/TN", crate::ops::ELEVATED_HELPER_TASK]
}

fn run_schtasks(args: &[&str]) -> Result<(), String> {
    let status = Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("could not run schtasks.exe: {}", e))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("schtasks.exe exited with {}", status))
    }
}

/// Opt-in: register `ops::ELEVATED_HELPER_TASK` so `AtherMemGuard.exe --flush-cache` can run
/// elevated without UAC prompts. Requires administrator privileges; never called automatically.
pub fn install_elevated_helper() -> Result<(), String> {
    if cfg!(test) || paths::is_test_home() {
        return Err("refusing to modify scheduled tasks in test mode".into());
    }
    if !win::is_elevated() {
        return Err("installing the elevated helper task requires administrator privileges".into());
    }
    let exe = elevated_helper_exe()?;
    let args = elevated_helper_create_args(&exe);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_schtasks(&refs)
}

/// Remove the opt-in elevated helper scheduled task if present.
pub fn uninstall_elevated_helper() -> Result<(), String> {
    if cfg!(test) || paths::is_test_home() {
        return Ok(());
    }
    run_schtasks(&elevated_helper_delete_args())
}

/// True when the opt-in elevated helper scheduled task exists.
pub fn is_elevated_helper_installed() -> bool {
    if cfg!(test) || paths::is_test_home() {
        return false;
    }
    run_schtasks(&elevated_helper_query_args()).is_ok()
}

pub fn uninstall() -> Result<(), String> {
    stop_running();
    let _ = set_run_at_startup(false);
    let _ = win::reg_delete_tree(RegHive::Hkcu, paths::UNINSTALL_KEY);
    let _ = update_user_path(&paths::install_dir(), false);
    let _ = fs::remove_file(paths::start_menu_shortcut());
    if !paths::is_test_home() && is_elevated_helper_installed() {
        let _ = uninstall_elevated_helper();
    }

    let dir = paths::install_dir();
    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    if here.starts_with(&dir) {
        // A running exe can't delete itself: let cmd do it a moment after we exit, and retry
        // in case the exe is still closing.
        let rd = format!("rmdir /s /q \"{}\" 2>nul", dir.display());
        let params = format!("/c ping -n 3 127.0.0.1 >nul & {rd} & ping -n 3 127.0.0.1 >nul & {rd}");
        if !win::launch_detached(Path::new("cmd.exe"), &params, true) {
            return Err("could not schedule removal of the program folder".into());
        }
    } else if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    crate::log::write("uninstalled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_runs_the_gui_exe() {
        let dir = Path::new(r"C:\Users\me\AppData\Local\Programs\AtherMemGuard");
        assert_eq!(
            uninstall_command(dir),
            r#""C:\Users\me\AppData\Local\Programs\AtherMemGuard\AtherMemGuard.exe" --uninstall"#
        );
    }

    #[test]
    fn decides_launch_mode() {
        let install = Path::new(r"C:\Users\me\AppData\Local\Programs\AtherMemGuard");
        let installed = Path::new(r"C:\Users\me\AppData\Local\Programs\AtherMemGuard\AtherMemGuard.exe");
        let cased = Path::new(r"c:\users\ME\appdata\local\programs\athermemguard\AtherMemGuard.exe");
        let download = Path::new(r"C:\Users\me\Downloads\AtherMemGuard-Setup-0.2.0.exe");
        assert_eq!(decide_launch_mode(installed, install, false), LaunchMode::Tray);
        assert_eq!(decide_launch_mode(cased, install, false), LaunchMode::Tray);
        assert_eq!(decide_launch_mode(download, install, false), LaunchMode::Setup);
        assert_eq!(decide_launch_mode(download, install, true), LaunchMode::Tray);
        let sub = Path::new(r"C:\Users\me\AppData\Local\Programs\AtherMemGuard\old\AtherMemGuard.exe");
        assert_eq!(decide_launch_mode(sub, install, false), LaunchMode::Setup);
    }

    #[test]
    fn compares_paths_like_windows() {
        assert!(same_path(Path::new(r"C:\A\b\"), Path::new(r"c:\a\B")));
        assert!(same_path(Path::new("C:/A/b"), Path::new(r"C:\A\b")));
        assert!(!same_path(Path::new(r"C:\A\b"), Path::new(r"C:\A\bc")));
    }

    #[test]
    fn picks_install_sources() {
        let setup = Path::new(r"C:\Downloads\AtherMemGuard-Setup-0.2.0.exe");
        assert_eq!(install_sources(setup, true), Sources { tray: setup.to_path_buf(), cli: None });
        assert_eq!(
            install_sources(setup, false),
            Sources { tray: setup.to_path_buf(), cli: Some(PathBuf::from(r"C:\Downloads\memguard.exe")) }
        );
        let cli = Path::new(r"C:\Tools\MEMGUARD.EXE");
        assert_eq!(
            install_sources(cli, true),
            Sources { tray: PathBuf::from(r"C:\Tools\AtherMemGuard.exe"), cli: Some(cli.to_path_buf()) }
        );
    }

    #[test]
    fn builds_a_shortcut() {
        let target = Path::new(r"C:\Users\Zoë\AppData\Local\Programs\AtherMemGuard\AtherMemGuard.exe");
        let b = shortcut_bytes(target, "Desc");
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        assert_eq!(u32_at(0), 0x4C);
        assert_eq!(&b[4..20], &[0x01, 0x14, 0x02, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
        assert_eq!(u32_at(20), 0x01 | 0x02 | 0x04 | 0x10 | 0x40 | 0x80);
        // The ID list: This PC, C:\, then one item per component, each ending in its Unicode name.
        let ids_len = u16::from_le_bytes([b[76], b[77]]) as usize;
        let ids = &b[78..78 + ids_len];
        assert_eq!(&ids[..4], &[0x14, 0x00, 0x1F, 0x50]);
        assert_eq!(&ids[20..26], &[0x19, 0x00, 0x2F, b'C', b':', b'\\']);
        let mut at = 20 + 0x19;
        let mut names = Vec::new();
        loop {
            let cb = u16::from_le_bytes([ids[at], ids[at + 1]]) as usize;
            if cb == 0 {
                break;
            }
            let item = &ids[at..at + cb];
            let ext = u16::from_le_bytes([item[cb - 2], item[cb - 1]]) as usize;
            assert_eq!(&item[ext + 4..ext + 8], &0xBEEF_0004u32.to_le_bytes());
            let name: Vec<u16> =
                item[ext + 46..cb - 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
            names.push((item[2], String::from_utf16(&name).unwrap()));
            at += cb;
        }
        assert_eq!(at + 2, ids_len);
        assert_eq!(names.first(), Some(&(0x31, "Users".to_string())));
        assert_eq!(names[1], (0x31, "Zoë".to_string()));
        assert_eq!(names.last(), Some(&(0x32, "AtherMemGuard.exe".to_string())));
        assert!(id_list(Path::new(r"\\server\share\x.exe")).is_none());
        // LinkInfo follows the ID list; its size takes us to the string data.
        let li = 78 + ids_len;
        let li_size = u32_at(li) as usize;
        assert_eq!(u32_at(li + 4), 0x24);
        let base_u = li + u32_at(li + 28) as usize;
        let path: Vec<u16> = b[base_u..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        assert_eq!(String::from_utf16(&path).unwrap(), target.display().to_string());
        let mut i = li + li_size;
        let mut strings = Vec::new();
        for _ in 0..3 {
            let n = u16::from_le_bytes([b[i], b[i + 1]]) as usize;
            let s: Vec<u16> = b[i + 2..i + 2 + n * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            strings.push(String::from_utf16(&s).unwrap());
            i += 2 + n * 2;
        }
        assert_eq!(strings[0], "Desc");
        assert_eq!(strings[1], r"C:\Users\Zoë\AppData\Local\Programs\AtherMemGuard");
        assert_eq!(strings[2], target.display().to_string());
        assert_eq!(u32_at(i), 0);
        assert_eq!(b.len(), i + 4);
    }

    #[test]
    fn replaces_files() {
        let dir = std::env::temp_dir().join(format!("memguard-install-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("memguard.exe");
        replace_file(&f, b"one").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"one");
        replace_file(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        assert!(!dir.join("memguard.exe.new").exists() && !dir.join("memguard.exe.old").exists());
        let lnk = dir.join("Ather MemGuard.lnk");
        create_shortcut(&f, &lnk).unwrap();
        assert!(fs::metadata(&lnk).unwrap().len() > 100);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lznt1_embedded_cli_round_trip_and_fallback() {
        let dir = std::env::temp_dir().join(format!("memguard-lznt1-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut payload = b"MZ\x90\x00".to_vec();
        for i in 0..8192u32 {
            payload.extend_from_slice(&(i % 13).to_le_bytes());
        }
        let encoded = encode_embedded_cli(&payload).unwrap();
        assert!(encoded.starts_with(b"AMGZ"));
        assert!(encoded.len() < payload.len());
        assert_eq!(decode_embedded_cli(&encoded).unwrap().as_ref(), payload.as_slice());

        write_embedded_cli_bytes(&dir, &encoded).unwrap();
        assert_eq!(fs::read(dir.join(paths::CLI_EXE)).unwrap(), payload);

        // Raw MZ fallback still writes verbatim.
        let raw_mz = b"MZ-raw-cli-binary-test";
        write_embedded_cli_bytes(&dir, raw_mz).unwrap();
        assert_eq!(fs::read(dir.join(paths::CLI_EXE)).unwrap(), raw_mz);

        // Error cases: truncated header, invalid size, empty/corrupt payload, unrecognized magic.
        assert!(decode_embedded_cli(b"AMGZ\x01").is_err());
        assert!(decode_embedded_cli(b"AMGZ\x00\x00\x00\x00\x01").is_err());
        let mut oversized = b"AMGZ".to_vec();
        oversized.extend_from_slice(&((MAX_EMBEDDED_CLI_BYTES as u32) + 1).to_le_bytes());
        oversized.push(0);
        assert!(decode_embedded_cli(&oversized).is_err());
        let mut bad_len = encoded.clone();
        let wrong_len = (payload.len() as u32) + 16;
        bad_len[4..8].copy_from_slice(&wrong_len.to_le_bytes());
        assert!(decode_embedded_cli(&bad_len).is_err());
        assert!(decode_embedded_cli(b"AMGZ\x10\x00\x00\x00").is_err());
        assert!(decode_embedded_cli(b"NOPE1234").is_err());
        let mut corrupt = b"AMGZ".to_vec();
        corrupt.extend_from_slice(&64u32.to_le_bytes());
        corrupt.extend_from_slice(&[0xff; 12]);
        assert!(decode_embedded_cli(&corrupt).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn elevated_helper_args_and_test_guard() {
        let tray_exe = Path::new(r"C:\Apps\AtherMemGuard\AtherMemGuard.exe");
        let args = elevated_helper_create_args(tray_exe);
        assert_eq!(
            args,
            vec![
                "/Create",
                "/F",
                "/TN",
                crate::ops::ELEVATED_HELPER_TASK,
                "/TR",
                r#""C:\Apps\AtherMemGuard\AtherMemGuard.exe" --flush-cache"#,
                "/SC",
                "ONCE",
                "/ST",
                "00:00",
                "/RL",
                "HIGHEST",
            ]
        );
        let cli_args = elevated_helper_create_args(Path::new(r"C:\Apps\AtherMemGuard\memguard.exe"));
        assert_eq!(cli_args[5], r#""C:\Apps\AtherMemGuard\memguard.exe" flush-cache"#);
        assert_eq!(elevated_helper_delete_args(), ["/Delete", "/F", "/TN", crate::ops::ELEVATED_HELPER_TASK]);
        assert_eq!(elevated_helper_query_args(), ["/Query", "/TN", crate::ops::ELEVATED_HELPER_TASK]);

        // Guarded under cfg!(test): never spawns schtasks.exe.
        assert!(!is_elevated_helper_installed());
        assert!(install_elevated_helper().is_err());
        assert!(uninstall_elevated_helper().is_ok());
    }

    #[test]
    fn t5_embedded_cli_empty_oversize_and_incompressible_roundtrip() {
        // Empty payload must be rejected cleanly.
        assert!(encode_embedded_cli(&[]).is_err());
        assert!(decode_embedded_cli(b"").is_err());

        // Header claiming > 64 MiB uncompressed size must be rejected cleanly.
        let mut oversize = Vec::from(*b"AMGZ");
        oversize.extend_from_slice(&((65u32 * 1024 * 1024).to_le_bytes()));
        oversize.extend_from_slice(&[0u8; 16]);
        assert!(decode_embedded_cli(&oversize).is_err(), "65 MiB AMGZ header must fail validation");

        // Incompressible > 128 KiB pseudo-random payload must survive LZNT1 chunked roundtrip.
        let mut incomp = vec![0u8; 160_000];
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for b in &mut incomp {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 24) as u8;
        }
        let encoded = encode_embedded_cli(&incomp).expect("encode incompressible >128KB");
        let decoded = decode_embedded_cli(&encoded).expect("decode incompressible >128KB");
        assert_eq!(decoded.as_ref(), incomp.as_slice());
    }
}

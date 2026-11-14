//! Per-user install: copy the executables, start at sign-in, optionally add to PATH.

use std::fs;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ};
use winreg::{RegKey, RegValue};

use crate::paths;
use crate::win;

const BINARIES: [&str; 2] = [paths::CLI_EXE, paths::TRAY_EXE];

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

fn copy_with_retry(from: &PathBuf, to: &PathBuf) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..20 {
        match fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        sleep(Duration::from_millis(250));
    }
    Err(format!("could not copy to {}: {}", to.display(), last))
}

fn to_wide_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect()
}

fn from_wide_bytes(b: &[u8]) -> String {
    let wide: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    String::from_utf16_lossy(&wide)
}

/// Add or remove the install folder on the user's PATH, keeping the value's registry type.
fn update_user_path(dir: &std::path::Path, add: bool) -> Result<bool, String> {
    let dir = dir.display().to_string();
    let env = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
        .map_err(|e| e.to_string())?;
    let (current, vtype) = match env.get_raw_value("Path") {
        Ok(v) => (from_wide_bytes(&v.bytes), v.vtype),
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
    let vtype = if matches!(vtype, REG_SZ | REG_EXPAND_SZ) { vtype } else { REG_EXPAND_SZ };
    env.set_raw_value("Path", &RegValue { bytes: to_wide_bytes(&parts.join(";")), vtype }).map_err(|e| e.to_string())?;
    crate::tray::broadcast_environment_change();
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
    if let Ok(run) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(paths::RUN_KEY, KEY_READ | KEY_WRITE) {
        if run.get_raw_value("MemCap").is_ok() {
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
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(paths::RUN_KEY, KEY_READ)
        .map(|k| k.get_raw_value(paths::RUN_VALUE).is_ok())
        .unwrap_or(false)
}

/// Turn start-at-sign-in on or off. On points at the installed tray app if there is one,
/// otherwise at the copy that is running.
pub fn set_run_at_startup(on: bool) -> Result<(), String> {
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(paths::RUN_KEY).map_err(|e| e.to_string())?;
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
    run.set_value(paths::RUN_VALUE, &format!("\"{}\"", exe.display())).map_err(|e| e.to_string())
}

/// The entry in Settings > Apps, so the app can be removed the usual way.
fn write_uninstall_entry(dir: &std::path::Path) -> Result<(), String> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(paths::UNINSTALL_KEY).map_err(|e| e.to_string())?;
    let cli = dir.join(paths::CLI_EXE);
    let set = |name: &str, value: &str| key.set_value(name, &value.to_string()).map_err(|e| e.to_string());
    set("DisplayName", paths::PRODUCT_NAME)?;
    set("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    set("Publisher", paths::PUBLISHER)?;
    set("DisplayIcon", &dir.join(paths::TRAY_EXE).display().to_string())?;
    set("InstallLocation", &dir.display().to_string())?;
    set("UninstallString", &format!("\"{}\" uninstall", cli.display()))?;
    key.set_value("NoModify", &1u32).map_err(|e| e.to_string())?;
    key.set_value("NoRepair", &1u32).map_err(|e| e.to_string())?;
    Ok(())
}

pub struct InstallResult {
    pub dir: PathBuf,
    pub stopped_previous: bool,
    pub migrated_from_memcap: bool,
    pub path_changed: bool,
    pub started: bool,
}

pub fn install(add_to_path: bool, start: bool) -> Result<InstallResult, String> {
    let stopped_previous = stop_running();
    let migrated_from_memcap = migrate_from_memcap();
    let dir = paths::install_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    let source_dir = here.parent().ok_or("cannot find my own folder")?.to_path_buf();
    if source_dir != dir {
        for name in BINARIES {
            let from = source_dir.join(name);
            if !from.exists() {
                return Err(format!("{} is missing next to memguard.exe; copy both files", name));
            }
            copy_with_retry(&from, &dir.join(name))?;
        }
    }

    crate::config::Config::load()?; // writes the default settings file if there is none

    let tray = dir.join(paths::TRAY_EXE);
    set_run_at_startup(true)?;
    write_uninstall_entry(&dir)?;

    let path_changed = if add_to_path { update_user_path(&paths::install_dir(), true)? } else { false };
    // ShellExecute starts it without inheriting our handles. A plain spawn would pass on
    // the caller's output pipe, and a script or agent reading it would wait forever.
    let started = start && crate::tray::launch_detached(&tray, "", false);
    crate::log::write(&format!("installed to {}", dir.display()));
    Ok(InstallResult { dir, stopped_previous, migrated_from_memcap, path_changed, started })
}

pub fn uninstall() -> Result<(), String> {
    stop_running();
    let _ = set_run_at_startup(false);
    let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(paths::UNINSTALL_KEY);
    let _ = update_user_path(&paths::install_dir(), false);

    let dir = paths::install_dir();
    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    if here.starts_with(&dir) {
        // A running exe can't delete itself: let cmd do it a moment after we exit.
        let params = format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
        if !crate::tray::launch_detached(std::path::Path::new("cmd.exe"), &params, true) {
            return Err("could not schedule removal of the program folder".into());
        }
    } else if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    crate::log::write("uninstalled");
    Ok(())
}

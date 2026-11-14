use std::path::PathBuf;

/// Internal name: folders, registry values, executables.
pub const APP_NAME: &str = "AtherMemGuard";
/// The name people see.
pub const PRODUCT_NAME: &str = "Ather MemGuard";
pub const PUBLISHER: &str = "Ather";

pub const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const RUN_VALUE: &str = "AtherMemGuard";
pub const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\AtherMemGuard";

pub const TRAY_EXE: &str = "AtherMemGuard.exe";
pub const CLI_EXE: &str = "memguard.exe";

/// Setting ATHER_MEMGUARD_DIR points the app at another data folder and gives it its own
/// instance names, so a test copy can run beside the installed one.
fn test_home() -> Option<PathBuf> {
    std::env::var_os("ATHER_MEMGUARD_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn mutex() -> &'static str {
    if test_home().is_some() {
        "Local\\AtherMemGuard.Instance.Test"
    } else {
        "Local\\AtherMemGuard.Instance"
    }
}

pub fn exit_event() -> &'static str {
    if test_home().is_some() {
        "Local\\AtherMemGuard.Exit.Test"
    } else {
        "Local\\AtherMemGuard.Exit"
    }
}

fn env_dir(name: &str) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Settings, log and status live here: %APPDATA%\AtherMemGuard, like other Ather apps.
pub fn data_dir() -> PathBuf {
    if let Some(home) = test_home() {
        return home;
    }
    env_dir("APPDATA").join(APP_NAME)
}

/// Where `memguard install` puts the executables.
pub fn install_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join("Programs").join(APP_NAME)
}

pub fn config_file() -> PathBuf {
    data_dir().join("settings.toml")
}

pub fn log_file() -> PathBuf {
    data_dir().join("memguard.log")
}

pub fn status_file() -> PathBuf {
    data_dir().join("status.json")
}

/// A file with this name next to `AtherMemGuard.exe` makes it run as the tray app from that
/// folder instead of offering to install (portable zip, dev builds).
pub const PORTABLE_MARKER: &str = "AtherMemGuard.portable";

/// The Start menu entry `install` adds: %APPDATA%\Microsoft\Windows\Start Menu\Programs.
pub fn start_menu_shortcut() -> PathBuf {
    env_dir("APPDATA")
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join(format!("{}.lnk", PRODUCT_NAME))
}

/// Overrides the update manifest URL (tests); only then is plain http to this PC allowed.
pub const UPDATE_URL_ENV: &str = "ATHER_MEMGUARD_UPDATE_URL";
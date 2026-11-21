use std::path::{Path, PathBuf};

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

pub fn is_test_home() -> bool {
    test_home().is_some()
}

fn normalize_home(p: &Path) -> String {
    let mut s: String = p
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' { '\\' } else { c.to_ascii_lowercase() })
        .collect();
    while s.ends_with('\\') && !(s.len() == 3 && s.as_bytes()[1] == b':') && s.len() > 1 {
        s.pop();
    }
    s
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

pub fn home_scope_id_for(home: Option<&Path>) -> Option<String> {
    home.filter(|p| !p.as_os_str().is_empty())
        .map(|p| format!("{:016x}", fnv1a_64(normalize_home(p).as_bytes())))
}

pub fn home_scope_id() -> Option<String> {
    home_scope_id_for(test_home().as_deref())
}

pub fn scoped_kernel_name_for(base: &str, home: Option<&Path>) -> String {
    match home_scope_id_for(home) {
        Some(id) => format!("{}.Test.{}", base, id),
        None => base.to_string(),
    }
}

pub fn scoped_kernel_name(base: &str) -> String {
    scoped_kernel_name_for(base, test_home().as_deref())
}

pub fn control_pipe_for(home: Option<&Path>) -> String {
    scoped_kernel_name_for(r"\\.\pipe\AtherMemGuard.Control", home)
}

pub fn control_pipe() -> String {
    control_pipe_for(test_home().as_deref())
}

pub fn control_event_for(home: Option<&Path>) -> String {
    scoped_kernel_name_for(r"Local\AtherMemGuard.Control", home)
}

pub fn control_event() -> String {
    control_event_for(test_home().as_deref())
}

pub fn status_mapping_for(home: Option<&Path>) -> String {
    scoped_kernel_name_for(r"Local\AtherMemGuard.Status", home)
}

pub fn status_mapping() -> String {
    status_mapping_for(test_home().as_deref())
}

pub fn mutex_for(home: Option<&Path>) -> String {
    scoped_kernel_name_for(r"Local\AtherMemGuard.Instance", home)
}

pub fn mutex() -> String {
    mutex_for(test_home().as_deref())
}

pub fn exit_event_for(home: Option<&Path>) -> String {
    scoped_kernel_name_for(r"Local\AtherMemGuard.Exit", home)
}

pub fn exit_event() -> String {
    exit_event_for(test_home().as_deref())
}

/// Signalled by `AtherMemGuard.exe --settings` to ask the running tray to open its settings
/// window (waking its event loop immediately).
pub fn settings_event_for(home: Option<&Path>) -> String {
    scoped_kernel_name_for(r"Local\AtherMemGuard.OpenSettings", home)
}

pub fn settings_event() -> String {
    settings_event_for(test_home().as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_names_isolate_production_and_distinct_temp_dirs() {
        assert_eq!(home_scope_id_for(None), None);
        assert_eq!(home_scope_id_for(Some(Path::new(""))), None);
        assert_eq!(control_pipe_for(None), r"\\.\pipe\AtherMemGuard.Control");
        assert_eq!(control_event_for(None), r"Local\AtherMemGuard.Control");
        assert_eq!(status_mapping_for(None), r"Local\AtherMemGuard.Status");
        assert_eq!(mutex_for(None), r"Local\AtherMemGuard.Instance");
        assert_eq!(exit_event_for(None), r"Local\AtherMemGuard.Exit");
        assert_eq!(settings_event_for(None), r"Local\AtherMemGuard.OpenSettings");

        let dir_a = Path::new(r"C:\Temp\amg-test-alpha");
        let dir_b = Path::new(r"C:\Temp\amg-test-beta");
        let dir_a_alt = Path::new(r"c:/temp/amg-test-alpha/");
        assert_eq!(home_scope_id_for(Some(dir_a)), home_scope_id_for(Some(dir_a_alt)));
        assert_ne!(home_scope_id_for(Some(dir_a)), home_scope_id_for(Some(dir_b)));

        let pipe_a = control_pipe_for(Some(dir_a));
        let pipe_b = control_pipe_for(Some(dir_b));
        assert_ne!(pipe_a, control_pipe_for(None));
        assert_ne!(pipe_b, control_pipe_for(None));
        assert_ne!(pipe_a, pipe_b);
        assert!(pipe_a.starts_with(r"\\.\pipe\AtherMemGuard.Control.Test."));

        assert_ne!(control_event_for(Some(dir_a)), control_event_for(None));
        assert_ne!(control_event_for(Some(dir_a)), control_event_for(Some(dir_b)));
        assert_ne!(status_mapping_for(Some(dir_a)), status_mapping_for(None));
        assert_ne!(status_mapping_for(Some(dir_a)), status_mapping_for(Some(dir_b)));

        assert_eq!(mutex_for(Some(dir_a)), mutex_for(Some(dir_a_alt)));
        assert_ne!(mutex_for(Some(dir_a)), mutex_for(None));
        assert_ne!(mutex_for(Some(dir_a)), mutex_for(Some(dir_b)));
        assert!(mutex_for(Some(dir_a)).starts_with(r"Local\AtherMemGuard.Instance.Test."));

        assert_eq!(exit_event_for(Some(dir_a)), exit_event_for(Some(dir_a_alt)));
        assert_ne!(exit_event_for(Some(dir_a)), exit_event_for(None));
        assert_ne!(exit_event_for(Some(dir_a)), exit_event_for(Some(dir_b)));
        assert!(exit_event_for(Some(dir_a)).starts_with(r"Local\AtherMemGuard.Exit.Test."));

        assert_eq!(settings_event_for(Some(dir_a)), settings_event_for(Some(dir_a_alt)));
        assert_ne!(settings_event_for(Some(dir_a)), settings_event_for(None));
        assert_ne!(settings_event_for(Some(dir_a)), settings_event_for(Some(dir_b)));
        assert!(settings_event_for(Some(dir_a)).starts_with(r"Local\AtherMemGuard.OpenSettings.Test."));
    }

    #[test]
    fn t5_ather_memguard_dir_canonicalization_and_scoped_kernel_name() {
        let a = scoped_kernel_name_for("Local\\MemGuard", Some(Path::new(r"C:\Temp\AmgTest\")));
        let b = scoped_kernel_name_for("Local\\MemGuard", Some(Path::new(r"c:/temp/amgtest")));
        assert_eq!(a, b);
        assert!(a.starts_with("Local\\MemGuard.Test."), "{}", a);

        let slash_a = Path::new(r"C:\Temp\AmgTest\sub\\\");
        let slash_b = Path::new(r"c:/temp/amgtest/sub/");
        assert_eq!(
            scoped_kernel_name_for("Local\\MemGuard", Some(slash_a)),
            scoped_kernel_name_for("Local\\MemGuard", Some(slash_b))
        );
    }
}
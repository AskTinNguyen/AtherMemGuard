use std::path::PathBuf;

/// Setting MEMCAP_HOME points MemCap at another data folder and gives it its own
/// instance names, so a test copy can run beside the installed one.
fn test_home() -> Option<PathBuf> {
    std::env::var_os("MEMCAP_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn mutex() -> &'static str {
    if test_home().is_some() { "Local\\MemCap-Running-Test" } else { "Local\\MemCap-Running" }
}

pub fn exit_event() -> &'static str {
    if test_home().is_some() { "Local\\MemCap-Exit-Test" } else { "Local\\MemCap-Exit" }
}
pub const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const RUN_VALUE: &str = "MemCap";

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Settings, log and status live here.
pub fn data_dir() -> PathBuf {
    if let Some(home) = test_home() {
        return home;
    }
    local_app_data().join("MemCap")
}

/// Where `memcap install` puts the executables.
pub fn install_dir() -> PathBuf {
    local_app_data().join("Programs").join("MemCap")
}

pub fn config_file() -> PathBuf {
    data_dir().join("memcap.toml")
}

pub fn log_file() -> PathBuf {
    data_dir().join("memcap.log")
}

pub fn status_file() -> PathBuf {
    data_dir().join("status.json")
}

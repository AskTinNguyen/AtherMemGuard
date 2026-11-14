//! AtherMemGuard: keeps a Windows PC primed for agentic Unreal Engine work.
//!
//! - `win`     thin safe wrappers over the Win32 / NT calls we need
//! - `config`  TOML settings (groups, pressure thresholds, UE process names)
//! - `engine`  snapshot -> analyze (pure) -> apply (soft caps, hard caps, throttling)
//! - `ops`     one-shot operations for agents: prime, wait, run, advise
//! - `install` per-user install, autostart, PATH
//! - `mcp`     MCP server over stdio, so coding agents can call MemGuard as tools
//! - `tray`    tray icon + background loop

pub mod config;
pub mod engine;
pub mod install;
pub mod logo;
pub mod mcp;
pub mod ops;
pub mod paths;
pub mod tray;
pub mod win;

pub mod log {
    use std::fs;
    use std::io::Write;

    /// Append one timestamped line to the log. Never fails loudly.
    pub fn write(msg: &str) {
        let path = crate::paths::log_file();
        let _ = fs::create_dir_all(crate::paths::data_dir());
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > 2 * 1024 * 1024 {
                let _ = fs::rename(&path, path.with_extension("log.old"));
            }
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}  {}", crate::win::local_time_string(), msg);
        }
    }
}

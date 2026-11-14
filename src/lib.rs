//! AtherMemGuard: keeps a Windows PC primed for agentic Unreal Engine work.
//!
//! - `win`     thin safe wrappers over the Win32 / NT calls we need
//! - `config`  TOML settings (groups, pressure thresholds, UE process names)
//! - `engine`  snapshot -> analyze (pure) -> apply (soft caps, hard caps, throttling)
//! - `ops`     one-shot operations for agents: prime, wait, run, advise
//! - `history` memory history: 24-hour ring, daily CSV files, editor growth slope
//! - `install` per-user install, autostart, PATH, Start menu, launch mode
//! - `mcp`     MCP server over stdio, so coding agents can call MemGuard as tools
//! - `doctor`  checkup: what uses memory Unreal work doesn't need, and what to do about it
//! - `startup` reading and switching "starts with Windows" entries (used by `doctor`)
//! - `tray`    tray icon + background loop
//! - `net`     HTTP(S) GET through WinHTTP
//! - `updater` check, download, verify and swap in a new version
//! - `ramcheck` RAM and BIOS check: memory below its rated speed, slots, BIOS age (Part G)
//! - `smbios`  reading and parsing the firmware SMBIOS tables (Part G)
//! - `disk`    disk headroom, Dev Drive health and the wait gate
//! - `disk_space` space accounting without double counting (pure)
//! - `disk_scan` the report-only disk checkup and plan, scanned in a low-priority child
//! - `disk_win` volumes, FSCTLs and the read-only scanner

pub mod breakdown;
pub mod config;
pub mod disk; // disk headroom and Dev Drive health
pub mod disk_scan; // the disk checkup and plan (report only)
pub mod disk_space; // space accounting without double counting (pure)
pub mod disk_win;
pub mod doctor;
pub mod drivers; // which drivers and Windows processes hold the "Windows and drivers" RAM
mod doctor_win;
pub mod engine;
pub mod history;
pub mod install;
pub mod logo;
pub mod mcp;
pub mod net;
pub mod ops;
pub mod paths;
pub mod ramcheck; // Part G: RAM and BIOS check
pub mod report_page;
pub mod smbios; // Part G: firmware tables for the RAM check
pub mod startup;
pub mod tray;
pub mod ui;
pub mod updater;
pub mod win;

pub mod log {
    use std::fs;
    use std::io::Write;

    /// Append one timestamped line to the log. Never fails loudly.
    pub fn write(msg: &str) {
        // Unit tests must never write to the real data folder.
        if cfg!(test) {
            return;
        }
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

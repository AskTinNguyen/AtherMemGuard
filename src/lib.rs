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
//! - `disk_clean` cleanup with undo: delete, the bin, tool cleans, the measured result (Phase 2)
//! - `disk_relocate` moving caches off the system drive with user environment variables
//! - `disk_win` volumes, FSCTLs, the read-only scanner and the environment broadcast

pub mod breakdown;
pub mod config;
pub mod disk; // disk headroom and Dev Drive health
pub mod disk_clean; // cleanup with undo, the bin and the measured result
pub mod disk_relocate; // moving caches off the system drive
pub mod disk_scan; // the disk checkup and plan (report only)
pub mod disk_space; // space accounting without double counting (pure)
pub mod disk_win;
pub mod doctor;
pub mod drivers; // which drivers and Windows processes hold the "Windows and drivers" RAM
mod doctor_win;
pub mod engine;
pub mod history;
pub mod install;
pub mod ipc;
#[cfg(feature = "gui")]
pub mod logo;
pub mod mcp;
#[cfg(feature = "gui")]
pub mod net;
pub mod ops;
pub mod paths;
pub mod ramcheck; // Part G: RAM and BIOS check
pub mod report_page;
pub mod smbios; // Part G: firmware tables for the RAM check
pub mod startup;
#[cfg(feature = "gui")]
pub mod tray;
#[cfg(feature = "gui")]
pub mod ui;
#[cfg(feature = "gui")]
pub mod updater;
pub mod win;

pub mod log {
    use std::fs;
    use std::io::{ErrorKind, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    static LOG_LOCK: Mutex<()> = Mutex::new(());

    struct LockFile(Option<PathBuf>);

    impl Drop for LockFile {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = fs::remove_file(p);
            }
        }
    }

    fn acquire_lockfile(lock_path: &Path) -> LockFile {
        let pid = std::process::id();
        for attempt in 0..80u32 {
            match fs::OpenOptions::new().write(true).create_new(true).open(lock_path) {
                Ok(f) => {
                    drop(f);
                    return LockFile(Some(lock_path.to_path_buf()));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if let Ok(meta) = fs::metadata(lock_path) {
                        if meta
                            .modified()
                            .ok()
                            .and_then(|t| t.elapsed().ok())
                            .is_some_and(|age| age > Duration::from_secs(5))
                        {
                            let _ = fs::remove_file(lock_path);
                            continue;
                        }
                    }
                    let wait_ms = 2 + ((pid.wrapping_add(attempt)) % 5) as u64;
                    std::thread::sleep(Duration::from_millis(wait_ms));
                }
                Err(_) => return LockFile(None),
            }
        }
        LockFile(None)
    }

    fn is_sharing_err(e: &std::io::Error) -> bool {
        e.kind() == ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5 | 32 | 33))
    }

    /// Append one timestamped line to the log. Never fails loudly.
    pub fn write(msg: &str) {
        // Unit tests must never write to the real data folder.
        if cfg!(test) {
            return;
        }
        let line = format!("{}  {}\n", crate::win::local_time_string(), msg);
        let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = crate::paths::log_file();
        let _ = fs::create_dir_all(crate::paths::data_dir());
        let _file_lock = acquire_lockfile(&path.with_extension("log.lock"));
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > 2 * 1024 * 1024 {
                let old = path.with_extension("log.old");
                for attempt in 0..8 {
                    match fs::rename(&path, &old) {
                        Ok(()) => break,
                        Err(e) if is_sharing_err(&e) && attempt < 7 => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        for attempt in 0..8 {
            match fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(mut f) => {
                    let _ = f.write_all(line.as_bytes());
                    break;
                }
                Err(e) if is_sharing_err(&e) && attempt < 7 => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }
}

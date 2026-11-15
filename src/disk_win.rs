//! The Win32 side of the disk features: volumes and their free space, the Dev Drive and ReFS
//! facts readable on a root handle, the Defender probe, and the low-priority scanner that
//! reads directory records and extent reference counts. Everything here is read-only: files
//! and folders are opened with `FILE_READ_ATTRIBUTES` (or `FILE_LIST_DIRECTORY`) only, reparse
//! points are never followed, and nothing is written. Each `unsafe` block is small and only
//! passes buffers it owns to the API it documents. The record layouts follow the prototype
//! `spaceprobe`, which was cross-checked against a full count of a ReFS volume.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW,
    GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::disk_space::{Ext, ItemExtents};
use crate::win::{wide, Handle};

const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_LIST_DIRECTORY: u32 = 0x1;
const SYNCHRONIZE: u32 = 0x0010_0000;
const SHARE_ALL: u32 = 7;
const OPEN_EXISTING: u32 = 3;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

pub const A_DIR: u32 = 0x10;
pub const A_SPARSE: u32 = 0x200;
pub const A_REPARSE: u32 = 0x400;
pub const A_COMPRESSED: u32 = 0x800;
pub const A_OFFLINE: u32 = 0x1000;
pub const A_RECALL_OPEN: u32 = 0x4_0000;
pub const A_RECALL_DATA: u32 = 0x40_0000;

/// `GetVolumeInformationW` flags.
pub const FS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
pub const FS_HARD_LINKS: u32 = 0x0040_0000;

// Not in windows-sys 0.59 (or not under the features we use): declared here, as in winioctl.h.
const FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT: u32 = 0x0009_03D3;
const FSCTL_GET_RETRIEVAL_POINTERS: u32 = 0x0009_0073;
const FSCTL_GET_REFS_VOLUME_DATA: u32 = 0x0009_02D8;
const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const FSCTL_QUERY_PERSISTENT_VOLUME_STATE: u32 = 0x0009_023C;
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
/// `PERSISTENT_VOLUME_STATE_DEV_VOLUME` and `..._TRUSTED_VOLUME`.
const STATE_DEV_VOLUME: u32 = 0x2000;
const STATE_TRUSTED_VOLUME: u32 = 0x4000;
/// `STORAGE_BUS_TYPE::BusTypeFileBackedVirtual` (a VHD or VHDX).
pub const BUS_FILE_BACKED_VIRTUAL: u32 = 15;

// FILE_INFO_BY_HANDLE_CLASS values.
const FILE_BASIC_INFO_CLASS: i32 = 0;
const FILE_STANDARD_INFO_CLASS: i32 = 1;
const FILE_ID_INFO_CLASS: i32 = 18;
const FILE_ID_EXTD_DIRECTORY_INFO_CLASS: i32 = 19;

const ERROR_MORE_DATA: u32 = 234;
const ERROR_HANDLE_EOF: u32 = 38;

fn long(p: &str) -> String {
    if p.starts_with(r"\\") {
        p.to_string()
    } else {
        format!(r"\\?\{}", p)
    }
}

fn from_w(b: &[u16]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn i64_at(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Open a file or folder without following a reparse point. `access` is attributes-only or
/// list-directory: nothing here can read or change file contents.
fn open(path: &str, access: u32) -> Result<Handle, u32> {
    let p = wide(&long(path));
    // SAFETY: `p` is NUL-terminated and outlives the call.
    let h = unsafe { CreateFileW(p.as_ptr(), access, SHARE_ALL, null(), OPEN_EXISTING, BACKUP_SEMANTICS | OPEN_REPARSE_POINT, null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

fn info(h: HANDLE, class: i32, buf: &mut [u8]) -> bool {
    // SAFETY: `buf` is valid for its length for the call's duration.
    unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) != 0 }
}

fn ioctl(h: HANDLE, code: u32, input: &[u8], out: &mut [u8]) -> Result<u32, u32> {
    let mut ret = 0u32;
    // SAFETY: both buffers are valid for their lengths; no OVERLAPPED (synchronous handle).
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            if input.is_empty() { null() } else { input.as_ptr() as *const c_void },
            input.len() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut ret,
            null_mut(),
        )
    };
    if ok != 0 {
        Ok(ret)
    } else {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    }
}

// ---------------------------------------------------------------- volumes

/// Everything readable about one volume without admin.
#[derive(Debug, Clone, Default)]
pub struct VolumeFacts {
    /// `\\?\Volume{...}\`
    pub guid: String,
    /// Every path it is mounted at (`D:\`, `C:\mnt\x\`).
    pub paths: Vec<String>,
    /// `GetDriveTypeW`: 2 removable, 3 fixed, 4 remote, 5 CD, 6 RAM disk.
    pub drive_type: u32,
    pub label: String,
    pub fs: String,
    pub fs_flags: u32,
    pub total: u64,
    /// Available to this user (quotas apply).
    pub avail: u64,
    pub free: u64,
    pub cluster: u64,
    /// `None` when the persistent-state FSCTL failed.
    pub dev_volume: Option<bool>,
    pub trusted: Option<bool>,
    pub reserved: Option<u64>,
    pub serial64: Option<u64>,
    pub fs_version: Option<(u32, u32)>,
    pub bus_type: Option<u32>,
    /// A rotating disk (seek penalty); `None` = unknown.
    pub seek_penalty: Option<bool>,
}

fn path_names(guid: &str) -> Vec<String> {
    let g = wide(guid);
    let mut buf = vec![0u16; 4096];
    let mut len = 0u32;
    // SAFETY: `buf` holds 4096 units; `len` is a valid out pointer.
    if unsafe { GetVolumePathNamesForVolumeNameW(g.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut len) } == 0 {
        return Vec::new();
    }
    buf[..(len as usize).min(buf.len())].split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// (available to caller, total, total free) for the volume holding `path`.
pub fn free_space(path: &str) -> Option<(u64, u64, u64)> {
    let p = wide(path);
    let (mut a, mut t, mut f) = (0u64, 0u64, 0u64);
    // SAFETY: valid out pointers.
    (unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &mut a, &mut t, &mut f) } != 0).then_some((a, t, f))
}

fn cluster_size(root: &str) -> u64 {
    let p = wide(root);
    let (mut spc, mut bps, mut fc, mut tc) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: valid out pointers.
    unsafe { GetDiskFreeSpaceW(p.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) };
    (spc as u64 * bps as u64).max(512)
}

/// The facts for one volume, read through `root` (a GUID path or a mount path).
pub fn volume_facts(guid: &str, paths: Vec<String>) -> VolumeFacts {
    let root = paths.first().cloned().unwrap_or_else(|| guid.to_string());
    let wroot = wide(&root);
    let mut v = VolumeFacts { guid: guid.to_string(), paths, ..Default::default() };
    // SAFETY: NUL-terminated path.
    v.drive_type = unsafe { GetDriveTypeW(wroot.as_ptr()) };
    if v.drive_type == 4 || v.drive_type == 5 {
        return v; // network or optical: don't touch (it may block or spin up)
    }
    let (mut name, mut fsname) = (vec![0u16; 261], vec![0u16; 261]);
    let (mut serial, mut maxc, mut flags) = (0u32, 0u32, 0u32);
    // SAFETY: both buffers hold 261 units.
    if unsafe { GetVolumeInformationW(wroot.as_ptr(), name.as_mut_ptr(), 261, &mut serial, &mut maxc, &mut flags, fsname.as_mut_ptr(), 261) } != 0 {
        v.label = from_w(&name);
        v.fs = from_w(&fsname);
        v.fs_flags = flags;
    }
    if let Some((a, t, f)) = free_space(&root) {
        (v.avail, v.total, v.free) = (a, t, f);
    }
    v.cluster = cluster_size(&root);
    if let Ok(h) = open(&root, FILE_READ_ATTRIBUTES) {
        let mut out = vec![0u8; 512];
        let mut inb = [0u8; 16];
        inb[4..8].copy_from_slice(&(STATE_DEV_VOLUME | STATE_TRUSTED_VOLUME).to_le_bytes());
        inb[8..12].copy_from_slice(&1u32.to_le_bytes());
        if ioctl(h.0, FSCTL_QUERY_PERSISTENT_VOLUME_STATE, &inb, &mut out[..16]).is_ok() {
            let f = u32_at(&out, 0);
            v.dev_volume = Some(f & STATE_DEV_VOLUME != 0);
            v.trusted = Some(f & STATE_TRUSTED_VOLUME != 0);
        }
        if v.fs.eq_ignore_ascii_case("ReFS") {
            if ioctl(h.0, FSCTL_GET_REFS_VOLUME_DATA, &[], &mut out).is_ok() {
                let bpc = u32_at(&out, 60) as u64;
                v.fs_version = Some((u32_at(&out, 4), u32_at(&out, 8)));
                v.serial64 = Some(u64_at(&out, 16));
                v.reserved = Some(u64_at(&out, 48) * bpc);
            }
        } else if v.fs.eq_ignore_ascii_case("NTFS") && ioctl(h.0, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut out).is_ok() {
            let bpc = u32_at(&out, 44) as u64;
            v.serial64 = Some(u64_at(&out, 0));
            v.reserved = Some(u64_at(&out, 32) * bpc);
        }
    }
    (v.bus_type, v.seek_penalty) = storage_props(&v);
    v
}

/// The storage bus behind a volume and whether it seeks (`IOCTL_STORAGE_QUERY_PROPERTY` on
/// the volume device, opened with no access rights, which needs no admin).
fn storage_props(v: &VolumeFacts) -> (Option<u32>, Option<bool>) {
    let dev = v.guid.trim_end_matches('\\').to_string();
    let w = wide(&dev);
    // SAFETY: NUL-terminated path; zero access only queries the device.
    let h = unsafe { CreateFileW(w.as_ptr(), 0, SHARE_ALL, null(), OPEN_EXISTING, 0, null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return (None, None);
    }
    let h = Handle(h);
    let mut out = vec![0u8; 1024];
    // StorageDeviceProperty (0), PropertyStandardQuery
    let bus = ioctl(h.0, IOCTL_STORAGE_QUERY_PROPERTY, &[0u8; 12], &mut out).ok().filter(|n| *n >= 32).map(|_| u32_at(&out, 28));
    // StorageDeviceSeekPenaltyProperty (7)
    let mut q = [0u8; 12];
    q[0] = 7;
    let seek = ioctl(h.0, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out[..12]).ok().filter(|n| *n >= 9).map(|_| out[8] != 0);
    (bus, seek)
}

/// Every volume on this PC, once each, however many paths reach it.
pub fn volumes() -> Vec<VolumeFacts> {
    let mut out = Vec::new();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `buf` holds 1024 units.
    let h = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if h == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        let guid = from_w(&buf);
        let paths = path_names(&guid);
        out.push(volume_facts(&guid, paths));
        // SAFETY: `h` is the open search handle; `buf` holds 1024 units.
        if unsafe { FindNextVolumeW(h, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: closes the search handle opened above.
    unsafe { FindVolumeClose(h) };
    out
}

/// The volume mount path (`D:\`) and GUID path holding `path`.
pub fn volume_of(path: &str) -> Option<(String, String)> {
    let p = wide(path);
    let mut root = vec![0u16; 1024];
    // SAFETY: `root` holds 1024 units.
    if unsafe { GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let root_s = from_w(&root);
    let wr = wide(&root_s);
    let mut guid = vec![0u16; 100];
    // SAFETY: `guid` holds 100 units.
    if unsafe { GetVolumeNameForVolumeMountPointW(wr.as_ptr(), guid.as_mut_ptr(), guid.len() as u32) } == 0 {
        return Some((root_s.clone(), root_s)); // e.g. a network share: its own key
    }
    Some((root_s, from_w(&guid)))
}

/// A folder's identity: (64-bit volume serial, 128-bit file id, attributes).
pub fn identity(path: &str) -> Option<(u64, u128, u32)> {
    let h = open(path, FILE_READ_ATTRIBUTES).ok()?;
    let mut id = [0u8; 24];
    let mut basic = [0u8; 40];
    if !info(h.0, FILE_ID_INFO_CLASS, &mut id) || !info(h.0, FILE_BASIC_INFO_CLASS, &mut basic) {
        return None;
    }
    Some((u64_at(&id, 0), u128::from_le_bytes(id[8..24].try_into().unwrap()), u32_at(&basic, 32)))
}

/// The current user's SID as a string (`S-1-5-21-...`).
pub fn current_user_sid() -> Result<String, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut tok: HANDLE = null_mut();
    // SAFETY: valid out pointer; the pseudo handle needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
        return Err(format!("couldn't read this user's token ({})", std::io::Error::last_os_error()));
    }
    let tok = Handle(tok);
    // u64 elements keep TOKEN_USER aligned.
    let mut buf = vec![0u64; 64];
    let mut ret = 0u32;
    // SAFETY: `buf` is valid for its length in bytes.
    if unsafe { GetTokenInformation(tok.0, TokenUser, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret) } == 0 {
        return Err(format!("couldn't read this user's SID ({})", std::io::Error::last_os_error()));
    }
    // SAFETY: the call filled a TOKEN_USER at the start of `buf`, whose SID points into `buf`.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut s: *mut u16 = null_mut();
    // SAFETY: `sid` is valid while `buf` lives; `s` receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 || s.is_null() {
        return Err(format!("couldn't write this user's SID ({})", std::io::Error::last_os_error()));
    }
    let mut n = 0;
    // SAFETY: `s` is NUL-terminated.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `s` holds `n` units; freed once, after copying.
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) });
    unsafe { LocalFree(s as *mut c_void) };
    Ok(out)
}

/// Make the folder `path` (its parent must exist) with an ACL of its own: full control for
/// this user, SYSTEM and Administrators, nothing inherited from the drive's root, so other
/// users of the PC can't list or open what is moved into it. Fails if it already exists.
pub fn create_private_dir(path: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let sid = current_user_sid()?;
    // O: owner this user; D:P protected (inheritance off); OICI: folders and files inside too.
    let sddl = wide(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"));
    let mut sd: *mut c_void = null_mut();
    // SAFETY: `sddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
        return Err(format!("couldn't build the folder's permissions ({})", std::io::Error::last_os_error()));
    }
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let p = wide(path);
    // SAFETY: `p` is NUL-terminated; `sa` and the descriptor outlive the call.
    let ok = unsafe { CreateDirectoryW(p.as_ptr(), &sa) } != 0;
    let err = std::io::Error::last_os_error();
    // SAFETY: freed once, after the call.
    unsafe { LocalFree(sd) };
    if ok {
        Ok(())
    } else {
        Err(format!("couldn't make {} ({})", path, err))
    }
}

/// How many names (hard links) a file has, read without following a reparse point.
pub fn link_count(path: &str) -> Option<u32> {
    let h = open(path, FILE_READ_ATTRIBUTES).ok()?;
    let mut std_info = [0u8; 24];
    info(h.0, FILE_STANDARD_INFO_CLASS, &mut std_info).then(|| u32_at(&std_info, 16))
}

/// Volumes holding a page file, from the registry (read-only, no admin): drive letters.
pub fn pagefile_drives() -> Vec<char> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let key = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(mm) = key.open_subkey(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management") else {
        return Vec::new();
    };
    let list: Vec<String> = mm.get_value("PagingFiles").unwrap_or_default();
    let mut out = Vec::new();
    for entry in list {
        let e = entry.trim();
        let c = if e.starts_with("?:") {
            std::env::var("SystemDrive").ok().and_then(|s| s.chars().next())
        } else {
            e.chars().next()
        };
        if let Some(c) = c.filter(|c| c.is_ascii_alphabetic()) {
            let c = c.to_ascii_uppercase();
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Lower this process's CPU, I/O and memory priority (`PROCESS_MODE_BACKGROUND_BEGIN`), for
/// the scanner child.
pub fn enter_background_mode() -> bool {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass};
    const PROCESS_MODE_BACKGROUND_BEGIN: u32 = 0x0010_0000;
    // SAFETY: the pseudo handle of this process.
    unsafe { SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN) != 0 }
}

/// Tell running programs that the user environment changed (`WM_SETTINGCHANGE` with
/// "Environment", as the System control panel does), so Explorer starts new programs with the
/// new variables. Waits at most 5 s for windows that don't answer. Writes nothing.
pub fn broadcast_environment_change() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};
    let what = wide("Environment");
    let mut result = 0usize;
    // SAFETY: `what` is NUL-terminated and outlives the call (SendMessageTimeout is synchronous);
    // `result` is a valid out pointer.
    unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr() as isize, SMTO_ABORTIFHUNG, 5000, &mut result) };
}

/// Make this process's stdin, stdout and stderr not inheritable, so a detached child (the
/// measured watcher) doesn't keep the caller's pipes open: whoever reads our output (a script,
/// the tray, an MCP client) would otherwise wait for the watcher to exit, up to 30 minutes.
/// Children that ask to inherit them still get them (std duplicates them for that).
pub fn stop_std_handle_inheritance() {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain calls on this process's own standard handles; an invalid or null handle
        // makes SetHandleInformation fail, which is ignored.
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// A FILETIME as local "YYYY-MM-DD HH:MM", or "" for 0.
pub fn filetime_local(ft: u64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if ft == 0 {
        return String::new();
    }
    let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    // SAFETY: plain structs; valid out pointers.
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) == 0 {
            return String::new();
        }
        format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute)
    }
}

// ---------------------------------------------------------------- Defender and Dev Drive policy

/// The Dev Drive policy and Defender facts. `None` = could not be read.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DefenderFacts {
    /// `Get-MpPreference`.PerformanceModeStatus: 0 enabled, 1 disabled (this PC's enum).
    pub performance_mode_status: Option<u32>,
    pub realtime_enabled: Option<bool>,
    /// `Get-MpComputerStatus`.AMRunningMode is "Normal" (1), passive or other (0).
    pub defender_active: Option<bool>,
    /// Antivirus products registered with Security Center that aren't Defender.
    pub third_party_av: Option<u32>,
}

/// One `powershell -NoProfile` child, numbers only, 10 s timeout.
pub fn probe_defender() -> DefenderFacts {
    const SCRIPT: &str = "$ErrorActionPreference='SilentlyContinue'; \
$p=Get-MpPreference; if($p -ne $null){'pm='+[int]$p.PerformanceModeStatus}; \
$s=Get-MpComputerStatus; if($s -ne $null){'rt='+[int][bool]$s.RealTimeProtectionEnabled; 'am='+[int]($s.AMRunningMode -eq 'Normal')}; \
$a=Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntivirusProduct; \
if($a -ne $null){'av='+@($a | Where-Object { $_.displayName -notmatch 'Defender' }).Count}";
    let out = run_hidden("powershell.exe", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT], Duration::from_secs(10));
    crate::disk::parse_defender(&out.unwrap_or_default())
}

/// `fsutil devdrv query` (no volume argument works without admin). Its output is prose.
pub fn fsutil_devdrv_query() -> Option<String> {
    run_hidden("fsutil.exe", &["devdrv", "query"], Duration::from_secs(10))
}

/// Run a console program with no window and return its stdout, or `None` on failure or timeout.
fn run_hidden(exe: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let bytes = reader.join().ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------- scanner

/// Extents of the open file, appended to `shared` (count above 1 or unknown); clusters with
/// count 1 are added to `exclusive`. Returns (counts were read, error code or 0).
fn extents(h: HANDLE, buf: &mut [u8], want_rc: bool, out: &mut Vec<Ext>) -> (bool, u32) {
    let mut start: i64 = 0;
    let mut use_rc = want_rc;
    loop {
        let (code, esz) = if use_rc { (FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT, 24usize) } else { (FSCTL_GET_RETRIEVAL_POINTERS, 16usize) };
        let (ok, err) = match ioctl(h, code, &start.to_le_bytes(), buf) {
            Ok(_) => (true, 0),
            Err(e) => (false, e),
        };
        if !ok && err != ERROR_MORE_DATA {
            if use_rc && start == 0 && (err == 1 || err == 50 || err == 87) {
                use_rc = false;
                continue;
            }
            return (use_rc, if err == ERROR_HANDLE_EOF { 0 } else { err });
        }
        let count = u32_at(buf, 0) as usize;
        let mut prev = i64_at(buf, 8);
        for i in 0..count {
            let o = 16 + i * esz;
            if o + esz > buf.len() {
                break;
            }
            let next = i64_at(buf, o);
            let lcn = i64_at(buf, o + 8);
            let rc = if use_rc { u32_at(buf, o + 16) } else { 0 };
            if lcn >= 0 && next > prev {
                out.push(Ext { lcn: lcn as u64, len: (next - prev) as u64, rc });
            }
            prev = next;
        }
        if err == ERROR_MORE_DATA && count > 0 {
            start = prev;
            continue;
        }
        return (use_rc, 0);
    }
}

/// What one walk of one item found.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WalkStats {
    pub files: u64,
    pub dirs: u64,
    pub logical: u64,
    pub allocated: u64,
    /// Newest last-write time of any file or folder, as FILETIME.
    pub last_write: u64,
    pub reparse_dirs: u64,
    pub reparse_files: u64,
    pub sparse_files: u64,
    pub compressed_files: u64,
    pub cloud_files: u64,
    /// Names of files seen before under another name (hard links), counted once.
    pub hard_links: u64,
    pub dir_failures: u64,
    pub open_failures: u64,
    pub extent_failures: u64,
    pub files_without_refcount: u64,
    /// Files and folders written after the scan started.
    pub written_during_scan: u64,
    /// The walk stopped before every folder was read.
    pub cut_short: bool,
}

impl WalkStats {
    fn add(&mut self, o: &WalkStats) {
        macro_rules! s { ($($f:ident),*) => { $( self.$f += o.$f; )* } }
        s!(files, dirs, logical, allocated, reparse_dirs, reparse_files, sparse_files, compressed_files, cloud_files, hard_links,
           dir_failures, open_failures, extent_failures, files_without_refcount, written_during_scan);
        self.last_write = self.last_write.max(o.last_write);
        self.cut_short |= o.cut_short;
    }
}

/// A file with more than one name: its extents are attributed once all names are known.
#[derive(Debug, Clone, Default)]
pub struct MultiLink {
    pub links: u32,
    pub first_item: usize,
    /// Names found per item.
    pub names: HashMap<usize, u32>,
    pub alloc_clusters: u64,
    pub exts: Vec<Ext>,
}

/// Shared between walks of one volume.
pub struct VolumeScan {
    pub cluster: u64,
    pub block_cloning: bool,
    pub level2: bool,
    pub threads: usize,
    pub deadline: Instant,
    pub stop: Arc<AtomicBool>,
    pub started_ft: u64,
    pub multi: Mutex<HashMap<u128, MultiLink>>,
    pub progress_files: Arc<AtomicU64>,
}

struct Queue {
    jobs: Mutex<Vec<String>>,
    cv: Condvar,
    pending: AtomicUsize,
}

struct Local {
    stats: WalkStats,
    ext: ItemExtents,
}

/// Walk one item's roots. `skip` holds normalised folders that belong to deeper items.
pub fn walk_item(vs: &VolumeScan, item: usize, roots: &[String], skip: &[String]) -> (WalkStats, ItemExtents) {
    let q = Arc::new(Queue { jobs: Mutex::new(roots.to_vec()), cv: Condvar::new(), pending: AtomicUsize::new(roots.len()) });
    let mut stats = WalkStats::default();
    let mut ext = ItemExtents::default();
    std::thread::scope(|s| {
        let mut hs = Vec::new();
        for _ in 0..vs.threads.max(1) {
            let q = q.clone();
            hs.push(s.spawn(move || {
                let mut lo = Local { stats: WalkStats::default(), ext: ItemExtents::default() };
                let mut dbuf = vec![0u8; 1 << 17];
                let mut ebuf = vec![0u8; 1 << 16];
                loop {
                    let job = {
                        let mut g = q.jobs.lock().unwrap();
                        loop {
                            if let Some(j) = g.pop() {
                                break Some(j);
                            }
                            if q.pending.load(Ordering::SeqCst) == 0 {
                                break None;
                            }
                            g = q.cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
                        }
                    };
                    let Some(dir) = job else { break };
                    if vs.stop.load(Ordering::Relaxed) || Instant::now() > vs.deadline {
                        lo.stats.cut_short = true;
                    } else {
                        let subdirs = scan_dir(vs, item, &dir, skip, &mut lo, &mut dbuf, &mut ebuf);
                        if !subdirs.is_empty() {
                            q.pending.fetch_add(subdirs.len(), Ordering::SeqCst);
                            q.jobs.lock().unwrap().extend(subdirs);
                            q.cv.notify_all();
                        }
                    }
                    q.pending.fetch_sub(1, Ordering::SeqCst);
                    q.cv.notify_all();
                }
                lo
            }));
        }
        for h in hs {
            let lo = h.join().unwrap();
            stats.add(&lo.stats);
            ext.exclusive_clusters += lo.ext.exclusive_clusters;
            ext.shared.extend(lo.ext.shared);
        }
    });
    (stats, ext)
}

fn scan_dir(vs: &VolumeScan, item: usize, dir: &str, skip: &[String], lo: &mut Local, dbuf: &mut [u8], ebuf: &mut [u8]) -> Vec<String> {
    let st = &mut lo.stats;
    let h = match open(dir, FILE_LIST_DIRECTORY | SYNCHRONIZE) {
        Ok(h) => h,
        Err(_) => {
            st.dir_failures += 1;
            return Vec::new();
        }
    };
    st.dirs += 1;
    let mut subdirs = Vec::new();
    let mut exts: Vec<Ext> = Vec::new();
    loop {
        if !info(h.0, FILE_ID_EXTD_DIRECTORY_INFO_CLASS, dbuf) {
            break;
        }
        let mut o = 0usize;
        loop {
            let b = &dbuf[o..];
            let next = u32_at(b, 0) as usize;
            let last_write = u64_at(b, 24);
            let eof = u64_at(b, 40);
            let alloc = u64_at(b, 48);
            let attrs = u32_at(b, 56);
            let nl = u32_at(b, 60) as usize;
            let fid = u128::from_le_bytes(b[72..88].try_into().unwrap());
            let name_u16: Vec<u16> = b[88..88 + nl].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let is_dot = name_u16 == [b'.' as u16] || name_u16 == [b'.' as u16, b'.' as u16];
            if !is_dot {
                let name = String::from_utf16_lossy(&name_u16);
                let full = format!("{}\\{}", dir.trim_end_matches('\\'), name);
                if last_write >= vs.started_ft {
                    st.written_during_scan += 1;
                }
                st.last_write = st.last_write.max(last_write);
                if attrs & A_DIR != 0 {
                    if attrs & A_REPARSE != 0 {
                        st.reparse_dirs += 1; // a junction or mount folder: never followed
                    } else {
                        let n = crate::disk_space::norm(&full);
                        if !skip.iter().any(|s| *s == n) {
                            subdirs.push(full);
                        }
                    }
                } else {
                    count_file(vs, item, &full, eof, alloc, attrs, fid, st, &mut lo.ext, &mut exts, ebuf);
                }
            }
            if next == 0 {
                break;
            }
            o += next;
        }
    }
    subdirs
}

#[allow(clippy::too_many_arguments)]
fn count_file(
    vs: &VolumeScan,
    item: usize,
    full: &str,
    eof: u64,
    alloc: u64,
    attrs: u32,
    fid: u128,
    st: &mut WalkStats,
    ext: &mut ItemExtents,
    exts: &mut Vec<Ext>,
    ebuf: &mut [u8],
) {
    vs.progress_files.fetch_add(1, Ordering::Relaxed);
    let flags = |s: &mut WalkStats| {
        if attrs & A_REPARSE != 0 {
            s.reparse_files += 1;
        }
        if attrs & A_SPARSE != 0 {
            s.sparse_files += 1;
        }
        if attrs & A_COMPRESSED != 0 {
            s.compressed_files += 1;
        }
        if attrs & (A_OFFLINE | A_RECALL_OPEN | A_RECALL_DATA) != 0 {
            s.cloud_files += 1;
        }
    };
    let alloc_clusters = alloc.div_ceil(vs.cluster);
    if !vs.level2 {
        st.files += 1;
        st.logical += eof;
        st.allocated += alloc;
        flags(st);
        return;
    }
    let fh = match open(full, FILE_READ_ATTRIBUTES) {
        Ok(h) => h,
        Err(_) => {
            st.open_failures += 1;
            st.files += 1;
            st.logical += eof;
            st.allocated += alloc;
            flags(st);
            return;
        }
    };
    let mut std_info = [0u8; 24];
    let links = if info(fh.0, FILE_STANDARD_INFO_CLASS, &mut std_info) { u32_at(&std_info, 16).max(1) } else { 1 };
    if links > 1 {
        {
            let mut m = vs.multi.lock().unwrap();
            if let Some(e) = m.get_mut(&fid) {
                *e.names.entry(item).or_default() += 1;
                st.hard_links += 1;
                return; // counted once, where first seen
            }
            // Claim the id before reading, so another thread meeting a second name counts it.
            let mut ml = MultiLink { links, first_item: item, alloc_clusters, ..Default::default() };
            ml.names.insert(item, 1);
            m.insert(fid, ml);
        }
        exts.clear();
        if vs.block_cloning && alloc > 0 {
            let (rc_ok, err) = extents(fh.0, ebuf, true, exts);
            if err != 0 {
                st.extent_failures += 1;
            }
            if !rc_ok {
                st.files_without_refcount += 1;
            }
        }
        if let Some(e) = vs.multi.lock().unwrap().get_mut(&fid) {
            e.exts = exts.clone();
        }
    } else if alloc > 0 {
        if vs.block_cloning {
            exts.clear();
            let (rc_ok, err) = extents(fh.0, ebuf, true, exts);
            if err != 0 {
                st.extent_failures += 1;
            }
            if !rc_ok {
                st.files_without_refcount += 1;
            }
            for e in exts.iter() {
                if e.rc == 1 {
                    ext.exclusive_clusters += e.len;
                } else {
                    ext.shared.push(*e);
                }
            }
        } else {
            // No block cloning: a file with one name owns its clusters.
            ext.exclusive_clusters += alloc_clusters;
        }
    }
    st.files += 1;
    st.logical += eof;
    st.allocated += alloc;
    flags(st);
}

/// Attribute every multi-link file's extents now that all its names are known: each item
/// with `k` names references them `k` times, and the count becomes `rc - 1 + links`, so the
/// file frees only when every name is gone (proposal 3.1, rule 3). Without block cloning the
/// file gets a synthetic run of its own beyond any real cluster number.
pub fn attribute_multilinks(multi: HashMap<u128, MultiLink>, items: &mut [ItemExtents], block_cloning: bool) {
    let mut synthetic = 1u64 << 62;
    for (_, ml) in multi {
        let runs: Vec<Ext> = if block_cloning {
            ml.exts.iter().map(|e| Ext { lcn: e.lcn, len: e.len, rc: if e.rc == 0 { 0 } else { e.rc - 1 + ml.links } }).collect()
        } else if ml.alloc_clusters > 0 {
            let e = Ext { lcn: synthetic, len: ml.alloc_clusters, rc: ml.links };
            synthetic += ml.alloc_clusters;
            vec![e]
        } else {
            Vec::new()
        };
        for (item, k) in &ml.names {
            if let Some(it) = items.get_mut(*item) {
                for _ in 0..*k {
                    it.shared.extend(runs.iter().copied());
                }
            }
        }
    }
}

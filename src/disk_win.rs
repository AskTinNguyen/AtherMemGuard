//! The Win32 side of the disk features: volumes and their free space, the Dev Drive and ReFS
//! facts readable on a root handle, the Defender probe, and the low-priority scanner that
//! reads directory records and extent reference counts. Everything here is read-only: files
//! and folders are opened with `FILE_READ_ATTRIBUTES` (or `FILE_LIST_DIRECTORY`) only, reparse
//! points are never followed, and nothing is written. Each `unsafe` block is small and only
//! passes buffers it owns to the API it documents. The record layouts follow the prototype
//! `spaceprobe`, which was cross-checked against a full count of a ReFS volume.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError, HANDLE, HMODULE, INVALID_HANDLE_VALUE, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, FileDispositionInfoEx, FileFullDirectoryInfo, FindFirstVolumeW, FindNextVolumeW,
    FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetFileAttributesW,
    GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW, RemoveDirectoryW, SetFileAttributesW, SetFileInformationByHandle, DELETE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, INVALID_FILE_ATTRIBUTES,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_MULTI_SZ, REG_SZ,
};

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
const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;

const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
const FILE_OPEN: u32 = 1;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x0000_4000;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *const UNICODE_STRING,
    attributes: u32,
    security_descriptor: *const c_void,
    security_quality_of_service: *const c_void,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const ObjectAttributes,
        io_status_block: *mut IO_STATUS_BLOCK,
        allocation_size: *const i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
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

const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_MORE_DATA: u32 = 234;
const ERROR_HANDLE_EOF: u32 = 38;

fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for ch in s.chars() {
        let c = if ch == '/' { '\\' } else { ch };
        if c == '\\' {
            if !prev_sep {
                out.push('\\');
            }
            prev_sep = true;
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    out
}

fn long(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let s = p.replace('/', "\\");
    if s.get(..8).is_some_and(|h| h.eq_ignore_ascii_case(r"\\?\UNC\")) {
        return format!(r"\\?\UNC\{}", collapse_slashes(&s[8..]).trim_start_matches('\\'));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return format!(r"\\?\{}", collapse_slashes(rest).trim_start_matches('\\'));
    }
    if let Some(rest) = s.strip_prefix(r"\\.\") {
        return format!(r"\\.\{}", collapse_slashes(rest).trim_start_matches('\\'));
    }
    let abs = std::path::absolute(std::path::Path::new(&s))
        .map(|pb| pb.to_string_lossy().into_owned())
        .unwrap_or(s);
    if let Some(unc_rest) = abs.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{}", collapse_slashes(unc_rest).trim_start_matches('\\'))
    } else {
        format!(r"\\?\{}", collapse_slashes(&abs))
    }
}

pub(crate) fn long_path(p: &Path) -> PathBuf {
    PathBuf::from(long(&p.display().to_string()))
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

#[inline]
fn as_bytes_mut<const N: usize>(buf: &mut [u64; N]) -> &mut [u8] {
    // SAFETY: `[u64; N]` is a contiguous initialized array of `N * 8` bytes with no padding.
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, N * 8) }
}

/// Open an entry relative to an open directory handle `dir_h` directly from its UTF-16 slice
/// (no NUL terminator, no `String`, no `long()`, no kernel prefix re-parse, never follows reparse points).
/// `non_dir`: `Some(true)` forces non-directory, `Some(false)` forces directory, `None` allows either
/// (needed for reparse points such as file symlinks or directory junctions).
fn open_relative_opts(dir_h: HANDLE, name_w: &[u16], access: u32, non_dir: Option<bool>) -> Result<Handle, u32> {
    if name_w.is_empty() {
        return Err(87);
    }
    let Some(byte_len) = name_w.len().checked_mul(2).and_then(|n| u16::try_from(n).ok()) else {
        return Err(87);
    };
    let uname = UNICODE_STRING { Length: byte_len, MaximumLength: byte_len, Buffer: name_w.as_ptr() as *mut u16 };
    let oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: dir_h,
        object_name: &uname,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: null(),
        security_quality_of_service: null(),
    };
    let mut iosb = IO_STATUS_BLOCK { Anonymous: windows_sys::Win32::System::IO::IO_STATUS_BLOCK_0 { Status: 0 }, Information: 0 };
    let mut fh: HANDLE = null_mut();
    let type_flag = match non_dir {
        Some(true) => FILE_NON_DIRECTORY_FILE,
        Some(false) => FILE_DIRECTORY_FILE,
        None => 0,
    };
    // SAFETY: `uname.Buffer` is valid for `byte_len` bytes during this call; `oa`, `iosb`, and `fh`
    // are valid stack locations; `dir_h` is an open directory handle.
    let status = unsafe {
        NtCreateFile(
            &mut fh,
            access | SYNCHRONIZE,
            &oa,
            &mut iosb,
            null(),
            0,
            SHARE_ALL,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_FOR_BACKUP_INTENT | FILE_OPEN_REPARSE_POINT | type_flag,
            null(),
            0,
        )
    };
    if status >= 0 && !fh.is_null() && fh != INVALID_HANDLE_VALUE {
        Ok(Handle(fh))
    } else {
        // SAFETY: plain NTSTATUS to Win32 error conversion.
        Err(unsafe { RtlNtStatusToDosError(status) })
    }
}

/// Open a file relative to an open directory handle `dir_h` directly from its UTF-16 slice.
fn open_relative(dir_h: HANDLE, name_w: &[u16], access: u32) -> Result<Handle, u32> {
    open_relative_opts(dir_h, name_w, access, Some(true))
}

/// Open an already-`long()`-normalised directory path stored in `dir_w` (without trailing NUL).
fn open_dir_w(dir_w: &mut Vec<u16>, access: u32) -> Result<Handle, u32> {
    dir_w.push(0);
    // SAFETY: `dir_w` is NUL-terminated for this call.
    let h = unsafe { CreateFileW(dir_w.as_ptr(), access, SHARE_ALL, null(), OPEN_EXISTING, BACKUP_SEMANTICS | OPEN_REPARSE_POINT, null_mut()) };
    dir_w.pop();
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

/// Open a file or folder without following a reparse point. `access` is attributes-only,
/// list-directory or read-the-owner: nothing here can read or change file contents.
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

fn info_res(h: HANDLE, class: i32, buf: &mut [u8]) -> Result<(), u32> {
    if info(h, class, buf) {
        Ok(())
    } else {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    }
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
    /// VHD / VHDX backing file path on the host volume (via `GetStorageDependencyInformation`).
    pub vhd_backed_by: Option<String>,
    /// Backing VHD/VHDX file size on the host volume in bytes.
    pub vhd_file_bytes: Option<u64>,
    /// Free bytes available on the VHD/VHDX's host volume.
    pub vhd_host_free: Option<u64>,
    /// Maximum virtual size of the VHD/VHDX in bytes.
    pub vhd_max_bytes: Option<u64>,
    /// Attached filesystem minifilter names (on Dev Drives, when accessible).
    pub attached_filters: Option<Vec<String>>,
}

/// ReFS volume space counters returned by `FSCTL_GET_REFS_VOLUME_DATA`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefsSpaceData {
    pub bytes_per_cluster: u64,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub reserved_bytes: u64,
}

/// Query `FSCTL_GET_REFS_VOLUME_DATA` on `root` (e.g. `D:\`). Returns `None` on non-ReFS volumes
/// or if the FSCTL fails.
pub fn refs_volume_data(root: &str) -> Option<RefsSpaceData> {
    let h = open(root, FILE_READ_ATTRIBUTES).ok()?;
    let mut out_aligned = [0u64; 64]; // 512 bytes, 8-byte aligned (>= 152-byte REFS_VOLUME_DATA_BUFFER)
    let out = as_bytes_mut(&mut out_aligned);
    let ret = ioctl(h.0, FSCTL_GET_REFS_VOLUME_DATA, &[], out).ok()?;
    if (ret as usize) < 64 {
        return None;
    }
    let bpc = u32_at(out, 60) as u64;
    if bpc == 0 {
        return None;
    }
    let total_clusters = i64_at(out, 32).max(0) as u64;
    let free_clusters = i64_at(out, 40).max(0) as u64;
    let reserved_clusters = i64_at(out, 48).max(0) as u64;
    Some(RefsSpaceData {
        bytes_per_cluster: bpc,
        total_bytes: total_clusters.saturating_mul(bpc),
        free_bytes: free_clusters.saturating_mul(bpc),
        reserved_bytes: reserved_clusters.saturating_mul(bpc),
    })
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

fn cluster_size(root: &str) -> Option<u64> {
    let p = wide(root);
    let (mut spc, mut bps, mut fc, mut tc) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: valid out pointers.
    let ok = unsafe { GetDiskFreeSpaceW(p.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) };
    if ok == 0 || spc == 0 || bps == 0 {
        return None;
    }
    Some(spc as u64 * bps as u64)
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
    v.cluster = cluster_size(&root).unwrap_or(0);
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
                if v.cluster == 0 && bpc > 0 {
                    v.cluster = bpc;
                }
                v.fs_version = Some((u32_at(&out, 4), u32_at(&out, 8)));
                v.serial64 = Some(u64_at(&out, 16));
                v.reserved = Some(u64_at(&out, 48) * bpc);
            }
        } else if v.fs.eq_ignore_ascii_case("NTFS") && ioctl(h.0, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut out).is_ok() {
            let bpc = u32_at(&out, 44) as u64;
            if v.cluster == 0 && bpc > 0 {
                v.cluster = bpc;
            }
            v.serial64 = Some(u64_at(&out, 0));
            v.reserved = Some(u64_at(&out, 32) * bpc);
        }
    }
    (v.bus_type, v.seek_penalty) = storage_props(&v);
    if v.bus_type == Some(BUS_FILE_BACKED_VIRTUAL) {
        if let Some((backed_by, file_bytes, host_free, max_bytes)) = probe_vhd_backing(&v) {
            v.vhd_backed_by = Some(backed_by);
            v.vhd_file_bytes = file_bytes;
            v.vhd_host_free = host_free;
            v.vhd_max_bytes = max_bytes;
        }
    }
    if v.dev_volume == Some(true) {
        v.attached_filters = volume_attached_filters(&root);
    }
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

/// Physical storage device number for a volume GUID (`\\?\Volume{...}\`), used to group
/// partitions sharing one physical disk while scanning distinct physical drives concurrently.
pub fn volume_disk_number(guid: &str) -> Option<u32> {
    let dev = guid.trim_end_matches('\\');
    let w = wide(dev);
    // SAFETY: NUL-terminated device path; 0 access queries device metadata without admin.
    let h = unsafe { CreateFileW(w.as_ptr(), 0, SHARE_ALL, null(), OPEN_EXISTING, 0, null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = Handle(h);
    let mut out = [0u8; 12];
    let n = ioctl(h.0, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out).ok()?;
    (n >= 8).then(|| u32_at(&out, 4))
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
    let mut id_buf = [0u64; 3];
    let mut basic_buf = [0u64; 5];
    let id = as_bytes_mut(&mut id_buf);
    let basic = as_bytes_mut(&mut basic_buf);
    if !info(h.0, FILE_ID_INFO_CLASS, id) || !info(h.0, FILE_BASIC_INFO_CLASS, basic) {
        return None;
    }
    Some((u64_at(id, 0), u128::from_le_bytes(id[8..24].try_into().unwrap()), u32_at(basic, 32)))
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

/// Is `path` (not followed if it is a link) owned by the current user? `None` when the owner
/// can't be read.
pub fn owned_by_current_user(path: &str) -> Option<bool> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    const READ_CONTROL: u32 = 0x0002_0000;
    let me = current_user_sid().ok()?;
    let h = open(path, READ_CONTROL).ok()?;
    let mut owner: *mut c_void = null_mut();
    let mut sd: *mut c_void = null_mut();
    // SAFETY: `h` is an open handle; `owner` points into `sd`, which the call LocalAllocs.
    if unsafe { GetSecurityInfo(h.0, SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, null_mut(), null_mut(), null_mut(), &mut sd) } != 0 {
        return None;
    }
    let mut s: *mut u16 = null_mut();
    // SAFETY: `owner` is valid while `sd` lives; `s` receives a LocalAlloc'd string.
    let ok = !owner.is_null() && unsafe { ConvertSidToStringSidW(owner, &mut s) } != 0 && !s.is_null();
    let mut out = None;
    if ok {
        let mut n = 0;
        // SAFETY: `s` is NUL-terminated.
        while unsafe { *s.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: `s` holds `n` units; freed once, after copying.
        out = Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) }).eq_ignore_ascii_case(&me));
        unsafe { LocalFree(s as *mut c_void) };
    }
    // SAFETY: freed once.
    unsafe { LocalFree(sd) };
    out
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
    let mut std_buf = [0u64; 3];
    let std_info = as_bytes_mut(&mut std_buf);
    info(h.0, FILE_STANDARD_INFO_CLASS, std_info).then(|| u32_at(std_info, 16))
}

/// Volumes holding a page file, from the registry (read-only, no admin): drive letters.
pub fn pagefile_drives() -> Vec<char> {
    let Ok(mm) = crate::win::RegKey::open_read(
        crate::win::RegHive::Hklm,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management",
    ) else {
        return Vec::new();
    };
    let list: Vec<String> = mm.get_multi_string("PagingFiles").unwrap_or_default();
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

/// Panic-safe Win32 Job Object wrapper with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
/// Assigning a spawned child ensures `terminate()` (or dropping the job) kills the entire
/// process subtree (e.g., `cmd\git.exe` -> `mingw64\bin\git.exe` -> `git-lfs.exe`).
pub struct ProcessJob {
    handle: Handle,
}

impl ProcessJob {
    pub fn new() -> Result<Self, String> {
        use std::mem::{size_of, zeroed};
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: null security attributes and null name create an anonymous job object.
        let h = unsafe { CreateJobObjectW(null(), null()) };
        if h.is_null() {
            return Err(format!("CreateJobObjectW failed: {}", std::io::Error::last_os_error()));
        }
        let job = Self { handle: Handle(h) };
        // SAFETY: zeroed POD struct; only BasicLimitInformation.LimitFlags is set.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `job.handle.0` is valid and `info` has exact size.
        let ok = unsafe {
            SetInformationJobObject(
                job.handle.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(format!("SetInformationJobObject failed: {}", std::io::Error::last_os_error()));
        }
        Ok(job)
    }

    pub fn assign_pid(&self, pid: u32) -> Result<(), String> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
        // SAFETY: opens process handle for job assignment; closed on `Handle` drop.
        let ph = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if ph.is_null() {
            return Err(format!("OpenProcess({}) failed: {}", pid, std::io::Error::last_os_error()));
        }
        let ph = Handle(ph);
        // SAFETY: `self.handle.0` and `ph.0` are valid handles.
        let ok = unsafe { AssignProcessToJobObject(self.handle.0, ph.0) };
        if ok == 0 {
            return Err(format!("AssignProcessToJobObject({}) failed: {}", pid, std::io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `self.handle.0` is a valid job handle.
        unsafe { TerminateJobObject(self.handle.0, 1) };
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

/// Locale-independent Dev Drive & Defender policy read directly from HKLM (no admin, no fsutil prose).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevDrivePolicyReg {
    pub enabled: Option<bool>,
    pub av_filters: &'static str,
    pub attach_policy: Vec<String>,
    pub perf_mode: Option<u32>,
    pub realtime_disabled: Option<bool>,
    pub passive_mode: Option<bool>,
    pub os_build: Option<u32>,
}

struct RegKeyGuard(HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was opened by `RegOpenKeyExW`.
            unsafe { RegCloseKey(self.0) };
        }
    }
}

fn reg_open_hklm_read(subkey: &str) -> Option<RegKeyGuard> {
    let w = wide(subkey);
    let mut hk: HKEY = null_mut();
    // SAFETY: `w` is NUL-terminated; `hk` is a valid out pointer.
    let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, w.as_ptr(), 0, KEY_READ, &mut hk) };
    (rc == 0 && !hk.is_null()).then_some(RegKeyGuard(hk))
}

fn reg_read_dword(hk: HKEY, name: &str) -> Option<u32> {
    let w = wide(name);
    let mut ty = 0u32;
    let mut val = 0u32;
    let mut cb = 4u32;
    // SAFETY: `w` is NUL-terminated; `val` holds 4 bytes (`cb = 4`).
    let rc = unsafe { RegQueryValueExW(hk, w.as_ptr(), null(), &mut ty, &mut val as *mut u32 as *mut u8, &mut cb) };
    (rc == 0 && ty == REG_DWORD && cb >= 4).then_some(val)
}

fn reg_read_sz(hk: HKEY, name: &str) -> Option<String> {
    let w = wide(name);
    let mut ty = 0u32;
    let mut cb = 0u32;
    // SAFETY: size-query call with null buffer.
    let rc = unsafe { RegQueryValueExW(hk, w.as_ptr(), null(), &mut ty, null_mut(), &mut cb) };
    if rc != 0 || ty != REG_SZ || cb == 0 {
        return None;
    }
    let mut buf = vec![0u16; (cb as usize).div_ceil(2) + 1];
    let mut actual = (buf.len() * 2) as u32;
    // SAFETY: `buf` is valid for `actual` bytes.
    let rc = unsafe { RegQueryValueExW(hk, w.as_ptr(), null(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut actual) };
    if rc != 0 || ty != REG_SZ {
        return None;
    }
    let units = (actual as usize / 2).min(buf.len());
    Some(from_w(&buf[..units]))
}

fn reg_read_multi_sz(hk: HKEY, name: &str) -> Option<Vec<String>> {
    let w = wide(name);
    let mut ty = 0u32;
    let mut cb = 0u32;
    // SAFETY: size-query call with null buffer.
    let rc = unsafe { RegQueryValueExW(hk, w.as_ptr(), null(), &mut ty, null_mut(), &mut cb) };
    if rc != 0 || ty != REG_MULTI_SZ || cb == 0 {
        return None;
    }
    let mut buf = vec![0u16; (cb as usize).div_ceil(2) + 2];
    let mut actual = (buf.len() * 2) as u32;
    // SAFETY: `buf` is valid for `actual` bytes.
    let rc = unsafe { RegQueryValueExW(hk, w.as_ptr(), null(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut actual) };
    if rc != 0 || ty != REG_MULTI_SZ {
        return None;
    }
    let units = (actual as usize / 2).min(buf.len());
    let list = buf[..units]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    Some(list)
}

fn reg_hklm_dword(subkey: &str, name: &str) -> Option<u32> {
    let hk = reg_open_hklm_read(subkey)?;
    reg_read_dword(hk.0, name)
}

fn reg_hklm_sz(subkey: &str, name: &str) -> Option<String> {
    let hk = reg_open_hklm_read(subkey)?;
    reg_read_sz(hk.0, name)
}

fn reg_hklm_multi_sz(subkey: &str, name: &str) -> Option<Vec<String>> {
    let hk = reg_open_hklm_read(subkey)?;
    reg_read_multi_sz(hk.0, name)
}

/// Read Dev Drive enablement, AV filter policy, attach policy, and Defender settings directly
/// from HKLM (works on non-English Windows without parsing localized `fsutil` prose).
pub fn read_dev_drive_policy() -> DevDrivePolicyReg {
    let os_build = reg_hklm_sz(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuildNumber")
        .and_then(|s| s.trim().parse::<u32>().ok());
    let enabled = reg_hklm_dword(r"SYSTEM\CurrentControlSet\Policies", "FsEnableDevDrive")
        .or_else(|| reg_hklm_dword(r"SYSTEM\CurrentControlSet\Control\FileSystem", "FsEnableDevDrive"))
        .map(|v| v != 0)
        .or_else(|| os_build.filter(|&b| b >= 22621).map(|_| true));
    let av_filters = match reg_hklm_dword(r"SYSTEM\CurrentControlSet\Policies", "FltmgrDevDriveAllowAntivirusFilter")
        .or_else(|| reg_hklm_dword(r"SYSTEM\CurrentControlSet\Control\FilterManager", "DevDriveAllowAntivirusFilter"))
    {
        Some(0) => "antivirus_disallowed",
        Some(_) => "antivirus_allowed",
        None if enabled == Some(true) => "antivirus_allowed",
        None => "unknown",
    };
    let attach_policy = reg_hklm_multi_sz(r"SYSTEM\CurrentControlSet\Policies", "FltmgrDevDriveAttachPolicy")
        .or_else(|| reg_hklm_multi_sz(r"SYSTEM\CurrentControlSet\Control\FilterManager", "DevDriveAttachPolicy"))
        .unwrap_or_default();
    let perf_mode = reg_hklm_dword(r"SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection", "PerformanceModeStatus")
        .or_else(|| reg_hklm_dword(r"SOFTWARE\Microsoft\Windows Defender\Real-Time Protection", "PerformanceModeStatus"));
    let realtime_disabled = reg_hklm_dword(r"SOFTWARE\Policies\Microsoft\Windows Defender\Real-Time Protection", "DisableRealtimeMonitoring")
        .or_else(|| reg_hklm_dword(r"SOFTWARE\Microsoft\Windows Defender\Real-Time Protection", "DisableRealtimeMonitoring"))
        .map(|v| v != 0);
    let passive_mode = reg_hklm_dword(r"SOFTWARE\Microsoft\Windows Defender", "PassiveMode").map(|v| v != 0);
    DevDrivePolicyReg { enabled, av_filters, attach_policy, perf_mode, realtime_disabled, passive_mode, os_build }
}

// ---- Dynamic virtdisk.dll (VHD/VHDX backing) ----

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtStorageType {
    device_id: u32,
    vendor_id: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StorageDependencyInfoType2 {
    dependency_type_flags: u32,
    provider_specific_flags: u32,
    virtual_storage_type: VirtStorageType,
    ancestor_level: u32,
    dependency_device_name: *mut u16,
    host_volume_name: *mut u16,
    dependent_volume_name: *mut u16,
    dependent_volume_relative_path: *mut u16,
}

#[repr(C)]
struct StorageDependencyInfoHeader {
    version: i32,
    number_entries: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OpenVirtualDiskParamsV2 {
    version: i32,
    get_info_only: i32,
    read_only: i32,
    resiliency_guid: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GetVirtualDiskInfoSize {
    version: i32,
    _pad: u32,
    virtual_size: u64,
    physical_size: u64,
    block_size: u32,
    sector_size: u32,
}

type GetStorageDependencyInfoFn = unsafe extern "system" fn(HANDLE, i32, u32, *mut c_void, *mut u32) -> u32;
type OpenVirtualDiskFn = unsafe extern "system" fn(*const VirtStorageType, *const u16, i32, i32, *const OpenVirtualDiskParamsV2, *mut HANDLE) -> u32;
type GetVirtualDiskInfoFn = unsafe extern "system" fn(HANDLE, *mut u32, *mut GetVirtualDiskInfoSize, *mut u32) -> u32;

struct VirtDiskApi {
    _module: HMODULE,
    get_storage_dep: GetStorageDependencyInfoFn,
    open_vhd: Option<OpenVirtualDiskFn>,
    get_vhd_info: Option<GetVirtualDiskInfoFn>,
}

// SAFETY: `HMODULE` and Win32 function pointers are process-global immutable addresses once loaded.
unsafe impl Send for VirtDiskApi {}
// SAFETY: `HMODULE` and Win32 function pointers are safe to invoke concurrently across threads.
unsafe impl Sync for VirtDiskApi {}

struct ModuleGuard(HMODULE);

impl ModuleGuard {
    fn take(mut self) -> HMODULE {
        let m = self.0;
        self.0 = null_mut();
        m
    }
}

impl Drop for ModuleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was loaded via `LoadLibraryW`.
            unsafe { FreeLibrary(self.0) };
        }
    }
}

fn virtdisk_api() -> Option<&'static VirtDiskApi> {
    static API: OnceLock<Option<VirtDiskApi>> = OnceLock::new();
    API.get_or_init(|| {
        let name = wide("virtdisk.dll");
        // SAFETY: `name` is NUL-terminated; `ModuleGuard` frees on early return.
        let m = unsafe { LoadLibraryW(name.as_ptr()) };
        if m.is_null() {
            return None;
        }
        let guard = ModuleGuard(m);
        // SAFETY: `m` is valid and proc names are NUL-terminated ASCII.
        let p_dep = unsafe { GetProcAddress(guard.0, b"GetStorageDependencyInformation\0".as_ptr()) }?;
        // SAFETY: signature matches `windows-sys::Win32::Storage::Vhd::GetStorageDependencyInformation`.
        let get_storage_dep: GetStorageDependencyInfoFn = unsafe { std::mem::transmute(p_dep) };
        // SAFETY: `m` is valid and proc names are NUL-terminated ASCII.
        let open_vhd = unsafe { GetProcAddress(guard.0, b"OpenVirtualDisk\0".as_ptr()) }.map(|p| unsafe { std::mem::transmute(p) });
        // SAFETY: `m` is valid and proc names are NUL-terminated ASCII.
        let get_vhd_info = unsafe { GetProcAddress(guard.0, b"GetVirtualDiskInformation\0".as_ptr()) }.map(|p| unsafe { std::mem::transmute(p) });
        Some(VirtDiskApi { _module: guard.take(), get_storage_dep, open_vhd, get_vhd_info })
    })
    .as_ref()
}

fn wstr_ptr_to_string(p: *const u16, buf: &[u8]) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let p_addr = p as usize;
    let start = buf.as_ptr() as usize;
    let end = start.checked_add(buf.len())?;
    if p_addr < start || p_addr >= end || (p_addr - start) % 2 != 0 || (p_addr % std::mem::align_of::<u16>()) != 0 {
        return None;
    }
    let byte_off = p_addr - start;
    let avail_units = (buf.len() - byte_off) / 2;
    let max_units = avail_units.min(4096);
    // SAFETY: `p` is non-null, 2-byte aligned, and within `buf[byte_off..]` for `max_units` UTF-16 units.
    let slice = unsafe { std::slice::from_raw_parts(p, max_units) };
    let len = slice.iter().position(|&c| c == 0)?;
    Some(String::from_utf16_lossy(&slice[..len]))
}

fn query_vhd_max_bytes(api: &VirtDiskApi, path: &str) -> Option<u64> {
    let open_fn = api.open_vhd?;
    let info_fn = api.get_vhd_info?;
    let vst = VirtStorageType::default();
    let wp = wide(path);
    let params = OpenVirtualDiskParamsV2 { version: 2, get_info_only: 1, read_only: 1, resiliency_guid: [0u8; 16] };
    let mut vh: HANDLE = null_mut();
    const VIRTUAL_DISK_ACCESS_GET_INFO: i32 = 0x0008_0000;
    // SAFETY: `wp` is NUL-terminated; `vst`, `params`, and `vh` are valid stack locations.
    let rc = unsafe { open_fn(&vst, wp.as_ptr(), VIRTUAL_DISK_ACCESS_GET_INFO, 0, &params, &mut vh) };
    if rc != 0 || vh.is_null() || vh == INVALID_HANDLE_VALUE {
        return None;
    }
    let _vh_guard = Handle(vh);
    let mut sz = std::mem::size_of::<GetVirtualDiskInfoSize>() as u32;
    let mut info_out = GetVirtualDiskInfoSize { version: 1, _pad: 0, virtual_size: 0, physical_size: 0, block_size: 0, sector_size: 0 };
    let mut used = 0u32;
    // SAFETY: `vh` is open; `sz`, `info_out`, and `used` are valid stack locations.
    let rc2 = unsafe { info_fn(vh, &mut sz, &mut info_out, &mut used) };
    (rc2 == 0 && info_out.virtual_size > 0).then_some(info_out.virtual_size)
}

fn probe_vhd_backing(v: &VolumeFacts) -> Option<(String, Option<u64>, Option<u64>, Option<u64>)> {
    let api = virtdisk_api()?;
    let root = v.paths.first().cloned().unwrap_or_else(|| v.guid.clone());
    let h = open(&root, FILE_READ_ATTRIBUTES)
        .or_else(|_| open(&v.guid, FILE_READ_ATTRIBUTES))
        .or_else(|_| open(&root, 0x8000_0000))
        .ok()?;
    const GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES: i32 = 1;
    const STORAGE_DEPENDENCY_INFO_VERSION_2: i32 = 2;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut buf = vec![0u64; 512]; // 4096 bytes, 8-byte aligned
    let mut used = 0u32;
    loop {
        let hdr = buf.as_mut_ptr() as *mut StorageDependencyInfoHeader;
        // SAFETY: `buf` is >= 8 bytes and 8-byte aligned.
        unsafe {
            (*hdr).version = STORAGE_DEPENDENCY_INFO_VERSION_2;
            (*hdr).number_entries = 0;
        }
        // SAFETY: `h.0` is open; `buf` is valid for `buf.len() * 8` bytes.
        let rc = unsafe { (api.get_storage_dep)(h.0, GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES, (buf.len() * 8) as u32, buf.as_mut_ptr() as *mut c_void, &mut used) };
        if rc == ERROR_INSUFFICIENT_BUFFER && used as usize > buf.len() * 8 && (used as usize) <= 256 * 1024 {
            buf.resize((used as usize).div_ceil(8), 0);
            continue;
        }
        if rc != 0 {
            return None;
        }
        break;
    }
    // SAFETY: call succeeded; `StorageDependencyInfoHeader` is at start of `buf`.
    let count = unsafe { (*(buf.as_ptr() as *const StorageDependencyInfoHeader)).number_entries } as usize;
    if count == 0 {
        return None;
    }
    let header_sz = std::mem::size_of::<StorageDependencyInfoHeader>();
    let entry_sz = std::mem::size_of::<StorageDependencyInfoType2>();
    if header_sz + entry_sz > buf.len() * 8 {
        return None;
    }
    // SAFETY: `buf` is contiguous initialized `u64`s of `buf.len() * 8` bytes.
    let buf_bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
    let mut best: Option<(u32, String, String)> = None;
    for i in 0..count {
        let off = header_sz + i * entry_sz;
        if off + entry_sz > buf.len() * 8 {
            break;
        }
        // SAFETY: `off + entry_sz <= buf.len() * 8` and `buf` is 8-byte aligned.
        let ent = unsafe { &*((buf.as_ptr() as *const u8).add(off) as *const StorageDependencyInfoType2) };
        let host_vol = wstr_ptr_to_string(ent.host_volume_name, buf_bytes).unwrap_or_default();
        let rel_path = wstr_ptr_to_string(ent.dependent_volume_relative_path, buf_bytes).unwrap_or_default();
        if rel_path.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(lvl, _, _)| ent.ancestor_level < *lvl) {
            best = Some((ent.ancestor_level, host_vol, rel_path));
        }
    }
    let (_, host_vol, rel_path) = best?;
    let host_vol_slash = if host_vol.is_empty() || host_vol.ends_with('\\') { host_vol } else { format!("{}\\", host_vol) };
    let host_mounts = if !host_vol_slash.is_empty() { path_names(&host_vol_slash) } else { Vec::new() };
    let host_root = host_mounts.first().cloned().unwrap_or_else(|| host_vol_slash.clone());
    let backed_by = if !host_root.is_empty() {
        format!("{}{}", host_root.trim_end_matches('\\'), if rel_path.starts_with('\\') { rel_path.clone() } else { format!("\\{}", rel_path) })
    } else {
        rel_path
    };
    let file_bytes = std::fs::metadata(&backed_by).map(|m| m.len()).ok();
    let host_free = if !host_root.is_empty() { free_space(&host_root).map(|x| x.0) } else { None };
    let max_bytes = query_vhd_max_bytes(api, &backed_by);
    Some((backed_by, file_bytes, host_free, max_bytes))
}

// ---- Dynamic fltlib.dll (per-volume minifilter audit) ----

type FilterVolInstFirstFn = unsafe extern "system" fn(*const u16, i32, *mut c_void, u32, *mut u32, *mut HANDLE) -> i32;
type FilterVolInstNextFn = unsafe extern "system" fn(HANDLE, i32, *mut c_void, u32, *mut u32) -> i32;
type FilterVolInstCloseFn = unsafe extern "system" fn(HANDLE) -> i32;

struct FltLibApi {
    _module: HMODULE,
    find_first: FilterVolInstFirstFn,
    find_next: FilterVolInstNextFn,
    find_close: FilterVolInstCloseFn,
}

// SAFETY: `HMODULE` and Win32 function pointers are process-global immutable addresses once loaded.
unsafe impl Send for FltLibApi {}
// SAFETY: `HMODULE` and Win32 function pointers are safe to invoke concurrently across threads.
unsafe impl Sync for FltLibApi {}

fn fltlib_api() -> Option<&'static FltLibApi> {
    static API: OnceLock<Option<FltLibApi>> = OnceLock::new();
    API.get_or_init(|| {
        let name = wide("fltlib.dll");
        // SAFETY: `name` is NUL-terminated; `ModuleGuard` frees on early return.
        let m = unsafe { LoadLibraryW(name.as_ptr()) };
        if m.is_null() {
            return None;
        }
        let guard = ModuleGuard(m);
        // SAFETY: `m` is valid and proc names are NUL-terminated ASCII.
        let p1 = unsafe { GetProcAddress(guard.0, b"FilterVolumeInstanceFindFirst\0".as_ptr()) }?;
        // SAFETY: `m` is valid and proc names are NUL-terminated ASCII.
        let p2 = unsafe { GetProcAddress(guard.0, b"FilterVolumeInstanceFindNext\0".as_ptr()) }?;
        // SAFETY: `m` is valid and proc names are NUL-terminated ASCII.
        let p3 = unsafe { GetProcAddress(guard.0, b"FilterVolumeInstanceFindClose\0".as_ptr()) }?;
        // SAFETY: signatures match `windows-sys::Win32::Storage::InstallableFileSystems`.
        Some(unsafe {
            FltLibApi {
                _module: guard.take(),
                find_first: std::mem::transmute(p1),
                find_next: std::mem::transmute(p2),
                find_close: std::mem::transmute(p3),
            }
        })
    })
    .as_ref()
}

struct FilterFindGuard<'a>(&'a FltLibApi, HANDLE);

impl Drop for FilterFindGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() && self.1 != INVALID_HANDLE_VALUE {
            // SAFETY: `self.1` was opened by `FilterVolumeInstanceFindFirst`.
            unsafe { (self.0.find_close)(self.1) };
        }
    }
}

fn extract_filter_name(buf: &[u8]) -> Option<String> {
    if buf.len() < 28 {
        return None;
    }
    let flags = u32_at(buf, 4);
    let (len_bytes, off) = if flags & 1 != 0 && buf.len() >= 40 {
        (u16::from_le_bytes([buf[32], buf[33]]) as usize, u16::from_le_bytes([buf[34], buf[35]]) as usize)
    } else if flags & 2 != 0 {
        (u16::from_le_bytes([buf[20], buf[21]]) as usize, u16::from_le_bytes([buf[22], buf[23]]) as usize)
    } else {
        return None;
    };
    if len_bytes == 0 || off + len_bytes > buf.len() {
        return None;
    }
    let units: Vec<u16> = buf[off..off + len_bytes].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let s = from_w(&units);
    (!s.is_empty()).then_some(s)
}

/// Attached filesystem filter driver names on `root` (via `fltlib.dll`). Returns `None` cleanly
/// when access is denied (e.g. non-admin) or `fltlib.dll` is unavailable.
pub fn volume_attached_filters(root: &str) -> Option<Vec<String>> {
    const INSTANCE_AGGREGATE_STANDARD_INFORMATION: i32 = 3;
    const HRESULT_ACCESS_DENIED: i32 = 0x8007_0005u32 as i32;
    const HRESULT_NO_MORE_ITEMS: i32 = 0x8007_0103u32 as i32;
    const HRESULT_INSUFFICIENT_BUFFER: i32 = 0x8007_007Au32 as i32;
    let api = fltlib_api()?;
    let vol = wide(root.trim_end_matches('\\'));
    let mut buf = vec![0u64; 512]; // 4096 bytes, 8-byte aligned
    let mut ret = 0u32;
    let mut fh: HANDLE = null_mut();
    // SAFETY: `vol` is NUL-terminated; `buf`, `ret`, and `fh` are valid stack/heap locations.
    let mut hr = unsafe { (api.find_first)(vol.as_ptr(), INSTANCE_AGGREGATE_STANDARD_INFORMATION, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret, &mut fh) };
    if hr == HRESULT_INSUFFICIENT_BUFFER && ret as usize > buf.len() * 8 && (ret as usize) <= 65_536 {
        buf.resize((ret as usize).div_ceil(8), 0);
        // SAFETY: resized buffer is valid for `buf.len() * 8` bytes.
        hr = unsafe { (api.find_first)(vol.as_ptr(), INSTANCE_AGGREGATE_STANDARD_INFORMATION, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret, &mut fh) };
    }
    if hr == HRESULT_ACCESS_DENIED || hr < 0 || fh.is_null() || fh == INVALID_HANDLE_VALUE {
        return None;
    }
    let _fh_guard = FilterFindGuard(api, fh);
    let mut filters = Vec::new();
    loop {
        let used = (ret as usize).min(buf.len() * 8);
        // SAFETY: `buf` is contiguous initialized `u64`s of `buf.len() * 8` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, used) };
        if let Some(name) = extract_filter_name(bytes) {
            if !filters.iter().any(|x: &String| x.eq_ignore_ascii_case(&name)) {
                filters.push(name);
            }
        }
        // SAFETY: `fh` is an active find handle from `FilterVolumeInstanceFindFirst`.
        let nhr = unsafe { (api.find_next)(fh, INSTANCE_AGGREGATE_STANDARD_INFORMATION, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret) };
        if nhr == HRESULT_INSUFFICIENT_BUFFER && ret as usize > buf.len() * 8 && (ret as usize) <= 65_536 {
            buf.resize((ret as usize).div_ceil(8), 0);
            // SAFETY: resized buffer is valid for `buf.len() * 8` bytes.
            let nhr2 = unsafe { (api.find_next)(fh, INSTANCE_AGGREGATE_STANDARD_INFORMATION, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret) };
            if nhr2 >= 0 {
                continue;
            }
            break;
        }
        if nhr == HRESULT_NO_MORE_ITEMS || nhr < 0 {
            break;
        }
    }
    Some(filters)
}

/// Run a console program with no window and return its stdout, or `None` on failure or timeout.
fn run_hidden(exe: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    stop_std_handle_inheritance();
    let job = ProcessJob::new().ok();
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let assigned = job.as_ref().is_some_and(|j| j.assign_pid(child.id()).is_ok());
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
                if let Some(j) = &job {
                    j.terminate();
                }
                let _ = child.kill();
                let _ = child.wait();
                drop(job);
                if assigned {
                    let _ = reader.join();
                }
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
                let mut cur_lcn = lcn as u64;
                let mut rem = (next - prev) as u64;
                while rem > 0 {
                    let chunk = rem.min(u32::MAX as u64) as u32;
                    out.push(Ext { lcn: cur_lcn, len: chunk, rc });
                    cur_lcn = cur_lcn.saturating_add(chunk as u64);
                    rem -= chunk as u64;
                }
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
    /// A `.git` file, folder, or link was seen anywhere inside the walked tree.
    #[serde(default)]
    pub holds_repo: bool,
}

impl WalkStats {
    fn add(&mut self, o: &WalkStats) {
        macro_rules! s { ($($f:ident),*) => { $( self.$f += o.$f; )* } }
        s!(files, dirs, logical, allocated, reparse_dirs, reparse_files, sparse_files, compressed_files, cloud_files, hard_links,
           dir_failures, open_failures, extent_failures, files_without_refcount, written_during_scan);
        self.last_write = self.last_write.max(o.last_write);
        self.cut_short |= o.cut_short;
        self.holds_repo |= o.holds_repo;
    }
}

/// Inline FxHashMap for fast `u128` `FileId` hashing with zero external dependencies.
#[derive(Clone, Default)]
pub(crate) struct FxBuildHasher;

pub(crate) struct FxHasher(u64);

impl std::hash::BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        FxHasher(0)
    }
}

impl FxHasher {
    #[inline]
    fn add(&mut self, v: u64) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(K);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.add(u64::from_ne_bytes(chunk.try_into().unwrap()));
        }
        for &b in chunks.remainder() {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.add(i as u64);
        self.add((i >> 64) as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
}

pub(crate) type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// A file with more than one name: its extents are attributed once all names are known.
#[derive(Debug, Clone, Default)]
pub struct MultiLink {
    pub links: u32,
    pub first_item: usize,
    /// Names found per item.
    pub names: HashMap<usize, u32>,
    pub alloc_clusters: u64,
    pub first_alloc: u64,
    pub exts: Vec<Ext>,
    pub promoted_from_exclusive: bool,
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

/// One item's roots and nested exclusions for `walk_volume`.
pub struct WalkItemInput<'a> {
    pub roots: &'a [String],
    pub skip: &'a [String],
}

/// Configuration for a single-pass persistent worker pool walk of one volume.
pub struct VolumeWalkConfig {
    pub cluster: u64,
    pub block_cloning: bool,
    pub supports_hard_links: bool,
    pub level2: bool,
    pub threads: usize,
    pub deadline: Instant,
    pub extent_deadline: Option<Instant>,
    pub stop: Arc<AtomicBool>,
    pub started_ft: u64,
    pub progress_files: Arc<AtomicU64>,
}

/// Result for one item from `walk_volume`.
#[derive(Debug, Clone, Default)]
pub struct WalkItemResult {
    pub level1: WalkStats,
    pub level2: Option<WalkStats>,
    pub extents: ItemExtents,
    pub not_reached: bool,
}

#[derive(Clone, Copy)]
struct SeenEntry {
    first_item: u32,
    alloc_clusters: u64,
    promoted: bool,
}

struct SeenFids {
    shards: [Mutex<FxHashMap<u128, SeenEntry>>; 64],
}

impl SeenFids {
    fn new() -> Self {
        Self { shards: std::array::from_fn(|_| Mutex::new(FxHashMap::default())) }
    }

    #[inline]
    fn shard_for(fid: u128) -> usize {
        let x = (fid as u64) ^ ((fid >> 64) as u64);
        (x.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize
    }
}

struct DirJob {
    path_w: Vec<u16>,
    item: usize,
    check_skip: bool,
    is_root: bool,
}

struct GlobalQueue {
    jobs: VecDeque<DirJob>,
    recycle: Vec<Vec<u16>>,
}

struct WorkQueue {
    global: Mutex<GlobalQueue>,
    locals: Vec<Mutex<Vec<DirJob>>>,
    cv: Condvar,
    pending: AtomicUsize,
    idle: AtomicUsize,
    aborted: AtomicBool,
}

struct WorkerScratch {
    dbuf: Vec<u64>,
    ebuf: Vec<u8>,
    exts: Vec<Ext>,
    name_u16: Vec<u16>,
    name_buf: String,
    dir_utf8: String,
    dir_norm: String,
    child_norm: String,
    subdirs: Vec<DirJob>,
    free_wbufs: Vec<Vec<u16>>,
    extents_cutoff: bool,
}

impl WorkerScratch {
    fn new() -> Self {
        Self {
            dbuf: vec![0u64; 16_384], // 128 KiB, 8-byte aligned
            ebuf: Vec::new(),
            exts: Vec::new(),
            name_u16: Vec::new(),
            name_buf: String::new(),
            dir_utf8: String::new(),
            dir_norm: String::new(),
            child_norm: String::new(),
            subdirs: Vec::new(),
            free_wbufs: Vec::new(),
            extents_cutoff: false,
        }
    }

    fn recycle_job_buf(&mut self, mut buf: Vec<u16>) {
        buf.clear();
        if self.free_wbufs.len() < 64 {
            self.free_wbufs.push(buf);
        }
    }

    fn drain_subdirs_to_free(&mut self) {
        for mut j in self.subdirs.drain(..) {
            j.path_w.clear();
            if self.free_wbufs.len() < 64 {
                self.free_wbufs.push(j.path_w);
            }
        }
    }
}

#[derive(Default)]
struct ItemAcc {
    l1: WalkStats,
    l2: WalkStats,
    ext: ItemExtents,
    l2_attempted: bool,
    saw_extents_cutoff: bool,
}

enum MultiSink<'a> {
    Volume {
        seen_fids: Option<&'a SeenFids>,
        multi: &'a Mutex<FxHashMap<u128, MultiLink>>,
    },
    Single {
        external_item_id: usize,
        multi: &'a Mutex<HashMap<u128, MultiLink>>,
    },
}

struct PoolCtx<'a> {
    cluster: u64,
    block_cloning: bool,
    supports_hard_links: bool,
    level2: bool,
    deadline: Instant,
    extent_deadline: Option<Instant>,
    extents_cutoff: &'a AtomicBool,
    stop: &'a AtomicBool,
    started_ft: u64,
    progress_files: &'a AtomicU64,
    skips: &'a [&'a [String]],
    stop_on_repo: bool,
    prune_dot_git: bool,
    sink: MultiSink<'a>,
}

#[inline]
fn is_dot_git_w(name_w: &[u16]) -> bool {
    name_w.len() == 4
        && name_w[0] == b'.' as u16
        && (name_w[1] | 0x20) == b'g' as u16
        && (name_w[2] | 0x20) == b'i' as u16
        && (name_w[3] | 0x20) == b't' as u16
}

fn root_last_write(path: &str) -> Option<u64> {
    let h = open(path, FILE_READ_ATTRIBUTES).ok()?;
    let mut basic_buf = [0u64; 5];
    let basic = as_bytes_mut(&mut basic_buf);
    info(h.0, FILE_BASIC_INFO_CLASS, basic).then(|| u64_at(basic, 16))
}

struct PoolWorkerGuard<'a> {
    q: &'a WorkQueue,
}

impl Drop for PoolWorkerGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.q.aborted.store(true, Ordering::Release);
            let _g = self.q.global.lock();
            self.q.cv.notify_all();
        }
    }
}

struct PendingJobGuard<'a> {
    q: &'a WorkQueue,
}

impl Drop for PendingJobGuard<'_> {
    fn drop(&mut self) {
        if self.q.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _g = self.q.global.lock();
            self.q.cv.notify_all();
        }
    }
}

fn run_pool(ctx: &PoolCtx<'_>, initial_jobs: Vec<DirJob>, item_count: usize, threads: usize) -> Vec<ItemAcc> {
    let nthreads = threads.max(1);
    let total_roots = initial_jobs.len();
    let mut merged: Vec<ItemAcc> = (0..item_count).map(|_| ItemAcc::default()).collect();
    if total_roots == 0 {
        return merged;
    }

    let q = Arc::new(WorkQueue {
        global: Mutex::new(GlobalQueue { jobs: VecDeque::from(initial_jobs), recycle: Vec::new() }),
        locals: (0..nthreads).map(|_| Mutex::new(Vec::new())).collect(),
        cv: Condvar::new(),
        pending: AtomicUsize::new(total_roots),
        idle: AtomicUsize::new(0),
        aborted: AtomicBool::new(false),
    });

    let per_worker: Vec<Vec<ItemAcc>> = std::thread::scope(|s| {
        let mut hs = Vec::with_capacity(nthreads);
        for wid in 0..nthreads {
            let q = q.clone();
            hs.push(s.spawn(move || {
                let _worker_guard = PoolWorkerGuard { q: &q };
                let mut accs: Vec<ItemAcc> = (0..item_count).map(|_| ItemAcc::default()).collect();
                let mut scratch = WorkerScratch::new();
                loop {
                    if q.aborted.load(Ordering::Acquire) {
                        break;
                    }
                    let mut job = q.locals[wid].lock().unwrap_or_else(|p| p.into_inner()).pop();
                    if job.is_none() {
                        job = q.global.lock().unwrap_or_else(|p| p.into_inner()).jobs.pop_front();
                    }
                    if job.is_none() && nthreads > 1 {
                        for off in 1..nthreads {
                            let victim = (wid + off) % nthreads;
                            if let Ok(mut v) = q.locals[victim].try_lock() {
                                if let Some(stolen) = v.pop() {
                                    job = Some(stolen);
                                    break;
                                }
                            }
                        }
                    }
                    let mut job = match job {
                        Some(j) => j,
                        None => {
                            let mut g = q.global.lock().unwrap_or_else(|p| p.into_inner());
                            if let Some(j) = g.jobs.pop_front() {
                                j
                            } else if q.aborted.load(Ordering::Acquire) || q.pending.load(Ordering::SeqCst) == 0 {
                                break;
                            } else {
                                q.idle.fetch_add(1, Ordering::SeqCst);
                                g = q.cv.wait_timeout(g, Duration::from_millis(20)).unwrap_or_else(|p| p.into_inner()).0;
                                q.idle.fetch_sub(1, Ordering::SeqCst);
                                if let Some(j) = g.jobs.pop_front() {
                                    j
                                } else if q.aborted.load(Ordering::Acquire) || q.pending.load(Ordering::SeqCst) == 0 {
                                    break;
                                } else {
                                    drop(g);
                                    continue;
                                }
                            }
                        }
                    };

                    let _job_guard = PendingJobGuard { q: &q };
                    let item = job.item;
                    if ctx.stop.load(Ordering::Relaxed) || Instant::now() > ctx.deadline {
                        let lo = &mut accs[item];
                        lo.l1.cut_short = true;
                        if lo.l2_attempted {
                            lo.l2.cut_short = true;
                        }
                    } else {
                        scan_dir(ctx, &mut job, &mut accs[item], &mut scratch);
                        if !scratch.subdirs.is_empty() {
                            let added = scratch.subdirs.len();
                            q.pending.fetch_add(added, Ordering::SeqCst);
                            let have_idle = q.idle.load(Ordering::Acquire) > 0;
                            if have_idle || scratch.free_wbufs.len() < 16 || scratch.free_wbufs.len() > 64 {
                                let mut g = q.global.lock().unwrap_or_else(|p| p.into_inner());
                                if have_idle {
                                    let keep_local = scratch.subdirs.pop().unwrap();
                                    g.jobs.extend(scratch.subdirs.drain(..));
                                    q.locals[wid].lock().unwrap_or_else(|p| p.into_inner()).push(keep_local);
                                    q.cv.notify_all();
                                } else {
                                    q.locals[wid].lock().unwrap_or_else(|p| p.into_inner()).extend(scratch.subdirs.drain(..));
                                }
                                if scratch.free_wbufs.len() < 16 {
                                    for _ in 0..32 {
                                        if let Some(b) = g.recycle.pop() {
                                            scratch.free_wbufs.push(b);
                                        } else {
                                            break;
                                        }
                                    }
                                } else if scratch.free_wbufs.len() > 64 && g.recycle.len() < 512 {
                                    let excess = scratch.free_wbufs.len() - 48;
                                    let room = 512 - g.recycle.len();
                                    for _ in 0..excess.min(room) {
                                        if let Some(b) = scratch.free_wbufs.pop() {
                                            g.recycle.push(b);
                                        }
                                    }
                                }
                            } else {
                                q.locals[wid].lock().unwrap_or_else(|p| p.into_inner()).extend(scratch.subdirs.drain(..));
                            }
                        }
                    }
                    scratch.recycle_job_buf(job.path_w);
                }
                accs
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for worker_accs in per_worker {
        for (dst, src) in merged.iter_mut().zip(worker_accs) {
            dst.l1.add(&src.l1);
            dst.l2.add(&src.l2);
            dst.ext.exclusive_clusters += src.ext.exclusive_clusters;
            dst.ext.shared.extend(src.ext.shared);
            dst.l2_attempted |= src.l2_attempted;
            dst.saw_extents_cutoff |= src.saw_extents_cutoff;
        }
    }
    merged
}

/// Walk all items on a single volume in a single traversal using a persistent work-stealing pool.
pub fn walk_volume(cfg: &VolumeWalkConfig, items: &[WalkItemInput<'_>]) -> (Vec<WalkItemResult>, Vec<crate::disk_space::LinkedFile>) {
    if items.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut initial_jobs = Vec::new();
    let mut skips: Vec<&[String]> = Vec::with_capacity(items.len());
    for (idx, it) in items.iter().enumerate() {
        skips.push(it.skip);
        for r in it.roots {
            let lr = long(r);
            let mut path_w = Vec::with_capacity(lr.len());
            path_w.extend(lr.encode_utf16());
            initial_jobs.push(DirJob { path_w, item: idx, check_skip: !it.skip.is_empty(), is_root: true });
        }
    }
    let seen_fids = (cfg.level2 && cfg.supports_hard_links).then(SeenFids::new);
    let multi: Mutex<FxHashMap<u128, MultiLink>> = Mutex::new(FxHashMap::default());
    let extents_cutoff = AtomicBool::new(false);
    let ctx = PoolCtx {
        cluster: cfg.cluster,
        block_cloning: cfg.block_cloning,
        supports_hard_links: cfg.supports_hard_links,
        level2: cfg.level2,
        deadline: cfg.deadline,
        extent_deadline: cfg.extent_deadline,
        extents_cutoff: &extents_cutoff,
        stop: &cfg.stop,
        started_ft: cfg.started_ft,
        progress_files: &cfg.progress_files,
        skips: &skips,
        stop_on_repo: false,
        prune_dot_git: false,
        sink: MultiSink::Volume { seen_fids: seen_fids.as_ref(), multi: &multi },
    };

    let merged = run_pool(&ctx, initial_jobs, items.len(), cfg.threads);
    let mut results = Vec::with_capacity(items.len());
    let mut all_exts = Vec::with_capacity(items.len());
    for mut acc in merged {
        let not_reached = acc.l1.cut_short && acc.l1.files == 0 && acc.l1.dirs == 0 && acc.l1.dir_failures == 0;
        let level2 = if !cfg.level2 || not_reached || (cfg.block_cloning && !acc.l2_attempted) {
            None
        } else {
            if acc.l1.cut_short || acc.saw_extents_cutoff {
                acc.l2.cut_short = true;
            }
            Some(acc.l2)
        };
        all_exts.push(acc.ext);
        results.push(WalkItemResult { level1: acc.l1, level2, extents: ItemExtents::default(), not_reached });
    }
    let multi_map = multi.into_inner().unwrap();
    let linked = attribute_multilinks(multi_map, &mut all_exts, cfg.block_cloning);
    for (res, ext) in results.iter_mut().zip(all_exts) {
        res.extents = ext;
    }
    (results, linked)
}

/// Walk one item's roots. `skip` holds normalised folders that belong to deeper items.
pub fn walk_item(vs: &VolumeScan, item: usize, roots: &[String], skip: &[String]) -> (WalkStats, ItemExtents) {
    let mut initial_jobs = Vec::with_capacity(roots.len());
    for r in roots {
        let lr = long(r);
        let mut path_w = Vec::with_capacity(lr.len());
        path_w.extend(lr.encode_utf16());
        initial_jobs.push(DirJob { path_w, item: 0, check_skip: !skip.is_empty(), is_root: true });
    }
    let skips = [skip];
    let extents_cutoff = AtomicBool::new(false);
    let ctx = PoolCtx {
        cluster: vs.cluster,
        block_cloning: vs.block_cloning,
        supports_hard_links: true,
        level2: vs.level2,
        deadline: vs.deadline,
        extent_deadline: None,
        extents_cutoff: &extents_cutoff,
        stop: &vs.stop,
        started_ft: vs.started_ft,
        progress_files: &vs.progress_files,
        skips: &skips,
        stop_on_repo: false,
        prune_dot_git: false,
        sink: MultiSink::Single { external_item_id: item, multi: &vs.multi },
    };
    let mut merged = run_pool(&ctx, initial_jobs, 1, vs.threads);
    let acc = merged.pop().unwrap_or_default();
    let mut stats = if vs.level2 {
        let mut s = acc.l2;
        if acc.l1.cut_short {
            s.cut_short = true;
        }
        s
    } else {
        acc.l1
    };
    // If any root failed to read FILE_BASIC_INFO on the directory handle or was skipped before pop
    // (only when not cut short), keep fallback root_last_write check if last_write == 0.
    if stats.last_write == 0 && !stats.cut_short {
        for r in roots {
            if let Some(lw) = root_last_write(r) {
                if lw >= vs.started_ft {
                    stats.written_during_scan += 1;
                }
                stats.last_write = stats.last_write.max(lw);
            }
        }
    }
    (stats, acc.ext)
}

/// Fast multi-threaded level-1 walk over clean candidate paths in a single pool pass.
/// Computes `last_write`, `dir_failures`, `cut_short`, and `holds_repo` per path without
/// opening files or reading extents.
pub fn walk_clean_paths(paths: &[String], threads: usize, deadline: Instant, prune_dot_git: bool) -> Vec<WalkStats> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut initial_jobs = Vec::with_capacity(paths.len());
    let skips: Vec<&[String]> = vec![&[]; paths.len()];
    for (idx, r) in paths.iter().enumerate() {
        let lr = long(r);
        let mut path_w = Vec::with_capacity(lr.len());
        path_w.extend(lr.encode_utf16());
        initial_jobs.push(DirJob { path_w, item: idx, check_skip: false, is_root: true });
    }
    let stop = AtomicBool::new(false);
    let extents_cutoff = AtomicBool::new(false);
    let progress_files = AtomicU64::new(0);
    let multi: Mutex<FxHashMap<u128, MultiLink>> = Mutex::new(FxHashMap::default());
    let ctx = PoolCtx {
        cluster: 4096,
        block_cloning: false,
        supports_hard_links: false,
        level2: false,
        deadline,
        extent_deadline: None,
        extents_cutoff: &extents_cutoff,
        stop: &stop,
        started_ft: u64::MAX,
        progress_files: &progress_files,
        skips: &skips,
        stop_on_repo: false,
        prune_dot_git,
        sink: MultiSink::Volume { seen_fids: None, multi: &multi },
    };
    let merged = run_pool(&ctx, initial_jobs, paths.len(), threads);
    let mut out = Vec::with_capacity(paths.len());
    for (idx, acc) in merged.into_iter().enumerate() {
        let mut st = acc.l1;
        if st.last_write == 0 && !st.cut_short && st.dir_failures == 0 {
            if let Some(lw) = root_last_write(&paths[idx]) {
                st.last_write = lw;
            }
        }
        out.push(st);
    }
    out
}

/// Does `root` hold a git repository (a `.git` file, folder, or link) anywhere inside it?
/// Reparse points are never followed. `None` when the walk couldn't read everything before `deadline`.
pub fn holds_repo(root: &str, deadline: Instant) -> Option<bool> {
    if Instant::now() > deadline {
        return None;
    }
    let lr = long(root);
    let path_w: Vec<u16> = lr.encode_utf16().collect();
    let skips: [&[String]; 1] = [&[]];
    let stop = AtomicBool::new(false);
    let extents_cutoff = AtomicBool::new(false);
    let progress_files = AtomicU64::new(0);
    let multi: Mutex<FxHashMap<u128, MultiLink>> = Mutex::new(FxHashMap::default());
    let ctx = PoolCtx {
        cluster: 4096,
        block_cloning: false,
        supports_hard_links: false,
        level2: false,
        deadline,
        extent_deadline: None,
        extents_cutoff: &extents_cutoff,
        stop: &stop,
        started_ft: u64::MAX,
        progress_files: &progress_files,
        skips: &skips,
        stop_on_repo: true,
        prune_dot_git: true,
        sink: MultiSink::Volume { seen_fids: None, multi: &multi },
    };
    let merged = run_pool(&ctx, vec![DirJob { path_w, item: 0, check_skip: false, is_root: true }], 1, 4);
    let st = &merged[0].l1;
    if st.holds_repo {
        Some(true)
    } else if st.cut_short || st.dir_failures > 0 {
        None
    } else {
        Some(false)
    }
}

#[inline]
fn add_file_flags(s: &mut WalkStats, attrs: u32) {
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
}

#[inline]
fn check_cutoffs(ctx: &PoolCtx<'_>, lo: &mut ItemAcc, scratch: &mut WorkerScratch) -> bool {
    if ctx.stop.load(Ordering::Relaxed) || Instant::now() > ctx.deadline {
        lo.l1.cut_short = true;
        if lo.l2_attempted {
            lo.l2.cut_short = true;
        }
        scratch.drain_subdirs_to_free();
        return true;
    }
    if let Some(ed) = ctx.extent_deadline {
        if !scratch.extents_cutoff && (ctx.extents_cutoff.load(Ordering::Relaxed) || Instant::now() > ed) {
            scratch.extents_cutoff = true;
            ctx.extents_cutoff.store(true, Ordering::Relaxed);
            lo.saw_extents_cutoff = true;
            if lo.l2_attempted {
                lo.l2.cut_short = true;
            }
        }
    }
    false
}

fn scan_dir(ctx: &PoolCtx<'_>, job: &mut DirJob, lo: &mut ItemAcc, scratch: &mut WorkerScratch) {
    if !scratch.extents_cutoff && ctx.extents_cutoff.load(Ordering::Relaxed) {
        scratch.extents_cutoff = true;
    }
    if ctx.extent_deadline.is_some_and(|ed| !scratch.extents_cutoff && Instant::now() > ed) {
        scratch.extents_cutoff = true;
        ctx.extents_cutoff.store(true, Ordering::Relaxed);
    }

    let skip = ctx.skips[job.item];
    let (has_direct_skip, child_check_skip, prefix_len) = if job.check_skip && !skip.is_empty() {
        scratch.dir_utf8.clear();
        for ch in std::char::decode_utf16(job.path_w.iter().copied()) {
            scratch.dir_utf8.push(ch.unwrap_or('\u{FFFD}'));
        }
        crate::disk_space::norm_into(&scratch.dir_utf8, &mut scratch.dir_norm);
        if skip.iter().any(|s| s == &scratch.dir_norm) {
            return;
        }
        let any_descendant_skip = skip.iter().any(|s| crate::disk_space::is_within(s, &scratch.dir_norm) && s.len() > scratch.dir_norm.len());
        let prefix_len = if scratch.dir_norm.ends_with('\\') { scratch.dir_norm.len() } else { scratch.dir_norm.len() + 1 };
        let has_direct_skip = any_descendant_skip
            && skip.iter().any(|s| crate::disk_space::is_within(s, &scratch.dir_norm) && s.len() > prefix_len && !s[prefix_len..].contains('\\'));
        (has_direct_skip, any_descendant_skip, prefix_len)
    } else {
        (false, false, 0)
    };

    let l2_active_at_start = ctx.level2 && (!ctx.block_cloning || !scratch.extents_cutoff);
    if l2_active_at_start {
        lo.l2_attempted = true;
    } else if ctx.level2 && scratch.extents_cutoff {
        lo.saw_extents_cutoff = true;
    }

    let h = match open_dir_w(&mut job.path_w, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE) {
        Ok(h) => h,
        Err(err) if job.is_root && matches!(err, 5 | 32) => {
            match open_dir_w(&mut job.path_w, FILE_READ_ATTRIBUTES | SYNCHRONIZE) {
                Ok(h) => h,
                Err(_) => {
                    lo.l1.dir_failures += 1;
                    if l2_active_at_start {
                        lo.l2.dir_failures += 1;
                    }
                    return;
                }
            }
        }
        Err(_) => {
            lo.l1.dir_failures += 1;
            if l2_active_at_start {
                lo.l2.dir_failures += 1;
            }
            return;
        }
    };

    if job.is_root {
        let mut basic_buf = [0u64; 5];
        let basic = as_bytes_mut(&mut basic_buf);
        if info(h.0, FILE_BASIC_INFO_CLASS, basic) {
            let lw = u64_at(basic, 16);
            let root_attrs = u32_at(basic, 32);
            if lw >= ctx.started_ft {
                lo.l1.written_during_scan += 1;
                if l2_active_at_start {
                    lo.l2.written_during_scan += 1;
                }
            }
            lo.l1.last_write = lo.l1.last_write.max(lw);
            if l2_active_at_start {
                lo.l2.last_write = lo.l2.last_write.max(lw);
            }
            if root_attrs & A_REPARSE != 0 {
                if root_attrs & A_DIR != 0 {
                    lo.l1.dirs += 1;
                    lo.l1.reparse_dirs += 1;
                    if l2_active_at_start {
                        lo.l2.dirs += 1;
                        lo.l2.reparse_dirs += 1;
                    }
                } else {
                    lo.l1.reparse_files += 1;
                    if l2_active_at_start {
                        lo.l2.reparse_files += 1;
                    }
                }
                return;
            }
            if root_attrs & A_DIR == 0 {
                if ctx.stop_on_repo {
                    lo.l1.dir_failures += 1;
                    return;
                }
                let mut std_buf = [0u64; 3];
                let mut id_buf = [0u64; 3];
                let std_info = as_bytes_mut(&mut std_buf);
                let id_info = as_bytes_mut(&mut id_buf);
                if !info(h.0, FILE_STANDARD_INFO_CLASS, std_info) || !info(h.0, FILE_ID_INFO_CLASS, id_info) {
                    lo.l1.dir_failures += 1;
                    if l2_active_at_start {
                        lo.l2.dir_failures += 1;
                    }
                    return;
                }
                let alloc = i64_at(std_info, 0).max(0) as u64;
                let eof = i64_at(std_info, 8).max(0) as u64;
                let fid = u128::from_le_bytes(id_info[8..24].try_into().unwrap());
                count_file_with_handle(
                    ctx,
                    job.item,
                    Some(h.0),
                    null_mut(),
                    &[],
                    lw,
                    eof,
                    alloc,
                    root_attrs,
                    fid,
                    lo,
                    &mut scratch.ebuf,
                    &mut scratch.exts,
                    scratch.extents_cutoff,
                );
                return;
            }
        }
    }
    lo.l1.dirs += 1;
    if l2_active_at_start {
        lo.l2.dirs += 1;
    }

    let dbuf_byte_len = scratch.dbuf.len() * 8;
    let mut entry_count = 0usize;
    loop {
        if check_cutoffs(ctx, lo, scratch) {
            return;
        }
        // SAFETY: `scratch.dbuf` is valid for `dbuf_byte_len` bytes.
        let dbuf_bytes = unsafe { std::slice::from_raw_parts_mut(scratch.dbuf.as_mut_ptr() as *mut u8, dbuf_byte_len) };
        if let Err(err) = info_res(h.0, FILE_ID_EXTD_DIRECTORY_INFO_CLASS, dbuf_bytes) {
            if err != ERROR_NO_MORE_FILES {
                lo.l1.dir_failures += 1;
                if ctx.level2 && !scratch.extents_cutoff {
                    lo.l2.dir_failures += 1;
                }
            }
            break;
        }
        let mut o = 0usize;
        loop {
            if entry_count & 63 == 0 && entry_count > 0 && check_cutoffs(ctx, lo, scratch) {
                return;
            }
            entry_count += 1;
            if o + 88 > dbuf_bytes.len() {
                break;
            }
            let b = &dbuf_bytes[o..];
            let next = u32_at(b, 0) as usize;
            let last_write = u64_at(b, 24);
            let eof = u64_at(b, 40);
            let alloc = u64_at(b, 48);
            let attrs = u32_at(b, 56);
            let nl = (u32_at(b, 60) as usize).min(b.len() - 88);
            let wlen = nl / 2;
            let fid = u128::from_le_bytes(b[72..88].try_into().unwrap());

            let name_ptr = b[88..].as_ptr();
            let name_w: &[u16] = if (name_ptr as usize) & 1 == 0 {
                // SAFETY: `name_ptr` is 2-byte aligned and `wlen * 2 <= b.len() - 88`.
                unsafe { std::slice::from_raw_parts(name_ptr as *const u16, wlen) }
            } else {
                scratch.name_u16.clear();
                for c in b[88..88 + wlen * 2].chunks_exact(2) {
                    scratch.name_u16.push(u16::from_le_bytes([c[0], c[1]]));
                }
                &scratch.name_u16
            };

            let is_dot = wlen == 0 || matches!(name_w, [0x002E] | [0x002E, 0x002E]);
            if !is_dot {
                if last_write >= ctx.started_ft {
                    lo.l1.written_during_scan += 1;
                }
                lo.l1.last_write = lo.l1.last_write.max(last_write);
                let is_git = is_dot_git_w(name_w);
                if is_git {
                    lo.l1.holds_repo = true;
                    if ctx.level2 && !scratch.extents_cutoff {
                        lo.l2.holds_repo = true;
                    }
                    if ctx.stop_on_repo {
                        ctx.stop.store(true, Ordering::Release);
                        scratch.drain_subdirs_to_free();
                        return;
                    }
                }
                if attrs & A_DIR != 0 {
                    if ctx.level2 && !scratch.extents_cutoff {
                        if last_write >= ctx.started_ft {
                            lo.l2.written_during_scan += 1;
                        }
                        lo.l2.last_write = lo.l2.last_write.max(last_write);
                    }
                    if attrs & A_REPARSE != 0 {
                        lo.l1.reparse_dirs += 1; // a junction or mount folder: never followed
                        if ctx.level2 && !scratch.extents_cutoff {
                            lo.l2.reparse_dirs += 1;
                        }
                    } else if !is_git || !ctx.prune_dot_git {
                        let skipped = if has_direct_skip {
                            scratch.name_buf.clear();
                            for ch in std::char::decode_utf16(name_w.iter().copied()) {
                                scratch.name_buf.push(ch.unwrap_or('\u{FFFD}'));
                            }
                            crate::disk_space::norm_into(&scratch.name_buf, &mut scratch.child_norm);
                            skip.iter().any(|s| {
                                crate::disk_space::is_within(s, &scratch.dir_norm)
                                    && s.len() > prefix_len
                                    && &s[prefix_len..] == scratch.child_norm.as_str()
                            })
                        } else {
                            false
                        };
                        if !skipped {
                            let mut child_w = scratch.free_wbufs.pop().unwrap_or_default();
                            child_w.clear();
                            let parent_trimmed = job.path_w.strip_suffix(&[b'\\' as u16]).unwrap_or(&job.path_w);
                            child_w.reserve(parent_trimmed.len() + 1 + name_w.len());
                            child_w.extend_from_slice(parent_trimmed);
                            child_w.push(b'\\' as u16);
                            child_w.extend_from_slice(name_w);
                            scratch.subdirs.push(DirJob { path_w: child_w, item: job.item, check_skip: child_check_skip, is_root: false });
                        }
                    }
                } else {
                    count_file(
                        ctx,
                        job.item,
                        h.0,
                        name_w,
                        last_write,
                        eof,
                        alloc,
                        attrs,
                        fid,
                        lo,
                        &mut scratch.ebuf,
                        &mut scratch.exts,
                        scratch.extents_cutoff,
                    );
                }
            }
            if next == 0 {
                break;
            }
            o += next;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn count_file(
    ctx: &PoolCtx<'_>,
    item: usize,
    dir_h: HANDLE,
    name_w: &[u16],
    last_write: u64,
    eof: u64,
    alloc: u64,
    attrs: u32,
    fid: u128,
    lo: &mut ItemAcc,
    ebuf: &mut Vec<u8>,
    exts: &mut Vec<Ext>,
    extents_cutoff: bool,
) {
    count_file_with_handle(ctx, item, None, dir_h, name_w, last_write, eof, alloc, attrs, fid, lo, ebuf, exts, extents_cutoff);
}

#[allow(clippy::too_many_arguments)]
fn count_file_with_handle(
    ctx: &PoolCtx<'_>,
    item: usize,
    existing_fh: Option<HANDLE>,
    dir_h: HANDLE,
    name_w: &[u16],
    last_write: u64,
    eof: u64,
    alloc: u64,
    attrs: u32,
    fid: u128,
    lo: &mut ItemAcc,
    ebuf: &mut Vec<u8>,
    exts: &mut Vec<Ext>,
    extents_cutoff: bool,
) {
    let open_target = || -> Result<(Option<Handle>, HANDLE), u32> {
        if let Some(fh) = existing_fh {
            Ok((None, fh))
        } else {
            let h = open_relative(dir_h, name_w, FILE_READ_ATTRIBUTES)?;
            let raw = h.0;
            Ok((Some(h), raw))
        }
    };
    ctx.progress_files.fetch_add(1, Ordering::Relaxed);
    lo.l1.files += 1;
    lo.l1.logical += eof;
    lo.l1.allocated += alloc;
    add_file_flags(&mut lo.l1, attrs);

    if !ctx.level2 {
        return;
    }
    if ctx.block_cloning && extents_cutoff {
        lo.saw_extents_cutoff = true;
        return;
    }

    lo.l2_attempted = true;
    if last_write >= ctx.started_ft {
        lo.l2.written_during_scan += 1;
    }
    lo.l2.last_write = lo.l2.last_write.max(last_write);

    let alloc_clusters = if ctx.cluster > 0 { alloc.div_ceil(ctx.cluster) } else { 0 };

    match &ctx.sink {
        MultiSink::Volume { seen_fids, multi } => {
            if alloc == 0 {
                if let Some(sf) = seen_fids {
                    let shard_idx = SeenFids::shard_for(fid);
                    let mut shard = sf.shards[shard_idx].lock().unwrap();
                    if shard.contains_key(&fid) {
                        lo.l2.hard_links += 1;
                        return;
                    }
                    shard.insert(fid, SeenEntry { first_item: item as u32, alloc_clusters: 0, promoted: false });
                }
                lo.l2.files += 1;
                lo.l2.logical += eof;
                add_file_flags(&mut lo.l2, attrs);
                return;
            }

            if !ctx.block_cloning {
                if let Some(sf) = seen_fids {
                    let shard_idx = SeenFids::shard_for(fid);
                    let mut shard = sf.shards[shard_idx].lock().unwrap();
                    match shard.get_mut(&fid) {
                        None => {
                            shard.insert(fid, SeenEntry { first_item: item as u32, alloc_clusters, promoted: false });
                            drop(shard);
                            lo.l2.files += 1;
                            lo.l2.logical += eof;
                            lo.l2.allocated += alloc;
                            add_file_flags(&mut lo.l2, attrs);
                            lo.ext.exclusive_clusters += alloc_clusters;
                            return;
                        }
                        Some(entry) if !entry.promoted => {
                            entry.promoted = true;
                            let first_item = entry.first_item as usize;
                            let first_alloc = entry.alloc_clusters;
                            {
                                // Strict lock order: shard -> multi.
                                let mut m = multi.lock().unwrap();
                                let mut ml = MultiLink {
                                    links: 2,
                                    first_item,
                                    names: HashMap::new(),
                                    alloc_clusters: first_alloc.max(alloc_clusters),
                                    first_alloc,
                                    exts: Vec::new(),
                                    promoted_from_exclusive: true,
                                };
                                *ml.names.entry(first_item).or_default() += 1;
                                *ml.names.entry(item).or_default() += 1;
                                m.insert(fid, ml);
                            }
                            drop(shard);
                            // Check true NumberOfLinks once on the 2nd sighting to catch external links.
                            match open_target() {
                                Ok((_guard, fh)) => {
                                    let mut std_buf = [0u64; 3];
                                    let std_info = as_bytes_mut(&mut std_buf);
                                    if info(fh, FILE_STANDARD_INFO_CLASS, std_info) {
                                        let links = u32_at(std_info, 16).max(1);
                                        if links > 2 {
                                            if let Some(ml) = multi.lock().unwrap().get_mut(&fid) {
                                                ml.links = ml.links.max(links);
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    lo.l2.open_failures += 1;
                                }
                            }
                            lo.l2.hard_links += 1;
                            return;
                        }
                        Some(_) => {
                            drop(shard);
                            let mut m = multi.lock().unwrap();
                            if let Some(ml) = m.get_mut(&fid) {
                                *ml.names.entry(item).or_default() += 1;
                                let total_seen: u32 = ml.names.values().sum();
                                if total_seen > ml.links {
                                    ml.links = total_seen;
                                }
                            }
                            lo.l2.hard_links += 1;
                            return;
                        }
                    }
                } else {
                    // !supports_hard_links and !block_cloning: 0 file opens!
                    lo.l2.files += 1;
                    lo.l2.logical += eof;
                    lo.l2.allocated += alloc;
                    add_file_flags(&mut lo.l2, attrs);
                    lo.ext.exclusive_clusters += alloc_clusters;
                    return;
                }
            }

            // ReFS (block_cloning == true) with alloc > 0:
            if ctx.supports_hard_links {
                let mut m = multi.lock().unwrap();
                if let Some(e) = m.get_mut(&fid) {
                    *e.names.entry(item).or_default() += 1;
                    let total_seen: u32 = e.names.values().sum();
                    if total_seen > e.links {
                        e.links = total_seen;
                    }
                    lo.l2.hard_links += 1;
                    return;
                }
            }

            let (_fh_guard, fh) = match open_target() {
                Ok(pair) => pair,
                Err(_) => {
                    lo.l2.open_failures += 1;
                    lo.l2.files += 1;
                    lo.l2.logical += eof;
                    lo.l2.allocated += alloc;
                    add_file_flags(&mut lo.l2, attrs);
                    return;
                }
            };
            let links = if ctx.supports_hard_links {
                let mut std_buf = [0u64; 3];
                let std_info = as_bytes_mut(&mut std_buf);
                if info(fh, FILE_STANDARD_INFO_CLASS, std_info) {
                    u32_at(std_info, 16).max(1)
                } else {
                    1
                }
            } else {
                1
            };

            if ebuf.len() < 65_536 {
                ebuf.resize(65_536, 0);
            }
            if links > 1 {
                {
                    let mut m = multi.lock().unwrap();
                    if let Some(e) = m.get_mut(&fid) {
                        *e.names.entry(item).or_default() += 1;
                        let total_seen: u32 = e.names.values().sum();
                        if total_seen > e.links {
                            e.links = total_seen;
                        }
                        lo.l2.hard_links += 1;
                        return;
                    }
                    let mut ml = MultiLink { links, first_item: item, alloc_clusters, ..Default::default() };
                    ml.names.insert(item, 1);
                    m.insert(fid, ml);
                }
                exts.clear();
                let (rc_ok, err) = extents(fh, ebuf, true, exts);
                if err != 0 {
                    lo.l2.extent_failures += 1;
                }
                if !rc_ok {
                    lo.l2.files_without_refcount += 1;
                }
                if let Some(e) = multi.lock().unwrap().get_mut(&fid) {
                    e.exts = exts.clone();
                }
            } else {
                exts.clear();
                let (rc_ok, err) = extents(fh, ebuf, true, exts);
                if err != 0 {
                    lo.l2.extent_failures += 1;
                }
                if !rc_ok {
                    lo.l2.files_without_refcount += 1;
                }
                for e in exts.iter() {
                    if e.rc == 1 {
                        lo.ext.exclusive_clusters += e.len as u64;
                    } else {
                        lo.ext.shared.push(*e);
                    }
                }
            }
            lo.l2.files += 1;
            lo.l2.logical += eof;
            lo.l2.allocated += alloc;
            add_file_flags(&mut lo.l2, attrs);
        }
        MultiSink::Single { external_item_id, multi } => {
            let ext_item = *external_item_id;
            if alloc == 0 {
                lo.l2.files += 1;
                lo.l2.logical += eof;
                add_file_flags(&mut lo.l2, attrs);
                return;
            }
            {
                let mut m = multi.lock().unwrap();
                if let Some(e) = m.get_mut(&fid) {
                    *e.names.entry(ext_item).or_default() += 1;
                    lo.l2.hard_links += 1;
                    return;
                }
            }
            let (_fh_guard, fh) = match open_target() {
                Ok(pair) => pair,
                Err(_) => {
                    lo.l2.open_failures += 1;
                    lo.l2.files += 1;
                    lo.l2.logical += eof;
                    lo.l2.allocated += alloc;
                    add_file_flags(&mut lo.l2, attrs);
                    return;
                }
            };
            let mut std_buf = [0u64; 3];
            let std_info = as_bytes_mut(&mut std_buf);
            let links = if info(fh, FILE_STANDARD_INFO_CLASS, std_info) { u32_at(std_info, 16).max(1) } else { 1 };
            if links > 1 {
                {
                    let mut m = multi.lock().unwrap();
                    if let Some(e) = m.get_mut(&fid) {
                        *e.names.entry(ext_item).or_default() += 1;
                        lo.l2.hard_links += 1;
                        return;
                    }
                    let mut ml = MultiLink { links, first_item: ext_item, alloc_clusters, ..Default::default() };
                    ml.names.insert(ext_item, 1);
                    m.insert(fid, ml);
                }
                exts.clear();
                if ctx.block_cloning {
                    if ebuf.len() < 65_536 {
                        ebuf.resize(65_536, 0);
                    }
                    let (rc_ok, err) = extents(fh, ebuf, true, exts);
                    if err != 0 {
                        lo.l2.extent_failures += 1;
                    }
                    if !rc_ok {
                        lo.l2.files_without_refcount += 1;
                    }
                }
                if let Some(e) = multi.lock().unwrap().get_mut(&fid) {
                    e.exts = exts.clone();
                }
            } else if ctx.block_cloning {
                if ebuf.len() < 65_536 {
                    ebuf.resize(65_536, 0);
                }
                exts.clear();
                let (rc_ok, err) = extents(fh, ebuf, true, exts);
                if err != 0 {
                    lo.l2.extent_failures += 1;
                }
                if !rc_ok {
                    lo.l2.files_without_refcount += 1;
                }
                for e in exts.iter() {
                    if e.rc == 1 {
                        lo.ext.exclusive_clusters += e.len as u64;
                    } else {
                        lo.ext.shared.push(*e);
                    }
                }
            } else {
                lo.ext.exclusive_clusters += alloc_clusters;
            }
            lo.l2.files += 1;
            lo.l2.logical += eof;
            lo.l2.allocated += alloc;
            add_file_flags(&mut lo.l2, attrs);
        }
    }
}

/// Attribute every multi-link file's extents now that all its names are known. When
/// `block_cloning` is enabled, preserves raw volume reference counts in `LinkedFile` records
/// so the sweep tracks hard links and block-cloned sharing independently; without block
/// cloning, each file gets a synthetic disjoint LCN run beyond any real cluster number.
pub fn attribute_multilinks<S: std::hash::BuildHasher>(
    multi: HashMap<u128, MultiLink, S>,
    items: &mut [ItemExtents],
    block_cloning: bool,
) -> Vec<crate::disk_space::LinkedFile> {
    let mut linked = Vec::new();
    let mut synthetic = 1u64 << 62;
    for (_, ml) in multi {
        if block_cloning {
            if !ml.exts.is_empty() {
                let mut list: Vec<(usize, u32)> = ml.names.into_iter().collect();
                list.sort_unstable_by_key(|(i, _)| *i);
                linked.push(crate::disk_space::LinkedFile { links: ml.links, items: list, exts: ml.exts });
            }
        } else if ml.alloc_clusters > 0 {
            if ml.promoted_from_exclusive {
                if let Some(first) = items.get_mut(ml.first_item) {
                    first.exclusive_clusters = first.exclusive_clusters.saturating_sub(ml.first_alloc);
                }
            }
            let mut rem = ml.alloc_clusters;
            while rem > 0 {
                let chunk = rem.min(u32::MAX as u64) as u32;
                let e = Ext { lcn: synthetic, len: chunk, rc: ml.links };
                synthetic = synthetic.saturating_add(chunk as u64);
                for (item, k) in &ml.names {
                    if let Some(it) = items.get_mut(*item) {
                        for _ in 0..*k {
                            it.shared.push(e);
                        }
                    }
                }
                rem -= chunk as u64;
            }
        }
    }
    linked
}

// ---------------------------------------------------------------- fast multi-threaded deletion

/// Stats returned by `disk_win::delete_tree`.
#[derive(Debug, Clone, Default)]
pub struct DeleteStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub links: u64,
    pub linked: u64,
    pub linked_bytes: u64,
    pub failed: Vec<(String, String)>,
    pub failed_count: u64,
    pub remains: bool,
}

impl DeleteStats {
    fn push_fail_w(&mut self, path_w: &[u16], err: u32) {
        self.failed_count += 1;
        if self.failed.len() < 20 {
            let msg = std::io::Error::from_raw_os_error(err as i32).to_string();
            self.failed.push((display_from_long_w(path_w), msg));
        }
    }

    fn merge_from(&mut self, o: DeleteStats) {
        self.files += o.files;
        self.dirs += o.dirs;
        self.bytes += o.bytes;
        self.links += o.links;
        self.linked += o.linked;
        self.linked_bytes += o.linked_bytes;
        self.failed_count += o.failed_count;
        let room = 20usize.saturating_sub(self.failed.len());
        self.failed.extend(o.failed.into_iter().take(room));
    }
}

fn long_wide(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    if let Some(s) = p.to_str() {
        return long(s).encode_utf16().collect();
    }
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let raw: Vec<u16> = abs.as_os_str().encode_wide().collect();
    if raw.is_empty() {
        return raw;
    }
    let bs = b'\\' as u16;
    let fs = b'/' as u16;
    let qm = b'?' as u16;
    let dot = b'.' as u16;
    let mut norm_w = Vec::with_capacity(raw.len() + 8);
    let mut i = 0usize;
    let is_unc = raw.len() >= 2 && (raw[0] == bs || raw[0] == fs) && (raw[1] == bs || raw[1] == fs);
    if is_unc {
        norm_w.push(bs);
        norm_w.push(bs);
        i = 2;
        while i < raw.len() && (raw[i] == bs || raw[i] == fs) {
            i += 1;
        }
    }
    let mut prev_sep = false;
    while i < raw.len() {
        let c = if raw[i] == fs { bs } else { raw[i] };
        if c == bs {
            if !prev_sep {
                norm_w.push(bs);
            }
            prev_sep = true;
        } else {
            norm_w.push(c);
            prev_sep = false;
        }
        i += 1;
    }
    if norm_w.len() >= 4 && norm_w[0] == bs && norm_w[1] == bs && (norm_w[2] == qm || norm_w[2] == dot) && norm_w[3] == bs {
        return norm_w;
    }
    let mut out = Vec::with_capacity(norm_w.len() + 8);
    if norm_w.len() >= 2 && norm_w[0] == bs && norm_w[1] == bs {
        out.extend([bs, bs, qm, bs, b'U' as u16, b'N' as u16, b'C' as u16, bs]);
        out.extend_from_slice(&norm_w[2..]);
    } else {
        out.extend([bs, bs, qm, bs]);
        out.extend_from_slice(&norm_w);
    }
    out
}

fn display_from_long_w(w: &[u16]) -> String {
    let s = from_w(w);
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

fn build_child_w(parent_w: &[u16], name_w: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let trimmed = parent_w.strip_suffix(&[b'\\' as u16]).unwrap_or(parent_w);
    out.reserve(trimmed.len() + 1 + name_w.len() + 1);
    out.extend_from_slice(trimmed);
    out.push(b'\\' as u16);
    out.extend_from_slice(name_w);
}

fn exists_no_follow_w(path_w: &mut Vec<u16>) -> bool {
    path_w.push(0);
    // SAFETY: `path_w` is NUL-terminated for this call.
    let attrs = unsafe { GetFileAttributesW(path_w.as_ptr()) };
    // SAFETY: plain call.
    let err = if attrs == INVALID_FILE_ATTRIBUTES { unsafe { GetLastError() } } else { 0 };
    path_w.pop();
    attrs != INVALID_FILE_ATTRIBUTES || !matches!(err, 2 | 3)
}

fn set_disposition_ex(h: HANDLE, flags: u32) -> Result<(), u32> {
    let info = FILE_DISPOSITION_INFO_EX { Flags: flags };
    // SAFETY: `h` is an open handle; `info` is a valid stack struct of size `size_of::<FILE_DISPOSITION_INFO_EX>()`.
    let ok = unsafe {
        SetFileInformationByHandle(
            h,
            FileDispositionInfoEx,
            &info as *const FILE_DISPOSITION_INFO_EX as *const c_void,
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    }
}

fn clear_readonly_w(path_w: &mut Vec<u16>, cur_attrs: u32, is_dir: bool) {
    let mut new_attrs = cur_attrs & !FILE_ATTRIBUTE_READONLY;
    if new_attrs == 0 {
        new_attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    }
    if cur_attrs & A_REPARSE != 0 {
        // Never call `SetFileAttributesW` on a reparse point (it follows the link to its target).
        // Open with `OPEN_REPARSE_POINT` + `FILE_WRITE_ATTRIBUTES` and update `FileBasicInfo`.
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        if let Ok(h) = open_dir_w(path_w, FILE_WRITE_ATTRIBUTES | SYNCHRONIZE) {
            let mut basic_buf = [0u64; 5];
            let basic = as_bytes_mut(&mut basic_buf);
            basic[32..36].copy_from_slice(&new_attrs.to_le_bytes());
            // SAFETY: `h.0` is open; `basic` is a 40-byte 8-byte-aligned `FILE_BASIC_INFO` buffer.
            unsafe {
                SetFileInformationByHandle(
                    h.0,
                    FILE_BASIC_INFO_CLASS,
                    basic.as_ptr() as *const c_void,
                    basic.len() as u32,
                );
            }
        }
        return;
    }
    path_w.push(0);
    // SAFETY: `path_w` is NUL-terminated for this call.
    unsafe { SetFileAttributesW(path_w.as_ptr(), new_attrs) };
    path_w.pop();
}

fn win32_unlink_w(path_w: &mut Vec<u16>, attrs: u32) -> Result<(), u32> {
    let is_dir = attrs & A_DIR != 0;
    if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        clear_readonly_w(path_w, attrs, is_dir);
    }
    path_w.push(0);
    // SAFETY: `path_w` is NUL-terminated for this call.
    let ok = unsafe {
        if is_dir {
            RemoveDirectoryW(path_w.as_ptr())
        } else {
            DeleteFileW(path_w.as_ptr())
        }
    };
    path_w.pop();
    if ok != 0 {
        Ok(())
    } else {
        // SAFETY: plain call.
        Err(unsafe { GetLastError() })
    }
}

fn unlink_path_w(path_w: &mut Vec<u16>, attrs: u32, posix_supported: &AtomicBool) -> Result<(), u32> {
    if posix_supported.load(Ordering::Relaxed) {
        let h = open_dir_w(path_w, DELETE | SYNCHRONIZE)?;
        let flags = FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | if attrs & FILE_ATTRIBUTE_READONLY != 0 { FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE } else { 0 };
        let res = match set_disposition_ex(h.0, flags) {
            Ok(()) => return Ok(()),
            Err(87) if (flags & FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE) != 0 => {
                clear_readonly_w(path_w, attrs, attrs & A_DIR != 0);
                set_disposition_ex(h.0, FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS)
            }
            Err(e) => Err(e),
        };
        match res {
            Ok(()) => return Ok(()),
            Err(50 | 1) => {
                posix_supported.store(false, Ordering::Relaxed);
            }
            Err(87) => {}
            Err(e) => return Err(e),
        }
    }
    win32_unlink_w(path_w, attrs)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileDelOutcome {
    Deleted,
    SkippedLinked,
    Failed(u32),
}

fn delete_file_entry(
    dir_h: Option<HANDLE>,
    parent_w: &[u16],
    name_w: &[u16],
    attrs: u32,
    posix_supported: &AtomicBool,
    scratch_w: &mut Vec<u16>,
) -> FileDelOutcome {
    let ensure_path = |sw: &mut Vec<u16>| {
        if dir_h.is_some() {
            build_child_w(parent_w, name_w, sw);
        } else {
            sw.clear();
            sw.extend_from_slice(parent_w);
        }
    };
    let is_ro_regular = (attrs & (A_DIR | A_REPARSE)) == 0 && (attrs & FILE_ATTRIBUTE_READONLY) != 0;
    if is_ro_regular {
        let open_res = match dir_h {
            Some(dh) => open_relative_opts(dh, name_w, DELETE | FILE_READ_ATTRIBUTES, Some(true)),
            None => {
                ensure_path(scratch_w);
                open_dir_w(scratch_w, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            }
        };
        match open_res {
            Ok(h) => {
                let mut std_buf = [0u64; 3];
                let std_info = as_bytes_mut(&mut std_buf);
                if !info(h.0, FILE_STANDARD_INFO_CLASS, std_info) || u32_at(std_info, 16) > 1 {
                    return FileDelOutcome::SkippedLinked;
                }
                if posix_supported.load(Ordering::Relaxed) {
                    let res = match set_disposition_ex(
                        h.0,
                        FILE_DISPOSITION_FLAG_DELETE
                            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
                    ) {
                        Ok(()) => return FileDelOutcome::Deleted,
                        Err(87) => {
                            ensure_path(scratch_w);
                            clear_readonly_w(scratch_w, attrs, false);
                            set_disposition_ex(h.0, FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS)
                        }
                        Err(e) => Err(e),
                    };
                    match res {
                        Ok(()) => return FileDelOutcome::Deleted,
                        Err(50 | 1) => posix_supported.store(false, Ordering::Relaxed),
                        Err(87) => {}
                        Err(e) => return FileDelOutcome::Failed(e),
                    }
                }
                drop(h);
                ensure_path(scratch_w);
                return match win32_unlink_w(scratch_w, attrs) {
                    Ok(()) => FileDelOutcome::Deleted,
                    Err(e) => FileDelOutcome::Failed(e),
                };
            }
            Err(open_err) => {
                if open_err == 32 || open_err == 5 {
                    let probe = match dir_h {
                        Some(dh) => open_relative_opts(dh, name_w, FILE_READ_ATTRIBUTES, Some(true)),
                        None => {
                            ensure_path(scratch_w);
                            open_dir_w(scratch_w, FILE_READ_ATTRIBUTES | SYNCHRONIZE)
                        }
                    };
                    let mut std_buf = [0u64; 3];
                    let std_info = as_bytes_mut(&mut std_buf);
                    let linked_or_unknown = match probe {
                        Ok(ph) => !info(ph.0, FILE_STANDARD_INFO_CLASS, std_info) || u32_at(std_info, 16) > 1,
                        Err(_) => true,
                    };
                    if linked_or_unknown {
                        return FileDelOutcome::SkippedLinked;
                    }
                }
                return FileDelOutcome::Failed(open_err);
            }
        }
    }

    if posix_supported.load(Ordering::Relaxed) {
        let non_dir = if attrs & A_REPARSE != 0 { None } else { Some(true) };
        let open_res = match dir_h {
            Some(dh) => open_relative_opts(dh, name_w, DELETE, non_dir),
            None => {
                ensure_path(scratch_w);
                open_dir_w(scratch_w, DELETE | SYNCHRONIZE)
            }
        };
        match open_res {
            Ok(h) => {
                let flags = FILE_DISPOSITION_FLAG_DELETE
                    | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                    | if attrs & FILE_ATTRIBUTE_READONLY != 0 { FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE } else { 0 };
                let res = match set_disposition_ex(h.0, flags) {
                    Ok(()) => return FileDelOutcome::Deleted,
                    Err(87) if (flags & FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE) != 0 => {
                        ensure_path(scratch_w);
                        clear_readonly_w(scratch_w, attrs, attrs & A_DIR != 0);
                        set_disposition_ex(h.0, FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS)
                    }
                    Err(e) => Err(e),
                };
                match res {
                    Ok(()) => return FileDelOutcome::Deleted,
                    Err(50 | 1) => posix_supported.store(false, Ordering::Relaxed),
                    Err(87) => {}
                    Err(e) => return FileDelOutcome::Failed(e),
                }
            }
            Err(open_err) => return FileDelOutcome::Failed(open_err),
        }
    }

    ensure_path(scratch_w);
    match win32_unlink_w(scratch_w, attrs) {
        Ok(()) => FileDelOutcome::Deleted,
        Err(e) => FileDelOutcome::Failed(e),
    }
}

fn is_dir_empty_w(dir_w: &mut Vec<u16>, dbuf: &mut [u64]) -> bool {
    let Ok(h) = open_dir_w(dir_w, FILE_LIST_DIRECTORY | SYNCHRONIZE) else {
        return false;
    };
    let byte_len = dbuf.len() * 8;
    // SAFETY: `dbuf` is valid for `byte_len` bytes.
    let dbuf_bytes = unsafe { std::slice::from_raw_parts_mut(dbuf.as_mut_ptr() as *mut u8, byte_len) };
    let mut name_u16 = Vec::new();
    loop {
        match info_res(h.0, FileFullDirectoryInfo, dbuf_bytes) {
            Ok(()) => {
                let mut o = 0usize;
                loop {
                    if o + 68 > dbuf_bytes.len() {
                        break;
                    }
                    let b = &dbuf_bytes[o..];
                    let next = u32_at(b, 0) as usize;
                    let nl = (u32_at(b, 60) as usize).min(b.len() - 68);
                    let wlen = nl / 2;
                    let name_ptr = b[68..].as_ptr();
                    let name_w: &[u16] = if (name_ptr as usize) & 1 == 0 {
                        // SAFETY: `name_ptr` is 2-byte aligned and `wlen * 2 <= b.len() - 68`.
                        unsafe { std::slice::from_raw_parts(name_ptr as *const u16, wlen) }
                    } else {
                        name_u16.clear();
                        for c in b[68..68 + wlen * 2].as_chunks::<2>().0 {
                            name_u16.push(u16::from_le_bytes(*c));
                        }
                        &name_u16
                    };
                    let is_dot = wlen == 0 || matches!(name_w, [0x002E] | [0x002E, 0x002E]);
                    if !is_dot {
                        return false;
                    }
                    if next == 0 {
                        break;
                    }
                    o += next;
                }
            }
            Err(ERROR_NO_MORE_FILES) => return true,
            Err(_) => return false,
        }
    }
}

struct DirNode {
    path_w: Vec<u16>,
    attrs: u32,
    parent: Option<Arc<DirNode>>,
    pending_children: AtomicUsize,
    child_kept: AtomicBool,
}

struct DelQueue {
    global: Mutex<VecDeque<Arc<DirNode>>>,
    locals: Vec<Mutex<Vec<Arc<DirNode>>>>,
    cv: Condvar,
    pending: AtomicUsize,
    aborted: AtomicBool,
}

struct DelWorkerGuard<'a> {
    q: &'a DelQueue,
}

impl Drop for DelWorkerGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.q.aborted.store(true, Ordering::Release);
            let _g = self.q.global.lock();
            self.q.cv.notify_all();
        }
    }
}

struct DelPendingGuard<'a> {
    q: &'a DelQueue,
}

impl Drop for DelPendingGuard<'_> {
    fn drop(&mut self) {
        if self.q.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _g = self.q.global.lock();
            self.q.cv.notify_all();
        }
    }
}

struct DelEntry {
    name_off: u32,
    name_len: u16,
    attrs: u32,
    eof: u64,
}

struct DelScratch {
    dbuf: Vec<u64>,
    name_u16: Vec<u16>,
    names_w: Vec<u16>,
    entries: Vec<DelEntry>,
    subdirs: Vec<Arc<DirNode>>,
    path_w: Vec<u16>,
}

impl DelScratch {
    fn new() -> Self {
        Self {
            dbuf: vec![0u64; 16_384],
            name_u16: Vec::new(),
            names_w: Vec::new(),
            entries: Vec::new(),
            subdirs: Vec::new(),
            path_w: Vec::new(),
        }
    }
}

fn enum_dir_for_delete(node: &Arc<DirNode>, scratch: &mut DelScratch, acc: &mut DeleteStats) -> Option<Handle> {
    scratch.names_w.clear();
    scratch.entries.clear();
    scratch.subdirs.clear();
    scratch.path_w.clear();
    scratch.path_w.extend_from_slice(&node.path_w);
    let h = match open_dir_w(&mut scratch.path_w, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE) {
        Ok(h) => h,
        Err(err) => {
            node.child_kept.store(true, Ordering::Release);
            acc.push_fail_w(&node.path_w, err);
            return None;
        }
    };
    let byte_len = scratch.dbuf.len() * 8;
    loop {
        // SAFETY: `scratch.dbuf` is valid for `byte_len` bytes.
        let dbuf_bytes = unsafe { std::slice::from_raw_parts_mut(scratch.dbuf.as_mut_ptr() as *mut u8, byte_len) };
        if let Err(err) = info_res(h.0, FileFullDirectoryInfo, dbuf_bytes) {
            if err != ERROR_NO_MORE_FILES {
                node.child_kept.store(true, Ordering::Release);
                acc.push_fail_w(&node.path_w, err);
            }
            break;
        }
        let mut o = 0usize;
        loop {
            if o + 68 > dbuf_bytes.len() {
                break;
            }
            let b = &dbuf_bytes[o..];
            let next = u32_at(b, 0) as usize;
            let eof = i64_at(b, 40).max(0) as u64;
            let attrs = u32_at(b, 56);
            let nl = (u32_at(b, 60) as usize).min(b.len() - 68);
            let wlen = nl / 2;
            let name_ptr = b[68..].as_ptr();
            let name_w: &[u16] = if (name_ptr as usize) & 1 == 0 {
                // SAFETY: `name_ptr` is 2-byte aligned and `wlen * 2 <= b.len() - 68`.
                unsafe { std::slice::from_raw_parts(name_ptr as *const u16, wlen) }
            } else {
                scratch.name_u16.clear();
                for c in b[68..68 + wlen * 2].as_chunks::<2>().0 {
                    scratch.name_u16.push(u16::from_le_bytes(*c));
                }
                &scratch.name_u16
            };
            let is_dot = wlen == 0 || matches!(name_w, [0x002E] | [0x002E, 0x002E]);
            if !is_dot {
                if attrs & A_DIR != 0 && attrs & A_REPARSE == 0 {
                    build_child_w(&node.path_w, name_w, &mut scratch.path_w);
                    scratch.subdirs.push(Arc::new(DirNode {
                        path_w: std::mem::take(&mut scratch.path_w),
                        attrs,
                        parent: Some(node.clone()),
                        pending_children: AtomicUsize::new(1),
                        child_kept: AtomicBool::new(false),
                    }));
                } else {
                    let off = scratch.names_w.len() as u32;
                    let len = u16::try_from(name_w.len()).unwrap_or(u16::MAX);
                    scratch.names_w.extend_from_slice(&name_w[..len as usize]);
                    scratch.entries.push(DelEntry { name_off: off, name_len: len, attrs, eof });
                }
            }
            if next == 0 {
                break;
            }
            o += next;
        }
    }
    Some(h)
}

fn delete_dir_entries(
    dir_h: HANDLE,
    node: &Arc<DirNode>,
    posix_supported: &AtomicBool,
    scratch: &mut DelScratch,
    acc: &mut DeleteStats,
) {
    for idx in 0..scratch.entries.len() {
        let ent = &scratch.entries[idx];
        let start = ent.name_off as usize;
        let end = start + ent.name_len as usize;
        let attrs = ent.attrs;
        let eof = ent.eof;
        let outcome = delete_file_entry(
            Some(dir_h),
            &node.path_w,
            &scratch.names_w[start..end],
            attrs,
            posix_supported,
            &mut scratch.path_w,
        );
        if attrs & A_REPARSE != 0 {
            match outcome {
                FileDelOutcome::Deleted => acc.links += 1,
                FileDelOutcome::SkippedLinked | FileDelOutcome::Failed(_) => {
                    let err = match outcome {
                        FileDelOutcome::Failed(e) => e,
                        _ => 5,
                    };
                    node.child_kept.store(true, Ordering::Release);
                    build_child_w(&node.path_w, &scratch.names_w[start..end], &mut scratch.path_w);
                    acc.push_fail_w(&scratch.path_w, err);
                }
            }
        } else {
            match outcome {
                FileDelOutcome::Deleted => {
                    acc.files += 1;
                    acc.bytes += eof;
                }
                FileDelOutcome::SkippedLinked => {
                    node.child_kept.store(true, Ordering::Release);
                    acc.linked += 1;
                    acc.linked_bytes += eof;
                }
                FileDelOutcome::Failed(err) => {
                    node.child_kept.store(true, Ordering::Release);
                    build_child_w(&node.path_w, &scratch.names_w[start..end], &mut scratch.path_w);
                    acc.push_fail_w(&scratch.path_w, err);
                }
            }
        }
    }
}

fn release_dir_node(
    mut node: Arc<DirNode>,
    posix_supported: &AtomicBool,
    scratch: &mut DelScratch,
    acc: &mut DeleteStats,
) {
    loop {
        if node.pending_children.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let mut kept = node.child_kept.load(Ordering::Acquire);
        if !kept {
            scratch.path_w.clear();
            scratch.path_w.extend_from_slice(&node.path_w);
            match unlink_path_w(&mut scratch.path_w, node.attrs, posix_supported) {
                Ok(()) => acc.dirs += 1,
                Err(err) => {
                    kept = true;
                    scratch.path_w.clear();
                    scratch.path_w.extend_from_slice(&node.path_w);
                    if is_dir_empty_w(&mut scratch.path_w, &mut scratch.dbuf) {
                        acc.push_fail_w(&node.path_w, err);
                    }
                }
            }
        }
        let Some(parent) = node.parent.clone() else {
            return;
        };
        if kept {
            parent.child_kept.store(true, Ordering::Release);
        }
        node = parent;
    }
}

fn delete_worker_loop(
    wid: usize,
    nthreads: usize,
    q: &DelQueue,
    posix_supported: &AtomicBool,
    scratch: &mut DelScratch,
    acc: &mut DeleteStats,
) {
    loop {
        if q.aborted.load(Ordering::Acquire) {
            break;
        }
        let mut job = q.locals[wid].lock().unwrap_or_else(|p| p.into_inner()).pop();
        if job.is_none() {
            job = q.global.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
        }
        if job.is_none() && nthreads > 1 {
            for off in 1..nthreads {
                let victim = (wid + off) % nthreads;
                if let Ok(mut v) = q.locals[victim].try_lock() {
                    if let Some(stolen) = v.pop() {
                        job = Some(stolen);
                        break;
                    }
                }
            }
        }
        let node = match job {
            Some(j) => j,
            None => {
                let mut g = q.global.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(j) = g.pop_front() {
                    j
                } else if q.aborted.load(Ordering::Acquire) || q.pending.load(Ordering::SeqCst) == 0 {
                    break;
                } else {
                    g = q.cv.wait_timeout(g, Duration::from_millis(20)).unwrap_or_else(|p| p.into_inner()).0;
                    if let Some(j) = g.pop_front() {
                        j
                    } else if q.aborted.load(Ordering::Acquire) || q.pending.load(Ordering::SeqCst) == 0 {
                        break;
                    } else {
                        drop(g);
                        continue;
                    }
                }
            }
        };
        let _job_guard = DelPendingGuard { q };
        if let Some(dir_h) = enum_dir_for_delete(&node, scratch, acc) {
            if !scratch.subdirs.is_empty() {
                let added = scratch.subdirs.len();
                node.pending_children.fetch_add(added, Ordering::AcqRel);
                q.pending.fetch_add(added, Ordering::SeqCst);
                let mut g = q.global.lock().unwrap_or_else(|p| p.into_inner());
                let keep_local = scratch.subdirs.pop().unwrap();
                g.extend(scratch.subdirs.drain(..));
                q.cv.notify_all();
                drop(g);
                q.locals[wid].lock().unwrap_or_else(|p| p.into_inner()).push(keep_local);
            }
            delete_dir_entries(dir_h.0, &node, posix_supported, scratch, acc);
            drop(dir_h);
        }
        release_dir_node(node, posix_supported, scratch, acc);
    }
}

/// Delete `root` and everything inside it using `\\?\` extended paths, multi-threaded bottom-up
/// traversal, and POSIX-semantics `FileDispositionInfoEx` with `DeleteFileW`/`RemoveDirectoryW` fallback.
/// Reparse points (junctions and symlinks) are never followed; read-only hard-linked files are preserved.
pub fn delete_tree(root: &std::path::Path) -> DeleteStats {
    delete_tree_with_opts(root, true)
}

fn delete_tree_with_opts(root: &std::path::Path, posix: bool) -> DeleteStats {
    let mut r = DeleteStats::default();
    let mut root_w = long_wide(root);
    if root_w.is_empty() {
        r.failed_count = 1;
        r.failed.push((root.display().to_string(), "empty path".into()));
        return r;
    }
    let posix_supported = AtomicBool::new(posix);

    let probe_h = match open_dir_w(&mut root_w, FILE_READ_ATTRIBUTES | SYNCHRONIZE) {
        Ok(h) => h,
        Err(err) => {
            r.push_fail_w(&root_w, err);
            r.remains = exists_no_follow_w(&mut root_w);
            return r;
        }
    };
    let mut basic_buf = [0u64; 5];
    let basic = as_bytes_mut(&mut basic_buf);
    if !info(probe_h.0, FILE_BASIC_INFO_CLASS, basic) {
        // SAFETY: plain call.
        let err = unsafe { GetLastError() };
        drop(probe_h);
        r.push_fail_w(&root_w, err);
        r.remains = exists_no_follow_w(&mut root_w);
        return r;
    }
    let attrs = u32_at(basic, 32);
    let mut std_buf = [0u64; 3];
    let std_info = as_bytes_mut(&mut std_buf);
    let eof = if attrs & A_DIR == 0 && info(probe_h.0, FILE_STANDARD_INFO_CLASS, std_info) {
        i64_at(std_info, 8).max(0) as u64
    } else {
        0
    };
    drop(probe_h);

    if attrs & A_REPARSE != 0 {
        match unlink_path_w(&mut root_w, attrs, &posix_supported) {
            Ok(()) => r.links += 1,
            Err(err) => r.push_fail_w(&root_w, err),
        }
        r.remains = exists_no_follow_w(&mut root_w);
        return r;
    }

    if attrs & A_DIR == 0 {
        let mut scratch_w = Vec::new();
        match delete_file_entry(None, &root_w, &[], attrs, &posix_supported, &mut scratch_w) {
            FileDelOutcome::Deleted => {
                r.files += 1;
                r.bytes += eof;
            }
            FileDelOutcome::SkippedLinked => {
                r.linked += 1;
                r.linked_bytes += eof;
            }
            FileDelOutcome::Failed(err) => r.push_fail_w(&root_w, err),
        }
        r.remains = exists_no_follow_w(&mut root_w);
        return r;
    }

    let root_node = Arc::new(DirNode {
        path_w: root_w.clone(),
        attrs,
        parent: None,
        pending_children: AtomicUsize::new(1),
        child_kept: AtomicBool::new(false),
    });
    let mut scratch = DelScratch::new();
    let Some(root_dir_h) = enum_dir_for_delete(&root_node, &mut scratch, &mut r) else {
        r.remains = exists_no_follow_w(&mut root_w);
        return r;
    };

    if scratch.subdirs.is_empty() {
        delete_dir_entries(root_dir_h.0, &root_node, &posix_supported, &mut scratch, &mut r);
        drop(root_dir_h);
        release_dir_node(root_node, &posix_supported, &mut scratch, &mut r);
        r.remains = exists_no_follow_w(&mut root_w);
        return r;
    }

    let sub_count = scratch.subdirs.len();
    root_node.pending_children.fetch_add(sub_count, Ordering::AcqRel);
    let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
    let q = Arc::new(DelQueue {
        global: Mutex::new(VecDeque::from(std::mem::take(&mut scratch.subdirs))),
        locals: (0..nthreads).map(|_| Mutex::new(Vec::new())).collect(),
        cv: Condvar::new(),
        pending: AtomicUsize::new(sub_count),
        aborted: AtomicBool::new(false),
    });

    let worker_stats: Vec<DeleteStats> = std::thread::scope(|s| {
        let mut hs = Vec::with_capacity(nthreads.saturating_sub(1));
        for wid in 1..nthreads {
            let q = q.clone();
            let posix_ref = &posix_supported;
            hs.push(s.spawn(move || {
                let _guard = DelWorkerGuard { q: &q };
                let mut w_scratch = DelScratch::new();
                let mut w_acc = DeleteStats::default();
                delete_worker_loop(wid, nthreads, &q, posix_ref, &mut w_scratch, &mut w_acc);
                w_acc
            }));
        }
        {
            let _guard = DelWorkerGuard { q: &q };
            delete_dir_entries(root_dir_h.0, &root_node, &posix_supported, &mut scratch, &mut r);
            drop(root_dir_h);
            release_dir_node(root_node, &posix_supported, &mut scratch, &mut r);
            delete_worker_loop(0, nthreads, &q, &posix_supported, &mut scratch, &mut r);
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for ws in worker_stats {
        r.merge_from(ws);
    }
    r.remains = exists_no_follow_w(&mut root_w);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_normalizes_slashes_dots_and_unc() {
        assert_eq!(long(""), "");
        assert_eq!(long("D:/a/b"), r"\\?\D:\a\b");
        assert_eq!(long("D:/"), r"\\?\D:\");
        assert_eq!(long(r"C:\a\\b/c"), r"\\?\C:\a\b\c");
        assert_eq!(long(r"C:/a/../b"), r"\\?\C:\b");
        assert_eq!(long(r"//server/share/dir"), r"\\?\UNC\server\share\dir");
        assert_eq!(long(r"\\server\share\dir"), r"\\?\UNC\server\share\dir");
        assert_eq!(long(r"\\?\C:/a//b"), r"\\?\C:\a\b");
        assert_eq!(long(r"\\?\UNC\server/share//a"), r"\\?\UNC\server\share\a");
        assert_eq!(long(r"\\.\PhysicalDrive0"), r"\\.\PhysicalDrive0");
        assert_eq!(long(r"\\?\Volume{abc}\"), r"\\?\Volume{abc}\");
        let rel = long("foo/bar");
        assert!(rel.starts_with(r"\\?\"), "{}", rel);
        assert!(rel.ends_with(r"\foo\bar"), "{}", rel);
        assert!(!rel.contains('/'), "{}", rel);

        // Live FS check using forward slashes.
        let root = std::env::temp_dir().join(format!("amg-win-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let fwd = root.display().to_string().replace('\\', "/");
        assert!(identity(&fwd).is_some());
        let vs = VolumeScan {
            cluster: 4096,
            block_cloning: false,
            level2: false,
            threads: 1,
            deadline: Instant::now() + Duration::from_secs(10),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            multi: Mutex::new(HashMap::new()),
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (st, _) = walk_item(&vs, 0, &[fwd], &[]);
        assert_eq!(st.files, 1);
        assert_eq!(st.dir_failures, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cluster_size_returns_none_on_invalid_root_instead_of_512() {
        assert_eq!(cluster_size(r"\\?\Volume{00000000-0000-0000-0000-000000000000}\"), None);
        let tmp = std::env::temp_dir();
        let (root, _) = volume_of(&tmp.display().to_string()).unwrap();
        let c = cluster_size(&root).expect("temp volume has cluster size");
        assert!(c >= 512, "unexpected cluster size {}", c);
    }

    #[test]
    fn walk_item_includes_root_last_write_time_and_empty_dir_has_no_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-win-rootlw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let empty = root.join("empty");
        let sub = root.join("sub");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("child.bin"), b"1234").unwrap();

        let old = std::time::SystemTime::now() - Duration::from_secs(5 * 3600);
        let newer_root = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        let set_mtime = |p: &std::path::Path, t: std::time::SystemTime| {
            let f = std::fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(p).unwrap();
            f.set_modified(t).unwrap();
        };
        set_mtime(&sub.join("child.bin"), old);
        set_mtime(&sub, newer_root);
        set_mtime(&empty, newer_root);

        let vs = VolumeScan {
            cluster: 4096,
            block_cloning: false,
            level2: false,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            multi: Mutex::new(HashMap::new()),
            progress_files: Arc::new(AtomicU64::new(0)),
        };

        let (st_empty, _) = walk_item(&vs, 0, &[empty.display().to_string()], &[]);
        assert_eq!(st_empty.dir_failures, 0);
        assert!(st_empty.last_write > 0);

        let (st_sub, _) = walk_item(&vs, 0, &[sub.display().to_string()], &[]);
        assert_eq!(st_sub.dir_failures, 0);
        assert_eq!(st_sub.files, 1);
        assert_eq!(st_sub.last_write, root_last_write(&sub.display().to_string()).unwrap());
        assert!(st_sub.last_write > root_last_write(&sub.join("child.bin").display().to_string()).unwrap());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attribute_multilinks_preserves_raw_rc_for_block_cloning_and_synthetic_otherwise() {
        let mut multi = HashMap::new();
        let mut names = HashMap::new();
        names.insert(0, 1);
        names.insert(1, 1);
        multi.insert(
            42u128,
            MultiLink {
                links: 2,
                first_item: 0,
                names,
                alloc_clusters: 10,
                exts: vec![Ext { lcn: 100, len: 10, rc: 2 }],
                ..Default::default()
            },
        );
        let mut exts = vec![
            ItemExtents::default(),
            ItemExtents::default(),
            ItemExtents { exclusive_clusters: 0, shared: vec![Ext { lcn: 100, len: 10, rc: 2 }] },
        ];
        let linked = attribute_multilinks(multi.clone(), &mut exts, true);
        assert!(exts[0].shared.is_empty());
        assert!(exts[1].shared.is_empty());
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].links, 2);
        assert_eq!(linked[0].exts, vec![Ext { lcn: 100, len: 10, rc: 2 }]);
        let sw = crate::disk_space::sweep_with_links(&exts, &linked);
        assert_eq!(sw.inconsistent, 0);
        assert_eq!(sw.anomalies, 0);
        assert_eq!(sw.frees_together(&[0, 1]), 0);
        assert_eq!(sw.frees_together(&[0, 1, 2]), 10);

        let mut exts_ntfs = vec![ItemExtents::default(), ItemExtents::default()];
        let linked_ntfs = attribute_multilinks(multi, &mut exts_ntfs, false);
        assert!(linked_ntfs.is_empty());
        assert_eq!(exts_ntfs[0].shared.len(), 1);
        assert_eq!(exts_ntfs[1].shared.len(), 1);
        assert_eq!(exts_ntfs[0].shared[0].rc, 2);
        assert_eq!(exts_ntfs[0].shared[0].len, 10);
        assert!(exts_ntfs[0].shared[0].lcn >= (1u64 << 62));
    }

    #[test]
    fn attribute_multilinks_deducts_promoted_exclusive_and_splits_u32_max() {
        let big = u32::MAX as u64 + 12_345;
        let mut multi = HashMap::new();
        let mut names = HashMap::new();
        names.insert(0, 1);
        names.insert(1, 1);
        multi.insert(
            99u128,
            MultiLink {
                links: 2,
                first_item: 0,
                names,
                alloc_clusters: big,
                first_alloc: big,
                exts: Vec::new(),
                promoted_from_exclusive: true,
            },
        );
        let mut exts = vec![
            ItemExtents { exclusive_clusters: big + 50, shared: Vec::new() },
            ItemExtents::default(),
        ];
        let linked = attribute_multilinks(multi, &mut exts, false);
        assert!(linked.is_empty());
        assert_eq!(exts[0].exclusive_clusters, 50);
        assert_eq!(exts[0].shared.len(), 2);
        assert_eq!(exts[0].shared[0].len, u32::MAX);
        assert_eq!(exts[0].shared[1].len, 12_345);
        assert_eq!(exts[0].shared[0].len as u64 + exts[0].shared[1].len as u64, big);
        let sw = crate::disk_space::sweep(&exts);
        assert_eq!(sw.items[0].frees_alone, 50);
        assert_eq!(sw.items[1].frees_alone, 0);
        assert_eq!(sw.frees_together(&[0, 1]), big + 50);

        // Concurrent growth between sighting 1 (first_alloc = 10) and sighting 2 (alloc_clusters = 25):
        // must only deduct first_alloc (10) from item 0's exclusive_clusters, preserving other exclusive files.
        let mut multi_grow = HashMap::new();
        let mut names_grow = HashMap::new();
        names_grow.insert(0, 1);
        names_grow.insert(1, 1);
        multi_grow.insert(
            100u128,
            MultiLink {
                links: 2,
                first_item: 0,
                names: names_grow,
                alloc_clusters: 25,
                first_alloc: 10,
                exts: Vec::new(),
                promoted_from_exclusive: true,
            },
        );
        let mut exts_grow = vec![
            ItemExtents { exclusive_clusters: 10 + 50, shared: Vec::new() },
            ItemExtents::default(),
        ];
        attribute_multilinks(multi_grow, &mut exts_grow, false);
        assert_eq!(exts_grow[0].exclusive_clusters, 50);
        let sw_grow = crate::disk_space::sweep(&exts_grow);
        assert_eq!(sw_grow.items[0].frees_alone, 50);
        assert_eq!(sw_grow.frees_together(&[0, 1]), 50 + 25);
    }

    #[test]
    fn open_relative_handles_long_paths_unicode_and_reparse_files() {
        let root = std::env::temp_dir().join(format!("amg-win-rel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Deep >260 char path + Unicode filename.
        let deep = root
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .join("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
            .join("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
        let deep_long = long(&deep.display().to_string());
        std::fs::create_dir_all(&deep_long).unwrap();
        let ufile = "тест_αβγ_🚀.bin";
        let fpath = format!(r"{}\{}", deep_long, ufile);
        std::fs::write(&fpath, vec![1u8; 8192]).unwrap();

        let mut dir_w: Vec<u16> = deep_long.encode_utf16().collect();
        let dir_h = open_dir_w(&mut dir_w, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE).unwrap();
        let file_w: Vec<u16> = ufile.encode_utf16().collect();
        let fh = open_relative(dir_h.0, &file_w, FILE_READ_ATTRIBUTES).unwrap();
        let mut std_info = [0u8; 24];
        assert!(info(fh.0, FILE_STANDARD_INFO_CLASS, &mut std_info));
        assert_eq!(u32_at(&std_info, 16), 1);
        let mut exts = Vec::new();
        let mut ebuf = vec![0u8; 65_536];
        let (_rc_ok, err) = extents(fh.0, &mut ebuf, false, &mut exts);
        assert_eq!(err, 0);
        assert!(!exts.is_empty());

        // Missing relative entry returns an error cleanly.
        let missing: Vec<u16> = "does_not_exist.bin".encode_utf16().collect();
        assert!(open_relative(dir_h.0, &missing, FILE_READ_ATTRIBUTES).is_err());

        // File symlink (if privileges allow creating one) is opened with FILE_OPEN_REPARSE_POINT (not followed).
        let target = format!(r"{}\target.bin", deep_long);
        std::fs::write(&target, vec![2u8; 65_536]).unwrap();
        let link = format!(r"{}\sym.bin", deep_long);
        if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
            let sym_w: Vec<u16> = "sym.bin".encode_utf16().collect();
            let sh = open_relative(dir_h.0, &sym_w, FILE_READ_ATTRIBUTES).unwrap();
            let mut basic = [0u8; 40];
            assert!(info(sh.0, FILE_BASIC_INFO_CLASS, &mut basic));
            assert_ne!(u32_at(&basic, 32) & A_REPARSE, 0);
        }

        drop(fh);
        drop(dir_h);
        let _ = std::fs::remove_dir_all(&long(&root.display().to_string()));
    }

    #[test]
    fn walk_volume_dedups_cross_item_zero_byte_and_external_hard_links_and_respects_skip() {
        let root = std::env::temp_dir().join(format!("amg-win-wv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let i0 = root.join("item0");
        let i0_skip = i0.join("nested_skip");
        let i0_skip2 = i0.join("nested_skip2");
        let i1 = root.join("item1");
        let ext_dir = root.join("outside");
        std::fs::create_dir_all(&i0_skip).unwrap();
        std::fs::create_dir_all(&i0_skip2).unwrap();
        std::fs::create_dir_all(&i1).unwrap();
        std::fs::create_dir_all(&ext_dir).unwrap();

        // 0-byte hard-linked pair inside item0.
        std::fs::write(i0.join("z1.empty"), b"").unwrap();
        std::fs::hard_link(i0.join("z1.empty"), i0.join("z2.empty")).unwrap();

        // Cross-item hard-linked file (1 link in item0, 1 in item1, 1 outside -> links=3).
        std::fs::write(i0.join("shared.bin"), vec![5u8; 65_536]).unwrap();
        std::fs::hard_link(i0.join("shared.bin"), i1.join("shared.bin")).unwrap();
        std::fs::hard_link(i0.join("shared.bin"), ext_dir.join("shared.bin")).unwrap();

        // File inside nested_skip (must be skipped by item0!) and file inside nested_skip2 (must be counted!).
        std::fs::write(i0_skip.join("inner.bin"), vec![3u8; 16_384]).unwrap();
        std::fs::write(i0_skip2.join("kept.bin"), vec![4u8; 16_384]).unwrap();

        let (_, guid) = volume_of(&root.display().to_string()).unwrap();
        let vf = volume_facts(&guid, Vec::new());
        let cfg = VolumeWalkConfig {
            cluster: vf.cluster.max(4096),
            block_cloning: false,
            supports_hard_links: true,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: None,
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let r0 = [i0.display().to_string()];
        let s0 = [crate::disk_space::norm(&i0_skip.display().to_string())];
        let r1 = [i1.display().to_string()];
        let (res, linked) = walk_volume(
            &cfg,
            &[WalkItemInput { roots: &r0, skip: &s0 }, WalkItemInput { roots: &r1, skip: &[] }],
        );
        assert!(linked.is_empty());
        // item0 saw: z1.empty, z2.empty, shared.bin, kept.bin -> l1.files == 4
        assert_eq!(res[0].level1.files, 4);
        let l2_0 = res[0].level2.as_ref().unwrap();
        let l2_1 = res[1].level2.as_ref().unwrap();
        assert_eq!(l2_0.files + l2_0.hard_links, 4);
        assert_eq!(l2_1.files + l2_1.hard_links, 1);

        let item_exts = vec![res[0].extents.clone(), res[1].extents.clone()];
        let sw = crate::disk_space::sweep(&item_exts);
        // Because shared.bin has an external link (links = 3 > 2 in-scan), deleting both items frees only kept.bin!
        let kept_clusters = res[0].extents.exclusive_clusters;
        assert!(kept_clusters > 0);
        assert_eq!(sw.items[0].frees_alone, kept_clusters);
        assert_eq!(sw.items[1].frees_alone, 0);
        assert_eq!(sw.frees_together(&[0, 1]), kept_clusters);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mid_dir_stop_truncates_cleanly_and_volume_disk_number_queries_system_drive() {
        let root = std::env::temp_dir().join(format!("amg-win-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for idx in 0..150 {
            std::fs::write(root.join(format!("f{:03}.txt", idx)), b"x").unwrap();
        }
        let (_, guid) = volume_of(&root.display().to_string()).unwrap();
        let _disk_num = volume_disk_number(&guid);

        let stop = Arc::new(AtomicBool::new(true));
        let cfg = VolumeWalkConfig {
            cluster: 4096,
            block_cloning: false,
            supports_hard_links: true,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: None,
            stop,
            started_ft: u64::MAX,
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let r0 = [root.display().to_string()];
        let (res, _) = walk_volume(&cfg, &[WalkItemInput { roots: &r0, skip: &[] }]);
        assert!(res[0].level1.cut_short);
        assert!(res[0].not_reached);
        assert!(res[0].level2.is_none());

        // Simulate ReFS extent_deadline cutoff: l1 finishes sizing all files while l2 stays None.
        let cfg_refs_cutoff = VolumeWalkConfig {
            cluster: 4096,
            block_cloning: true,
            supports_hard_links: true,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: Some(Instant::now() - Duration::from_secs(1)),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (res_refs, _) = walk_volume(&cfg_refs_cutoff, &[WalkItemInput { roots: &r0, skip: &[] }]);
        assert!(!res_refs[0].not_reached);
        assert!(!res_refs[0].level1.cut_short);
        assert_eq!(res_refs[0].level1.files, 150);
        assert!(res_refs[0].level2.is_none());

        // Mid-directory stop inside scan_dir: create 1_200 non-empty 4096-byte (non-resident, alloc > 0)
        // files and scan with block_cloning: true so each file performs open_relative + extents syscalls.
        let mid_root = root.join("mid");
        std::fs::create_dir_all(&mid_root).unwrap();
        let payload = vec![0xABu8; 4096];
        for idx in 0..1_200 {
            std::fs::write(mid_root.join(format!("m{:04}.bin", idx)), &payload).unwrap();
        }
        let r_mid = [mid_root.display().to_string()];

        struct StopperGuard {
            exit: Arc<AtomicBool>,
            handle: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for StopperGuard {
            fn drop(&mut self) {
                self.exit.store(true, Ordering::Release);
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }

        let stop2 = Arc::new(AtomicBool::new(false));
        let pf2 = Arc::new(AtomicU64::new(0));
        let exit_stopper = Arc::new(AtomicBool::new(false));
        let stop2_w = stop2.clone();
        let pf2_w = pf2.clone();
        let exit_w = exit_stopper.clone();
        let mut stopper_guard = StopperGuard {
            exit: exit_stopper.clone(),
            handle: Some(std::thread::spawn(move || {
                while pf2_w.load(Ordering::Relaxed) < 32 && !exit_w.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                if !exit_w.load(Ordering::Acquire) {
                    stop2_w.store(true, Ordering::Release);
                }
            })),
        };
        let cfg_mid = VolumeWalkConfig {
            cluster: 4096,
            block_cloning: true,
            supports_hard_links: true,
            level2: true,
            threads: 1,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: None,
            stop: stop2,
            started_ft: u64::MAX,
            progress_files: pf2,
        };
        let (res_mid, _) = walk_volume(&cfg_mid, &[WalkItemInput { roots: &r_mid, skip: &[] }]);
        exit_stopper.store(true, Ordering::Release);
        if let Some(h) = stopper_guard.handle.take() {
            h.join().unwrap();
        }
        assert!(res_mid[0].level1.cut_short);
        assert!(!res_mid[0].not_reached);
        assert!(res_mid[0].level1.files > 0);
        assert!(res_mid[0].level1.files < 1_200);
        assert!(res_mid[0].level2.as_ref().is_some_and(|l| l.cut_short));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_dir_updates_l2_written_during_scan_and_last_write_on_subdir_changes() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-win-subdir-lw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let old_file = sub.join("old.bin");
        std::fs::write(&old_file, b"1234").unwrap();

        let now = std::time::SystemTime::now();
        let old_t = now - Duration::from_secs(5 * 3600);
        let new_t = now - Duration::from_secs(1 * 3600);
        let set_mtime = |p: &std::path::Path, t: std::time::SystemTime| {
            let f = std::fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(p).unwrap();
            f.set_modified(t).unwrap();
        };
        set_mtime(&old_file, old_t);
        set_mtime(&sub, new_t);
        set_mtime(&root, old_t);

        let sub_lw = root_last_write(&sub.display().to_string()).unwrap();
        let old_lw = root_last_write(&old_file.display().to_string()).unwrap();
        let root_lw = root_last_write(&root.display().to_string()).unwrap();
        assert!(sub_lw > old_lw);
        assert!(sub_lw > root_lw);

        let cfg = VolumeWalkConfig {
            cluster: 4096,
            block_cloning: false,
            supports_hard_links: true,
            level2: true,
            threads: 1,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: None,
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: sub_lw,
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let r0 = [root.display().to_string()];
        let (res, _) = walk_volume(&cfg, &[WalkItemInput { roots: &r0, skip: &[] }]);
        assert_eq!(res[0].level1.written_during_scan, 1);
        let l2 = res[0].level2.as_ref().unwrap();
        assert_eq!(l2.written_during_scan, 1);
        assert_eq!(l2.last_write, sub_lw);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_pool_terminates_and_propagates_panic_without_hanging() {
        let root = std::env::temp_dir().join(format!("amg-win-pool-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let lr = long(&root.display().to_string());
        let path_w: Vec<u16> = lr.encode_utf16().collect();

        // `skips` is intentionally empty while `job.item == 0`, triggering an index panic inside
        // `scan_dir` on 1 worker while the remaining 3 workers are waiting on the `Condvar`.
        // `PoolWorkerGuard` + `PendingJobGuard` must wake all siblings and propagate the panic
        // without deadlocking.
        let extents_cutoff = AtomicBool::new(false);
        let stop = Arc::new(AtomicBool::new(false));
        let pf = Arc::new(AtomicU64::new(0));
        let multi: Mutex<FxHashMap<u128, MultiLink>> = Mutex::new(FxHashMap::default());
        let ctx = PoolCtx {
            cluster: 4096,
            block_cloning: false,
            supports_hard_links: true,
            level2: true,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: None,
            extents_cutoff: &extents_cutoff,
            stop: &stop,
            started_ft: u64::MAX,
            progress_files: &pf,
            skips: &[],
            stop_on_repo: false,
            prune_dot_git: false,
            sink: MultiSink::Volume { seen_fids: None, multi: &multi },
        };
        let initial_jobs = vec![DirJob { path_w, item: 0, check_skip: false, is_root: true }];
        let t0 = Instant::now();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_pool(&ctx, initial_jobs, 1, 4);
        }));
        assert!(res.is_err(), "expected panic from out-of-bounds ctx.skips to propagate");
        assert!(t0.elapsed() < Duration::from_secs(5), "run_pool hung instead of waking sibling workers");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_stats_serde_default_and_walk_clean_paths_and_holds_repo_edge_cases() {
        let legacy_json = r#"{"files":1,"dirs":2,"logical":3,"allocated":4,"last_write":5,"reparse_dirs":0,"reparse_files":0,"sparse_files":0,"compressed_files":0,"cloud_files":0,"hard_links":0,"dir_failures":0,"open_failures":0,"extent_failures":0,"files_without_refcount":0,"written_during_scan":0,"cut_short":false}"#;
        let parsed: WalkStats = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.files, 1);
        assert!(!parsed.holds_repo);

        let root = std::env::temp_dir().join(format!("amg-win-holdsrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir_git = root.join("dir_git");
        let file_git = root.join("file_git");
        let upper_git = root.join("upper_git");
        let junc_git = root.join("junc_git");
        let target_repo = root.join("target_repo");
        let clean_with_junc = root.join("clean_with_junc");
        let clean_plain = root.join("clean_plain");

        std::fs::create_dir_all(dir_git.join(r"a\b\.git\objects\00")).unwrap();
        std::fs::write(dir_git.join(r"a\b\.git\objects\00\pack"), b"x").unwrap();
        std::fs::create_dir_all(file_git.join("sub")).unwrap();
        std::fs::write(file_git.join(r"sub\.git"), b"gitdir: ../real").unwrap();
        std::fs::create_dir_all(upper_git.join(r"sub\.GIT")).unwrap();
        std::fs::create_dir_all(target_repo.join(".git")).unwrap();
        std::fs::create_dir_all(&junc_git).unwrap();
        std::fs::create_dir_all(&clean_with_junc).unwrap();
        std::fs::create_dir_all(clean_plain.join("sub")).unwrap();
        std::fs::write(clean_plain.join(r"sub\obj.bin"), b"ok").unwrap();

        let ok1 = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(junc_git.join(".git"))
            .arg(&target_repo)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let ok2 = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(clean_with_junc.join("link_not_git"))
            .arg(&target_repo)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let dl = Instant::now() + Duration::from_secs(10);
        assert_eq!(holds_repo(&dir_git.display().to_string(), dl), Some(true));
        assert_eq!(holds_repo(&file_git.display().to_string(), dl), Some(true));
        assert_eq!(holds_repo(&upper_git.display().to_string(), dl), Some(true));
        if ok1 {
            assert_eq!(holds_repo(&junc_git.display().to_string(), dl), Some(true));
        }
        if ok2 {
            // Junction NOT named .git pointing to a repo must NOT be followed.
            assert_eq!(holds_repo(&clean_with_junc.display().to_string(), dl), Some(false));
            // Junction root itself must NOT be followed.
            assert_eq!(holds_repo(&clean_with_junc.join("link_not_git").display().to_string(), dl), Some(false));
        }
        assert_eq!(holds_repo(&clean_plain.display().to_string(), dl), Some(false));
        assert_eq!(holds_repo(&dir_git.display().to_string(), Instant::now() - Duration::from_secs(1)), None);
        // Regular file passed as root cannot enumerate -> None.
        assert_eq!(holds_repo(&clean_plain.join(r"sub\obj.bin").display().to_string(), dl), None);

        // Multi-path walk_clean_paths sets holds_repo only on the offending path.
        let paths = vec![clean_plain.display().to_string(), dir_git.display().to_string()];
        let stats = walk_clean_paths(&paths, 4, dl, true);
        assert_eq!(stats.len(), 2);
        assert!(!stats[0].holds_repo && stats[0].dir_failures == 0 && !stats[0].cut_short);
        assert!(stats[1].holds_repo);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_tree_handles_long_paths_unicode_and_posix_open_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-win-del-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(long(&root.display().to_string()));
        let deep = root
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .join("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
            .join("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
        let deep_long = long(&deep.display().to_string());
        std::fs::create_dir_all(&deep_long).unwrap();
        let ufile = format!(r"{}\тест_αβγ.bin", deep_long);
        std::fs::write(&ufile, b"123456").unwrap();

        let fwd_root = std::path::PathBuf::from(root.display().to_string().replace('\\', "/"));
        let rep = delete_tree(&fwd_root);
        assert!(!rep.remains, "{:?}", rep);
        assert_eq!(rep.failed_count, 0, "{:?}", rep);
        assert_eq!(rep.files, 1);
        assert_eq!(rep.bytes, 6);
        assert!(rep.dirs >= 5);

        // POSIX unlink of a file held open with FILE_SHARE_DELETE (share_mode(7)):
        // both the file and its parent folder must be unlinked immediately.
        let posix_dir = root.join("posix_open");
        std::fs::create_dir_all(&posix_dir).unwrap();
        let open_file = posix_dir.join("shared.bin");
        std::fs::write(&open_file, b"hello-posix").unwrap();
        let holder = std::fs::OpenOptions::new().read(true).share_mode(7).open(&open_file).unwrap();
        let rep_posix = delete_tree(&posix_dir);
        assert!(!rep_posix.remains, "POSIX delete must unlink open file + dir: {:?}", rep_posix);
        assert_eq!((rep_posix.files, rep_posix.dirs, rep_posix.failed_count), (1, 1, 0), "{:?}", rep_posix);
        assert_eq!(rep_posix.bytes, 11);
        drop(holder);

        let _ = std::fs::remove_dir_all(long(&root.display().to_string()));
    }

    #[test]
    fn delete_tree_locked_file_no_deadlock_and_fallback_posix_false() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-win-del-fb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Fallback (posix = false) test covering readonly file, readonly dir, readonly hard link,
        // junction, and locked file.
        let item = root.join("item");
        let outside = root.join("outside");
        let ro_dir = item.join("ro_dir");
        std::fs::create_dir_all(&ro_dir).unwrap();
        std::fs::create_dir_all(item.join(r"deep\sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"outside").unwrap();

        let ro_single = ro_dir.join("ro_single.txt");
        std::fs::write(&ro_single, b"1234").unwrap();
        let mut p = std::fs::metadata(&ro_single).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&ro_single, p).unwrap();

        // Mark ro_dir readonly as well.
        let mut dp = std::fs::metadata(&ro_dir).unwrap().permissions();
        dp.set_readonly(true);
        std::fs::set_permissions(&ro_dir, dp).unwrap();

        // Read-only hard link shared with outside.
        let ro_shared_out = outside.join("shared_ro.txt");
        std::fs::write(&ro_shared_out, b"hardlink").unwrap();
        let mut sp = std::fs::metadata(&ro_shared_out).unwrap().permissions();
        sp.set_readonly(true);
        std::fs::set_permissions(&ro_shared_out, sp).unwrap();
        std::fs::hard_link(&ro_shared_out, item.join("shared_ro.txt")).unwrap();

        // Junction inside item.
        let junc = item.join("junc");
        let junc_ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junc)
            .arg(&outside)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        // Locked file in deep\sub.
        let locked = item.join(r"deep\sub\held.bin");
        std::fs::write(&locked, b"locked").unwrap();
        std::fs::write(item.join(r"deep\sibling.bin"), b"ok").unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&locked).unwrap();

        let rep = delete_tree_with_opts(&item, false);
        drop(held);
        assert!(rep.remains, "{:?}", rep);
        assert_eq!(rep.failed_count, 1, "ancestor dirs must not double-count: {:?}", rep);
        assert_eq!(rep.files, 2, "{:?}", rep); // ro_single.txt + sibling.bin
        assert_eq!(rep.linked, 1, "{:?}", rep);
        assert_eq!(rep.linked_bytes, 8, "{:?}", rep);
        if junc_ok {
            assert_eq!(rep.links, 1, "{:?}", rep);
        }
        assert!(std::fs::metadata(&ro_shared_out).unwrap().permissions().readonly());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"outside");
        assert!(!item.join(r"deep\sibling.bin").exists());

        // Cleanup readonly files before remove_dir_all.
        for f in [&ro_shared_out, &item.join("shared_ro.txt")] {
            if let Ok(m) = std::fs::metadata(f) {
                let mut pm = m.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                pm.set_readonly(false);
                let _ = std::fs::set_permissions(f, pm);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_tree_multithreaded_stress() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-win-del-stress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let item = root.join("tree");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("safe.txt"), b"safe").unwrap();

        for d in 0..64usize {
            let sub = item.join(format!("dir_{:02}", d));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..20usize {
                let fp = sub.join(format!("f_{:02}.bin", f));
                std::fs::write(&fp, b"abcd").unwrap();
                if f == 0 {
                    let mut p = std::fs::metadata(&fp).unwrap().permissions();
                    p.set_readonly(true);
                    std::fs::set_permissions(&fp, p).unwrap();
                }
            }
        }
        let mut junc_count = 0u64;
        for j in 0..4usize {
            let jp = item.join(format!("junc_{}", j));
            if std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&jp)
                .arg(&outside)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                junc_count += 1;
            }
        }
        let held_path = item.join(r"dir_32\held.bin");
        std::fs::write(&held_path, b"lock").unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&held_path).unwrap();

        let rep = delete_tree(&item);
        assert!(rep.remains, "{:?}", rep);
        assert_eq!(rep.files, 64 * 20, "{:?}", rep);
        assert_eq!(rep.bytes, 64 * 20 * 4);
        assert_eq!(rep.links, junc_count);
        assert_eq!(rep.failed_count, 1, "{:?}", rep);
        assert_eq!(std::fs::read(outside.join("safe.txt")).unwrap(), b"safe");

        drop(held);
        let rep2 = delete_tree(&item);
        assert!(!rep2.remains, "{:?}", rep2);
        assert_eq!(rep2.files, 1);
        assert_eq!(rep2.failed_count, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn single_file_root_scans_across_walk_apis() {
        let root = std::env::temp_dir().join(format!("amg-win-singlefile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("loose_old.tmp");
        std::fs::write(&file, vec![0x5Au8; 12_345]).unwrap();
        let fstr = file.display().to_string();

        let cfg = VolumeWalkConfig {
            cluster: 4096,
            block_cloning: false,
            supports_hard_links: true,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            extent_deadline: None,
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (res, _) = walk_volume(&cfg, &[WalkItemInput { roots: std::slice::from_ref(&fstr), skip: &[] }]);
        assert_eq!(res.len(), 1);
        assert!(!res[0].not_reached);
        assert_eq!(res[0].level1.files, 1);
        assert_eq!(res[0].level1.dirs, 0);
        assert_eq!(res[0].level1.dir_failures, 0);
        assert_eq!(res[0].level1.logical, 12_345);
        assert!(res[0].level1.last_write > 0);
        let l2 = res[0].level2.as_ref().unwrap();
        assert_eq!(l2.files, 1);
        assert_eq!(l2.logical, 12_345);
        assert_eq!(l2.dir_failures, 0);

        let clean_st = walk_clean_paths(std::slice::from_ref(&fstr), 2, Instant::now() + Duration::from_secs(10), false);
        assert_eq!(clean_st.len(), 1);
        assert_eq!(clean_st[0].files, 1);
        assert_eq!(clean_st[0].dir_failures, 0);
        assert_eq!(clean_st[0].last_write, res[0].level1.last_write);

        let vs = VolumeScan {
            cluster: 4096,
            block_cloning: false,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            multi: Mutex::new(HashMap::new()),
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (item_st, _) = walk_item(&vs, 0, &[fstr], &[]);
        assert_eq!(item_st.files, 1);
        assert_eq!(item_st.logical, 12_345);
        assert_eq!(item_st.dir_failures, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_readonly_on_reparse_point_leaves_target_readonly_intact() {
        use std::os::windows::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("amg-win-ro-junc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let target = root.join("target");
        let junc = root.join("junc");
        std::fs::create_dir_all(&target).unwrap();
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junc)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        // Mark the target directory read-only and mark the junction itself read-only via FileBasicInfo.
        let mut tp = std::fs::metadata(&target).unwrap().permissions();
        tp.set_readonly(true);
        std::fs::set_permissions(&target, tp).unwrap();

        let jw = wide(&long(&junc.display().to_string()));
        let mut jw_no_nul = jw[..jw.len() - 1].to_vec();
        if let Ok(h) = open_dir_w(&mut jw_no_nul, 0x0100 | SYNCHRONIZE) {
            let attrs = std::fs::symlink_metadata(&junc).unwrap().file_attributes() | FILE_ATTRIBUTE_READONLY;
            let mut basic = [0u8; 40];
            basic[32..36].copy_from_slice(&attrs.to_le_bytes());
            unsafe {
                SetFileInformationByHandle(h.0, FILE_BASIC_INFO_CLASS, basic.as_ptr() as *const c_void, basic.len() as u32);
            }
        }
        let j_attrs = std::fs::symlink_metadata(&junc).unwrap().file_attributes();
        clear_readonly_w(&mut jw_no_nul, j_attrs, true);
        // Target directory MUST retain its READONLY attribute!
        let target_attrs = std::fs::symlink_metadata(&target).unwrap().file_attributes();
        assert_ne!(target_attrs & FILE_ATTRIBUTE_READONLY, 0, "target must remain read-only");
        // Clean up readonly attribute on target before removal.
        let mut tp2 = std::fs::metadata(&target).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        tp2.set_readonly(false);
        let _ = std::fs::set_permissions(&target, tp2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn registry_policy_and_filter_name_parser() {
        let p = read_dev_drive_policy();
        assert!(["antivirus_allowed", "antivirus_disallowed", "unknown"].contains(&p.av_filters));

        // Synthetic INSTANCE_AGGREGATE_STANDARD_INFORMATION (MiniFilter) with FilterName = "WdFilter".
        let mut buf = vec![0u8; 64];
        buf[4..8].copy_from_slice(&1u32.to_le_bytes()); // Flags = MiniFilter
        let name_u16: Vec<u16> = "WdFilter".encode_utf16().collect();
        let len_bytes = (name_u16.len() * 2) as u16;
        let off = 44u16;
        buf[32..34].copy_from_slice(&len_bytes.to_le_bytes());
        buf[34..36].copy_from_slice(&off.to_le_bytes());
        for (i, &u) in name_u16.iter().enumerate() {
            buf[off as usize + i * 2..off as usize + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        assert_eq!(extract_filter_name(&buf).as_deref(), Some("WdFilter"));
    }

    #[test]
    fn refs_volume_data_returns_none_on_ntfs_and_some_on_refs() {
        let mut checked_ntfs = false;
        let mut checked_refs = false;
        for v in volumes() {
            let Some(root) = v.paths.first() else { continue };
            if v.fs.eq_ignore_ascii_case("NTFS") {
                assert_eq!(refs_volume_data(root), None, "NTFS {} must return None", root);
                checked_ntfs = true;
            } else if v.fs.eq_ignore_ascii_case("ReFS") {
                let d = refs_volume_data(root).unwrap_or_else(|| panic!("ReFS {} returned None", root));
                assert!(d.bytes_per_cluster > 0, "{:?}", d);
                assert!(d.total_bytes > 0, "{:?}", d);
                checked_refs = true;
            }
        }
        assert!(checked_ntfs, "expected at least one NTFS volume (e.g. C:\\)");
        let _ = checked_refs;
    }

    #[test]
    fn wstr_ptr_to_string_enforces_bounds_alignment_and_nul() {
        let mut aligned = [0u64; 4]; // 32 bytes
        let buf = as_bytes_mut(&mut aligned);
        let text: Vec<u16> = "D:\\VHDX".encode_utf16().chain(std::iter::once(0)).collect();
        for (i, &u) in text.iter().enumerate() {
            buf[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        let valid_ptr = buf.as_ptr() as *const u16;
        assert_eq!(wstr_ptr_to_string(valid_ptr, buf).as_deref(), Some("D:\\VHDX"));
        assert_eq!(wstr_ptr_to_string(null(), buf), None);
        // Unaligned pointer inside buffer must be rejected.
        // SAFETY: `buf.as_ptr().add(1)` is within `buf` (32 bytes), only used for address check.
        let unaligned = unsafe { buf.as_ptr().add(1) as *const u16 };
        assert_eq!(wstr_ptr_to_string(unaligned, buf), None);
        // Out-of-bounds pointer before/after buffer must be rejected.
        // SAFETY: `buf.as_ptr().add(buf.len())` is one-past-end pointer for bounds check only.
        let oob = unsafe { buf.as_ptr().add(buf.len()) as *const u16 };
        assert_eq!(wstr_ptr_to_string(oob, buf), None);
        // Unterminated buffer slice must be rejected.
        assert_eq!(wstr_ptr_to_string(valid_ptr, &buf[..8]), None);
    }

    #[test]
    fn t5_scan_dir_multi_batch_3500_files_and_sparse_zero_alloc() {
        let root = std::env::temp_dir().join(format!("amg-t5-many-sparse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let many = root.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..3500u32 {
            std::fs::write(many.join(format!("file_with_long_name_{:04}.obj", i)), b"x").unwrap();
        }
        let vs = VolumeScan {
            cluster: 4096,
            block_cloning: false,
            level2: true,
            threads: 4,
            deadline: Instant::now() + Duration::from_secs(20),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            multi: Mutex::new(HashMap::new()),
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let rs = vec![many.display().to_string()];
        let (wi, _) = walk_item(&vs, 0, &rs, &[]);
        assert_eq!(wi.files, 3500);
        assert_eq!(wi.dir_failures, 0);
        assert!(!wi.cut_short);

        let wcfg = VolumeWalkConfig {
            cluster: 4096,
            block_cloning: false,
            supports_hard_links: true,
            level2: true,
            threads: 4,
            deadline: Instant::now() + Duration::from_secs(20),
            extent_deadline: None,
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (wv, _) = walk_volume(&wcfg, &[WalkItemInput { roots: &rs, skip: &[] }]);
        assert_eq!(wv[0].level1.files, 3500);
        assert_eq!(wv[0].level2.as_ref().map(|l| l.files), Some(3500));

        // 10 MiB sparse file with 0 allocated clusters
        let sp_dir = root.join("sparse");
        std::fs::create_dir_all(&sp_dir).unwrap();
        let sp = sp_dir.join("zero_alloc.bin");
        std::fs::write(&sp, &[]).unwrap();
        if std::process::Command::new("fsutil").args(["sparse", "setflag"]).arg(&sp).output().is_ok_and(|o| o.status.success()) {
            let f = std::fs::OpenOptions::new().write(true).open(&sp).unwrap();
            f.set_len(10 * 1024 * 1024).unwrap();
            drop(f);
            let (ss, ext) = walk_item(&vs, 0, &[sp_dir.display().to_string()], &[]);
            assert_eq!(ss.files, 1);
            assert_eq!(ss.logical, 10 * 1024 * 1024);
            assert_eq!(ss.allocated, 0);
            assert_eq!(ss.sparse_files, 1);
            assert_eq!(ext.exclusive_clusters, 0);
            assert!(ext.shared.is_empty());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn t5_trailing_dot_space_and_readonly_junction_delete() {
        let root = std::env::temp_dir().join(format!("amg-t5-trail-roj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let trail = root.join("trail");
        std::fs::create_dir_all(&trail).unwrap();
        std::fs::write(format!("\\\\?\\{}\\trailing.", trail.display()), vec![9u8; 5000]).unwrap();
        std::fs::write(format!("\\\\?\\{}\\space ", trail.display()), vec![9u8; 5000]).unwrap();
        let vs = VolumeScan {
            cluster: 4096,
            block_cloning: false,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            multi: Mutex::new(HashMap::new()),
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (ws, _) = walk_item(&vs, 0, &[trail.display().to_string()], &[]);
        assert_eq!(ws.files, 2);
        assert_eq!(ws.dir_failures, 0);
        let del = delete_tree_with_opts(&trail, false);
        assert!(!del.remains && del.files == 2 && del.failed_count == 0, "{:?}", del);

        let target = root.join("TargetKeep");
        let wrap = root.join("ROJuncWrap");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&wrap).unwrap();
        std::fs::write(target.join("keep.uasset"), b"precious").unwrap();
        let junc = wrap.join("ro_link");
        if std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&junc).arg(&target).output().is_ok_and(|o| o.status.success()) {
            let _ = std::process::Command::new("attrib").args(["+R", "/L"]).arg(&junc).output();
            let r = delete_tree_with_opts(&wrap, false);
            assert!(!r.remains && r.links == 1 && r.failed_count == 0, "{:?}", r);
            assert!(target.join("keep.uasset").is_file(), "junction target must stay untouched");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn t5_ntfs_external_hardlink_walk_item_and_sharing_violation_delete() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-t5-extlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let item = root.join("item");
        let outside = root.join("outside");
        std::fs::create_dir_all(&item).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let out_file = outside.join("shared.bin");
        std::fs::write(&out_file, vec![7u8; 32_768]).unwrap();
        std::fs::hard_link(&out_file, item.join("in_item.bin")).unwrap();

        let vs = VolumeScan {
            cluster: 4096,
            block_cloning: false,
            level2: true,
            threads: 2,
            deadline: Instant::now() + Duration::from_secs(10),
            stop: Arc::new(AtomicBool::new(false)),
            started_ft: u64::MAX,
            multi: Mutex::new(HashMap::new()),
            progress_files: Arc::new(AtomicU64::new(0)),
        };
        let (ws, ext) = walk_item(&vs, 0, &[item.display().to_string()], &[]);
        assert_eq!(ws.files, 1);
        assert_eq!(ws.hard_links, 0);
        let mut items = vec![ext];
        let multi = std::mem::take(&mut *vs.multi.lock().unwrap());
        assert_eq!(multi.len(), 1, "walk_item (MultiSink::Single) detects 1-in/1-out external NTFS hard link in multi");
        let linked = attribute_multilinks(multi, &mut items, false);
        assert!(linked.is_empty());
        let sw = crate::disk_space::sweep(&items);
        assert!(sw.items[0].footprint >= 32_768 / 4096);
        assert_eq!(sw.items[0].frees_alone, 0, "external link keeps clusters live");

        // Sharing violation under delete_tree_with_opts(use_posix=true) must record failure without hanging
        let held_path = item.join("held.bin");
        std::fs::write(&held_path, b"locked").unwrap();
        let _held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&held_path).unwrap();
        let del = delete_tree_with_opts(&item, true);
        assert!(del.remains && del.failed_count >= 1, "{:?}", del);
        drop(_held);
        let _ = std::fs::remove_dir_all(&root);
    }
}

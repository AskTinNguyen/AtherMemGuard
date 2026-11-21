//! Safe wrappers over the Win32 / NT calls AtherMemGuard uses. All `unsafe` lives here and in `tray`.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fmt;
use std::mem::{size_of, size_of_val, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
    ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, HWND, INVALID_HANDLE_VALUE,
    LPARAM, SYSTEMTIME, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindCloseChangeNotification, FindFirstChangeNotificationW,
    FindNextChangeNotification, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_WORKINGSET,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, CreateMemoryResourceNotification, LowMemoryResourceNotification,
    MapViewOfFile, OpenFileMappingW, SetProcessWorkingSetSizeEx, UnmapViewOfFile, FILE_MAP_READ,
    FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE, QUOTA_LIMITS_HARDWS_MAX_DISABLE,
    QUOTA_LIMITS_HARDWS_MAX_ENABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS,
    PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW,
    RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WRITE, REG_BINARY, REG_DWORD, REG_EXPAND_SZ,
    REG_MULTI_SZ, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimeAsFileTime};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, GetCurrentProcessId, GetPriorityClass,
    GetProcessTimes, OpenEventW, OpenMutexW, OpenProcess, ProcessMemoryPriority,
    ProcessPowerThrottling, ResetEvent, SetEvent, SetPriorityClass, SetProcessInformation,
    TerminateProcess, WaitForMultipleObjects, WaitForSingleObject, BELOW_NORMAL_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, INFINITE, MEMORY_PRIORITY_INFORMATION, NORMAL_PRIORITY_CLASS,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, MsgWaitForMultipleObjectsEx, SendMessageTimeoutW,
    SetCoalescableTimer, HWND_BROADCAST, MWMO_INPUTAVAILABLE, QS_ALLINPUT, SMTO_ABORTIFHUNG,
    SW_SHOWNORMAL, WM_SETTINGCHANGE,
};

const SYNCHRONIZE: u32 = 0x0010_0000;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const POWER_THROTTLING_VERSION: u32 = 1;
const POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;
const PROCESS_IO_PRIORITY: u32 = 33;

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: HANDLE) -> i32;
    fn NtResumeProcess(process: HANDLE) -> i32;
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn NtSetInformationProcess(process: HANDLE, class: u32, info: *const c_void, len: u32) -> i32;
    fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn NtSetSystemInformation(class: u32, info: *mut c_void, len: u32) -> i32;
    fn RtlGetCompressionWorkSpaceSize(
        format_and_engine: u16,
        compress_workspace_size: *mut u32,
        decompress_fragment_workspace_size: *mut u32,
    ) -> i32;
    fn RtlCompressBuffer(
        format_and_engine: u16,
        uncompressed_buffer: *const u8,
        uncompressed_buffer_size: u32,
        compressed_buffer: *mut u8,
        compressed_buffer_size: u32,
        uncompressed_chunk_size: u32,
        final_compressed_size: *mut u32,
        workspace: *mut c_void,
    ) -> i32;
    fn RtlDecompressBuffer(
        format: u16,
        uncompressed_buffer: *mut u8,
        uncompressed_buffer_size: u32,
        compressed_buffer: *const u8,
        compressed_buffer_size: u32,
        final_uncompressed_size: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer_size: u32,
        in_buffer_size: u32,
        default_time_out: u32,
        security_attributes: *const c_void,
    ) -> HANDLE;
    fn ConnectNamedPipe(named_pipe: HANDLE, overlapped: *mut OVERLAPPED) -> i32;
    fn DisconnectNamedPipe(named_pipe: HANDLE) -> i32;
    fn WaitNamedPipeW(named_pipe_name: *const u16, time_out: u32) -> i32;
}

/// Owned kernel handle, closed on drop.
pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// Handles are plain kernel references; we only use them from one thread at a time.
unsafe impl Send for Handle {}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open(pid: u32, access: u32) -> Option<Handle> {
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Now, as a Windows FILETIME (100 ns ticks).
pub fn now_filetime() -> u64 {
    let mut ft: FILETIME = unsafe { zeroed() };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    filetime_u64(&ft)
}

pub fn local_time_string() -> String {
    let mut t: SYSTEMTIME = unsafe { zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

// ---------------------------------------------------------------- system memory

#[derive(Debug, Clone, Copy, Default)]
pub struct SysInfo {
    pub ram_total: u64,
    pub ram_avail: u64,
    pub commit_used: u64,
    pub commit_limit: u64,
    pub file_cache: u64,
    pub nonpaged_pool: u64,
    /// Kernel paged pool, bytes, wherever it is (part of it may be paged out).
    pub paged_pool: u64,
    /// Kernel memory that is in RAM right now, bytes, from SystemPerformanceInformation. All
    /// zero when that call failed or its numbers didn't check out against
    /// K32GetPerformanceInfo (an unexpected layout).
    pub resident_paged_pool: u64,
    pub resident_system_code: u64,
    pub resident_drivers: u64,
    /// The system file cache's working set (mapped file data in RAM, not the standby list).
    pub resident_system_cache: u64,
}

impl SysInfo {
    pub fn commit_free(&self) -> u64 {
        self.commit_limit.saturating_sub(self.commit_used)
    }
}

static BASIC_INFO: OnceLock<(u64, u64)> = OnceLock::new();

fn basic_info() -> (u64, u64) {
    *BASIC_INFO.get_or_init(|| {
        const SYSTEM_BASIC_INFORMATION_CLASS: u32 = 0;
        let mut buf = [0u32; 16]; // exact 64 bytes
        let mut ret = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_BASIC_INFORMATION_CLASS,
                buf.as_mut_ptr() as *mut c_void,
                size_of_val(&buf) as u32,
                &mut ret,
            )
        };
        if status >= 0 && ret >= 16 && buf[2] > 0 {
            let page = buf[2] as u64;
            return (page, buf[3] as u64 * page);
        }
        let mut pi: PERFORMANCE_INFORMATION = unsafe { zeroed() };
        pi.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
        if unsafe { K32GetPerformanceInfo(&mut pi, pi.cb) } != 0 && pi.PageSize > 0 {
            let page = pi.PageSize as u64;
            return (page, pi.PhysicalTotal as u64 * page);
        }
        (4096, 0)
    })
}

/// System-wide RAM, commit, pool, and cache counters from a single
/// `NtQuerySystemInformation(SystemPerformanceInformation)` call paired with cached
/// `SystemBasicInformation`.
pub fn sys_info() -> SysInfo {
    const SYSTEM_PERFORMANCE_INFORMATION_CLASS: u32 = 2;
    const AVAILABLE: usize = 11;
    const COMMITTED: usize = 12;
    const COMMIT_LIMIT: usize = 13;
    const PAGED_POOL: usize = 28;
    const NONPAGED_POOL: usize = 29;
    const RESIDENT_SYSTEM_CODE: usize = 35;
    const RESIDENT_SYSTEM_CACHE: usize = 41;
    const RESIDENT_PAGED_POOL: usize = 42;
    const RESIDENT_SYSTEM_DRIVER: usize = 43;

    let (page, ram_total) = basic_info();
    if ram_total == 0 {
        return SysInfo::default();
    }
    let mut buf = [0u32; 128];
    let mut ret = 0u32;
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_PERFORMANCE_INFORMATION_CLASS,
            buf.as_mut_ptr() as *mut c_void,
            size_of_val(&buf) as u32,
            &mut ret,
        )
    };
    if status < 0 || (ret as usize) < (RESIDENT_SYSTEM_DRIVER + 1) * 4 {
        return SysInfo::default();
    }
    let avail_pages = buf[AVAILABLE] as u64;
    let cache_ws_pages = buf[RESIDENT_SYSTEM_CACHE] as u64;
    SysInfo {
        ram_total,
        ram_avail: avail_pages * page,
        commit_used: buf[COMMITTED] as u64 * page,
        commit_limit: buf[COMMIT_LIMIT] as u64 * page,
        file_cache: (cache_ws_pages + avail_pages) * page,
        nonpaged_pool: buf[NONPAGED_POOL] as u64 * page,
        paged_pool: buf[PAGED_POOL] as u64 * page,
        resident_paged_pool: buf[RESIDENT_PAGED_POOL] as u64 * page,
        resident_system_code: buf[RESIDENT_SYSTEM_CODE] as u64 * page,
        resident_drivers: buf[RESIDENT_SYSTEM_DRIVER] as u64 * page,
        resident_system_cache: cache_ws_pages * page,
    }
}

// ---------------------------------------------------------------- processes

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Executable name without `.exe`.
    pub name: String,
    /// Physical RAM in use (working set), bytes. Includes pages shared with other processes.
    pub ram: u64,
    /// The part of `ram` only this process uses (private working set), bytes. Adding these
    /// up across processes counts nothing twice. 0 where it isn't known (the Toolhelp list).
    pub private: u64,
    /// Committed private memory, bytes.
    pub committed: u64,
    /// Creation time as FILETIME, 0 if unknown.
    pub created: u64,
    /// We can open it for query (PROCESS_QUERY_LIMITED_INFORMATION). Processes we can't
    /// open (elevated, protected, other users) can't be trimmed, re-prioritized or ended
    /// by us either, so the engine leaves them out of groups and orphans.
    pub queryable: bool,
}

const SYSTEM_PROCESS_INFORMATION_CLASS: u32 = 5;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

/// The documented head of SYSTEM_PROCESS_INFORMATION with the fields windows-sys hides in
/// `Reserved` arrays named (layout as in the NT headers). Thread entries follow it.
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name_length: u16,
    image_name_maximum_length: u16,
    image_name_buffer: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    /// The commit charge: the same number as PROCESS_MEMORY_COUNTERS_EX.PrivateUsage.
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
}

/// Walk a SystemProcessInformation buffer directly into `out` (and optionally `sessions`).
/// `buf` is the buffer as the kernel filled it: the image name pointers point into it.
/// Entries or names that fall outside it are skipped.
fn parse_spi_into(buf: &[u8], out: &mut Vec<Proc>, mut sessions: Option<&mut Vec<u32>>) {
    out.clear();
    if let Some(ref mut s) = sessions {
        s.clear();
    }
    let base = buf.as_ptr() as usize;
    let mut off = 0usize;
    loop {
        if off.checked_add(size_of::<SystemProcessInformation>()).map_or(true, |end| end > buf.len()) {
            break;
        }
        // SAFETY: the range was bounds-checked above, and read_unaligned copes with any
        // alignment of the caller's buffer.
        let e: SystemProcessInformation = unsafe { std::ptr::read_unaligned(buf[off..].as_ptr() as *const _) };
        let pid = e.unique_process_id as u32;
        let len = e.image_name_length as usize & !1;
        let mut name = if e.image_name_buffer == 0 || len == 0 {
            if pid == 0 { "[System Process]".to_string() } else { String::new() }
        } else {
            match e.image_name_buffer.checked_sub(base).filter(|&start| start.checked_add(len).is_some_and(|end| end <= buf.len())) {
                Some(start) => {
                    let iter = buf[start..start + len]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]));
                    let mut s = String::with_capacity(len / 2);
                    for r in char::decode_utf16(iter) {
                        s.push(r.unwrap_or(char::REPLACEMENT_CHARACTER));
                    }
                    s
                }
                None => String::new(),
            }
        };
        if name.len() >= 4 && name.get(name.len() - 4..).is_some_and(|t| t.eq_ignore_ascii_case(".exe")) {
            name.truncate(name.len() - 4);
        }
        out.push(Proc {
            pid,
            ppid: e.inherited_from_unique_process_id as u32,
            name,
            ram: e.working_set_size as u64,
            private: e.working_set_private_size.max(0) as u64,
            committed: e.pagefile_usage as u64,
            created: e.create_time.max(0) as u64,
            queryable: false,
        });
        if let Some(ref mut s) = sessions {
            s.push(e.session_id);
        }
        if e.next_entry_offset == 0 {
            break;
        }
        off += e.next_entry_offset as usize;
    }
}

#[cfg(test)]
fn parse_spi(buf: &[u8]) -> Vec<Proc> {
    let mut out = Vec::with_capacity(512);
    parse_spi_into(buf, &mut out, None);
    out
}

#[derive(Clone, Copy)]
struct QueryEntry {
    queryable: bool,
    session_id: u32,
    gen: u64,
}

/// The query buffer, reused between calls (8-byte aligned), and per-process cached metadata
/// keyed by `(pid, created)` with an in-place generation counter to avoid per-tick HashMap
/// allocations.
struct ListState {
    buf: Vec<u64>,
    cache: HashMap<(u32, u64), QueryEntry>,
    gen: u64,
    last_count: usize,
    scratch_sessions: Vec<u32>,
}

static LIST_STATE: Mutex<Option<ListState>> = Mutex::new(None);
static CMD_BUF: Mutex<Vec<u64>> = Mutex::new(Vec::new());

const DEFAULT_SPI_U64S: usize = 128 * 1024; // 1 MB

/// Every process on the machine with its memory use, from one
/// `NtQuerySystemInformation(SystemProcessInformation)` call. Preserves cached state on failure.
pub fn try_list_processes() -> Result<Vec<Proc>, String> {
    let mut guard = LIST_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(|| ListState {
        buf: vec![0u64; DEFAULT_SPI_U64S],
        cache: HashMap::with_capacity(512),
        gen: 0,
        last_count: 512,
        scratch_sessions: Vec::with_capacity(512),
    });
    let mut ret = 0u32;
    let mut status = STATUS_INFO_LENGTH_MISMATCH;
    for _ in 0..8 {
        let bytes = (state.buf.len() * 8).min(u32::MAX as usize) as u32;
        status = unsafe {
            NtQuerySystemInformation(SYSTEM_PROCESS_INFORMATION_CLASS, state.buf.as_mut_ptr() as *mut c_void, bytes, &mut ret)
        };
        if status != STATUS_INFO_LENGTH_MISMATCH {
            break;
        }
        // Processes start between calls: ask for what it said plus some room.
        let want = (ret as usize).max(state.buf.len() * 8) + 64 * 1024;
        state.buf = vec![0u64; want.div_ceil(8)];
    }
    if status < 0 {
        return Err(format!("NtQuerySystemInformation(SystemProcessInformation) failed: {:#x}", status as u32));
    }
    let len = (ret as usize).min(state.buf.len() * 8);
    // SAFETY: a u64 buffer viewed as bytes, within its length.
    let bytes = unsafe { std::slice::from_raw_parts(state.buf.as_ptr() as *const u8, len) };
    let mut out = Vec::with_capacity(state.last_count.max(64));
    parse_spi_into(bytes, &mut out, Some(&mut state.scratch_sessions));
    state.last_count = out.len();

    state.gen = state.gen.wrapping_add(1);
    let gen = state.gen;
    for (p, &sid) in out.iter_mut().zip(state.scratch_sessions.iter()) {
        let key = (p.pid, p.created);
        let entry = state.cache.entry(key).or_insert_with(|| QueryEntry {
            queryable: p.pid != 0 && open(p.pid, PROCESS_QUERY_LIMITED_INFORMATION).is_some(),
            session_id: sid,
            gen,
        });
        entry.gen = gen;
        entry.session_id = sid;
        p.queryable = entry.queryable;
    }
    state.cache.retain(|_, v| v.gen == gen);
    Ok(out)
}

/// Every process on the machine with its memory use, from one
/// `NtQuerySystemInformation(SystemProcessInformation)` call.
pub fn list_processes() -> Vec<Proc> {
    try_list_processes().unwrap_or_default()
}

/// Windows session ID captured during the most recent `list_processes` / `try_list_processes`.
pub fn process_session_id(pid: u32, created: u64) -> Option<u32> {
    let guard = LIST_STATE.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().and_then(|s| s.cache.get(&(pid, created)).map(|e| e.session_id))
}

/// Shrink oversized process-enumeration and command-line scratch buffers when idle.
pub fn shrink_buffers() {
    {
        let mut guard = LIST_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = guard.as_mut() {
            if state.buf.len() > DEFAULT_SPI_U64S {
                state.buf = vec![0u64; DEFAULT_SPI_U64S];
            }
            state.scratch_sessions.clear();
            state.scratch_sessions.shrink_to(512);
            state.cache.shrink_to_fit();
        }
    }
    {
        let mut cmd = CMD_BUF.lock().unwrap_or_else(|e| e.into_inner());
        *cmd = Vec::new();
    }
}

/// The process list the way 0.2.0 read it: a Toolhelp snapshot, then OpenProcess,
/// K32GetProcessMemoryInfo and GetProcessTimes per process. Processes we can't open report
/// zeros. Kept to compare with `list_processes` (tests and `examples/tick_bench`).
#[doc(hidden)]
pub fn list_processes_toolhelp() -> Vec<Proc> {
    let mut out = Vec::with_capacity(512);
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return out;
    }
    let snap = Handle(snap);
    let mut e: PROCESSENTRY32W = unsafe { zeroed() };
    e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snap.0, &mut e) };
    while ok != 0 {
        let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
        let mut name = String::from_utf16_lossy(&e.szExeFile[..len]);
        if name.to_ascii_lowercase().ends_with(".exe") {
            name.truncate(name.len() - 4);
        }
        let mut p = Proc {
            pid: e.th32ProcessID,
            ppid: e.th32ParentProcessID,
            name,
            ram: 0,
            private: 0,
            committed: 0,
            created: 0,
            queryable: false,
        };
        if p.pid != 0 {
            if let Some(h) = open(p.pid, PROCESS_QUERY_LIMITED_INFORMATION) {
                p.queryable = true;
                if let Some((ram, committed)) = memory_counters(&h) {
                    p.ram = ram;
                    p.committed = committed;
                }
                let (mut c, mut x, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) =
                    unsafe { (zeroed(), zeroed(), zeroed(), zeroed()) };
                if unsafe { GetProcessTimes(h.0, &mut c, &mut x, &mut k, &mut u) } != 0 {
                    p.created = filetime_u64(&c);
                }
            }
        }
        out.push(p);
        ok = unsafe { Process32NextW(snap.0, &mut e) };
    }
    out
}

/// (working set, private commit) in bytes from K32GetProcessMemoryInfo.
fn memory_counters(h: &Handle) -> Option<(u64, u64)> {
    let mut mc: PROCESS_MEMORY_COUNTERS_EX = unsafe { zeroed() };
    mc.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let got = unsafe { K32GetProcessMemoryInfo(h.0, &mut mc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mc.cb) };
    (got != 0).then_some((mc.WorkingSetSize as u64, mc.PrivateUsage as u64))
}

/// Full command line of a process, or None if it can't be read.
pub fn command_line(pid: u32) -> Option<String> {
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = CMD_BUF.lock().unwrap_or_else(|e| e.into_inner());
    if buf.len() < 8400 {
        buf.resize(8400, 0);
    }
    let bytes = (buf.len() * 8) as u32;
    let mut ret = 0u32;
    let status = unsafe {
        NtQueryInformationProcess(h.0, PROCESS_COMMAND_LINE_INFORMATION, buf.as_mut_ptr() as *mut c_void, bytes, &mut ret)
    };
    if status < 0 {
        return None;
    }
    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    if us.buffer.is_null() || us.length == 0 {
        return Some(String::new());
    }
    let base = buf.as_ptr() as usize;
    let total_bytes = buf.len() * size_of::<u64>();
    let ptr = us.buffer as usize;
    let byte_len = (us.length as usize) & !1;
    let off = ptr.checked_sub(base)?;
    if off.checked_add(byte_len).is_none_or(|end| end > total_bytes) {
        return None;
    }
    let raw = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, total_bytes) };
    let mut out = String::with_capacity(byte_len / 2);
    for r in char::decode_utf16(
        raw[off..off + byte_len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]])),
    ) {
        out.push(r.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    Some(out)
}

// ---------------------------------------------------------------- soft controls

fn set_memory_priority_handle(h: HANDLE, priority: u32) -> bool {
    let info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: priority };
    unsafe {
        SetProcessInformation(
            h,
            ProcessMemoryPriority,
            &info as *const _ as *const c_void,
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        ) != 0
    }
}

fn set_eco_mode_handle(h: HANDLE, on: bool) -> bool {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: POWER_THROTTLING_VERSION,
        ControlMask: if on { POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
        StateMask: if on { POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
    };
    unsafe {
        SetProcessInformation(
            h,
            ProcessPowerThrottling,
            &state as *const _ as *const c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) != 0
    }
}

fn set_io_priority_handle(h: HANDLE, priority: u32) -> bool {
    unsafe { NtSetInformationProcess(h, PROCESS_IO_PRIORITY, &priority as *const u32 as *const c_void, 4) >= 0 }
}

/// 1 = very low … 5 = normal. Lower-priority pages are evicted from RAM first.
pub fn set_memory_priority(pid: u32, priority: u32) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    set_memory_priority_handle(h.0, priority)
}

/// Push a process's memory out of RAM (to the compressed store / page file).
pub fn trim(pid: u32) -> bool {
    let Some(h) = open(pid, PROCESS_SET_QUOTA) else { return false };
    unsafe { SetProcessWorkingSetSizeEx(h.0, usize::MAX, usize::MAX, 0) != 0 }
}

/// Set or clear a process's physical working-set cap. When `max_bytes == 0`, clears any hard cap.
pub fn set_working_set_cap(pid: u32, min_bytes: usize, max_bytes: usize, hard: bool) -> bool {
    let Some(h) = open(pid, PROCESS_SET_QUOTA) else { return false };
    if max_bytes == 0 {
        // Passing (usize::MAX, usize::MAX) only trims without resetting quota limits/flags.
        // Restore default soft bounds (50..345 pages) with hard limits disabled.
        let flags = QUOTA_LIMITS_HARDWS_MAX_DISABLE | QUOTA_LIMITS_HARDWS_MIN_DISABLE;
        return unsafe { SetProcessWorkingSetSizeEx(h.0, 204_800, 1_413_120, flags) != 0 };
    }
    let (min, flags) = if hard {
        let min_floor = (max_bytes / 4).clamp(20 * 4096, 1 << 20).min(max_bytes);
        (min_bytes.max(min_floor).min(max_bytes), QUOTA_LIMITS_HARDWS_MAX_ENABLE)
    } else {
        (min_bytes.max(20 * 4096).min(max_bytes), 0)
    };
    unsafe { SetProcessWorkingSetSizeEx(h.0, min, max_bytes, flags) != 0 }
}

/// Trim the current process's own working set (used to keep the idle tray footprint small).
pub fn self_trim() -> bool {
    unsafe { SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0) != 0 }
}

/// Put the current process into (or out of) Windows EcoQoS + IDLE_PRIORITY_CLASS.
pub fn set_self_eco_qos(on: bool) -> bool {
    let h = unsafe { GetCurrentProcess() };
    let class = if on { IDLE_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS };
    let prio_ok = unsafe { SetPriorityClass(h, class) != 0 };
    let eco_ok = set_eco_mode_handle(h, on);
    prio_ok && eco_ok
}

/// PID of the process that owns the current foreground window, if any.
pub fn foreground_pid() -> Option<u32> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some(pid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuClass {
    Normal,
    BelowNormal,
    Idle,
}

pub fn set_cpu_class(pid: u32, class: CpuClass) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    let value = match class {
        CpuClass::Normal => NORMAL_PRIORITY_CLASS,
        CpuClass::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        CpuClass::Idle => IDLE_PRIORITY_CLASS,
    };
    unsafe { SetPriorityClass(h.0, value) != 0 }
}

/// Raw Windows priority class of a process, 0 if unknown.
pub fn cpu_class_raw(pid: u32) -> u32 {
    match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        Some(h) => unsafe { GetPriorityClass(h.0) },
        None => 0,
    }
}

pub fn set_cpu_class_raw(pid: u32, raw: u32) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    unsafe { SetPriorityClass(h.0, raw) != 0 }
}

/// Windows efficiency mode (EcoQoS): the scheduler runs the process on efficient cores at low speed.
/// Leaves priority class untouched for caller compatibility; use `set_eco_qos` for Win11 EcoQoS.
pub fn set_eco_mode(pid: u32, on: bool) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    set_eco_mode_handle(h.0, on)
}

/// Win11 EcoQoS: pairs `IDLE_PRIORITY_CLASS` + `ExecutionSpeed` when `on`, and restores
/// `NORMAL_PRIORITY_CLASS` + `(0, 0)` masks when `off`.
pub fn set_eco_qos(pid: u32, on: bool) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    let class = if on { IDLE_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS };
    let prio_ok = unsafe { SetPriorityClass(h.0, class) != 0 };
    let eco_ok = set_eco_mode_handle(h.0, on);
    prio_ok && eco_ok
}

/// Set process I/O priority (`0` = VeryLow, `1` = Low, `2` = Normal).
pub fn set_io_priority_raw(pid: u32, priority: u32) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    set_io_priority_handle(h.0, priority)
}

/// Set process I/O priority to VeryLow (`true`) or Normal (`false`).
pub fn set_io_priority(pid: u32, very_low: bool) -> bool {
    set_io_priority_raw(pid, if very_low { 0 } else { 2 })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessPolicy {
    pub memory_priority: Option<u32>,
    pub cpu_class_raw: Option<u32>,
    pub eco_mode: Option<bool>,
    pub io_priority: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyResult {
    pub opened: bool,
    pub prev_cpu_class: u32,
    pub memory_ok: bool,
    pub cpu_ok: bool,
    pub eco_ok: bool,
    pub io_ok: bool,
}

/// Apply memory priority, CPU priority class, EcoQoS, and I/O priority in a single `OpenProcess`.
pub fn apply_process_policy(pid: u32, read_cpu_class: bool, policy: ProcessPolicy) -> PolicyResult {
    let access = PROCESS_SET_INFORMATION | if read_cpu_class { PROCESS_QUERY_LIMITED_INFORMATION } else { 0 };
    let Some(h) = open(pid, access) else {
        return PolicyResult::default();
    };
    let mut res = PolicyResult { opened: true, ..PolicyResult::default() };
    if read_cpu_class {
        res.prev_cpu_class = unsafe { GetPriorityClass(h.0) };
    }
    if let Some(p) = policy.memory_priority {
        res.memory_ok = set_memory_priority_handle(h.0, p);
    }
    if let Some(raw) = policy.cpu_class_raw {
        res.cpu_ok = unsafe { SetPriorityClass(h.0, raw) != 0 };
    }
    if let Some(on) = policy.eco_mode {
        res.eco_ok = set_eco_mode_handle(h.0, on);
    }
    if let Some(io) = policy.io_priority {
        res.io_ok = set_io_priority_handle(h.0, io);
    }
    res
}

pub fn suspend(pid: u32) -> bool {
    match open(pid, PROCESS_SUSPEND_RESUME) {
        Some(h) => unsafe { NtSuspendProcess(h.0) >= 0 },
        None => false,
    }
}

pub fn resume(pid: u32) -> bool {
    match open(pid, PROCESS_SUSPEND_RESUME) {
        Some(h) => unsafe { NtResumeProcess(h.0) >= 0 },
        None => false,
    }
}

/// Resume a process through a handle we already own (used for CREATE_SUSPENDED children).
pub fn resume_handle(h: HANDLE) -> bool {
    unsafe { NtResumeProcess(h) >= 0 }
}

pub fn terminate(pid: u32) -> bool {
    match open(pid, PROCESS_TERMINATE) {
        Some(h) => unsafe { TerminateProcess(h.0, 1) != 0 },
        None => false,
    }
}

// ---------------------------------------------------------------- Windows' file cache (needs admin)

/// Is this process running with administrator rights (elevated)?
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Handle(token);
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    let ok = unsafe {
        GetTokenInformation(token.0, TokenElevation, &mut e as *mut _ as *mut c_void, size_of::<TOKEN_ELEVATION>() as u32, &mut ret)
    };
    ok != 0 && e.TokenIsElevated != 0
}

/// Turn on a privilege this process's token holds but has off ("SeIncreaseQuotaPrivilege").
/// False when the token doesn't hold it (not elevated).
fn enable_privilege(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Handle(token);
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    if unsafe { LookupPrivilegeValueW(null(), wide(name).as_ptr(), &mut luid) } == 0 {
        return false;
    }
    let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
    let ok = unsafe { AdjustTokenPrivileges(token.0, 0, &tp, 0, null_mut(), null_mut()) };
    ok != 0 && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED
}

/// Empty Windows' file cache: its pages move to the standby list, which counts as available
/// RAM, so nothing is lost: Windows reads the files again if they are needed. Needs
/// administrator rights (SeIncreaseQuotaPrivilege).
pub fn flush_file_cache() -> Result<(), String> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Memory::SetSystemFileCacheSize;
    if !enable_privilege("SeIncreaseQuotaPrivilege") {
        return Err("needs administrator rights".into());
    }
    // (SIZE_T)-1 for both limits empties the cache's working set.
    if unsafe { SetSystemFileCacheSize(usize::MAX, usize::MAX, 0) } == 0 {
        return Err(format!("SetSystemFileCacheSize failed (error {})", unsafe { GetLastError() }));
    }
    Ok(())
}

/// Write changed file data waiting in RAM (the modified list) to disk, so it becomes standby
/// memory too. Needs administrator rights (SeProfileSingleProcessPrivilege).
pub fn flush_modified_list() -> Result<(), String> {
    const SYSTEM_MEMORY_LIST_INFORMATION: u32 = 80;
    const MEMORY_FLUSH_MODIFIED_LIST: u32 = 3;
    if !enable_privilege("SeProfileSingleProcessPrivilege") {
        return Err("needs administrator rights".into());
    }
    let mut cmd = MEMORY_FLUSH_MODIFIED_LIST;
    let status = unsafe { NtSetSystemInformation(SYSTEM_MEMORY_LIST_INFORMATION, &mut cmd as *mut u32 as *mut c_void, 4) };
    if status < 0 {
        return Err(format!("flushing the modified list failed (status {:#x})", status as u32));
    }
    Ok(())
}

/// Purge the low-priority standby list (`MemoryPurgeLowPriorityStandbyList = 5`).
/// Needs administrator rights (`SeProfileSingleProcessPrivilege`).
pub fn purge_low_priority_standby() -> Result<(), String> {
    const SYSTEM_MEMORY_LIST_INFORMATION: u32 = 80;
    const MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST: u32 = 5;
    if !enable_privilege("SeProfileSingleProcessPrivilege") {
        return Err("needs administrator rights".into());
    }
    let mut cmd = MEMORY_PURGE_LOW_PRIORITY_STANDBY_LIST;
    let status = unsafe { NtSetSystemInformation(SYSTEM_MEMORY_LIST_INFORMATION, &mut cmd as *mut u32 as *mut c_void, 4) };
    if status < 0 {
        return Err(format!("purging low-priority standby list failed (status {:#x})", status as u32));
    }
    Ok(())
}

/// Deduplicate identical physical memory pages (`SystemCombinePhysicalMemoryInformation = 130`).
/// Returns the number of pages combined. Needs administrator rights (`SeProfileSingleProcessPrivilege`).
pub fn combine_physical_memory() -> Result<u64, String> {
    const SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION: u32 = 130;
    #[repr(C)]
    struct MemoryCombineInfoEx {
        handle: HANDLE,
        pages_combined: usize,
        flags: u64,
    }
    if !enable_privilege("SeProfileSingleProcessPrivilege") {
        return Err("needs administrator rights".into());
    }
    let mut info = MemoryCombineInfoEx { handle: null_mut(), pages_combined: 0, flags: 0 };
    let mut status = unsafe {
        NtSetSystemInformation(
            SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION,
            &mut info as *mut _ as *mut c_void,
            size_of::<MemoryCombineInfoEx>() as u32,
        )
    };
    if status == STATUS_INFO_LENGTH_MISMATCH {
        #[repr(C)]
        struct MemoryCombineInfo {
            handle: HANDLE,
            pages_combined: usize,
        }
        let mut basic = MemoryCombineInfo { handle: null_mut(), pages_combined: 0 };
        status = unsafe {
            NtSetSystemInformation(
                SYSTEM_COMBINE_PHYSICAL_MEMORY_INFORMATION,
                &mut basic as *mut _ as *mut c_void,
                size_of::<MemoryCombineInfo>() as u32,
            )
        };
        if status < 0 {
            return Err(format!("combining physical memory failed (status {:#x})", status as u32));
        }
        return Ok(basic.pages_combined as u64);
    }
    if status < 0 {
        return Err(format!("combining physical memory failed (status {:#x})", status as u32));
    }
    Ok(info.pages_combined as u64)
}

/// Why `run_elevated` didn't run the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevateError {
    /// The person said no to the Windows administrator prompt.
    Declined,
    Failed(String),
}

/// Start `exe args` with administrator rights (Windows asks the person first), hidden, and
/// wait up to `timeout_ms` for it to finish. Returns its exit code.
pub fn run_elevated(exe: &std::path::Path, args: &str, timeout_ms: u32) -> Result<u32, ElevateError> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_CANCELLED, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(args);
    let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = unsafe { GetLastError() };
        return Err(if e == ERROR_CANCELLED { ElevateError::Declined } else { ElevateError::Failed(format!("could not start it (error {})", e)) });
    }
    if info.hProcess.is_null() {
        return Err(ElevateError::Failed("Windows gave no process to wait for".into()));
    }
    let process = Handle(info.hProcess);
    if unsafe { WaitForSingleObject(process.0, timeout_ms) } == WAIT_TIMEOUT {
        return Err(ElevateError::Failed("it didn't finish in time".into()));
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(ElevateError::Failed("could not read how it ended".into()));
    }
    Ok(code)
}

// ---------------------------------------------------------------- hard caps (Job Objects)

#[derive(Debug, Clone, Copy, Default)]
struct JobLimits {
    per_process_bytes: u64,
    job_bytes: u64,
    kill_on_close: bool,
    min_ws_bytes: u64,
    max_ws_bytes: u64,
}

pub struct Job {
    handle: Handle,
    limits: Mutex<JobLimits>,
}

impl Job {
    /// Create a job with commit limits. 0 means "no limit" for either value.
    pub fn create(name: Option<&str>, per_process_bytes: u64, job_bytes: u64, kill_on_close: bool) -> Result<Job, String> {
        let wname = name.map(wide);
        let h = unsafe { CreateJobObjectW(null(), wname.as_ref().map_or(null(), |w| w.as_ptr())) };
        if h.is_null() {
            return Err(format!("CreateJobObject failed: {}", std::io::Error::last_os_error()));
        }
        let job = Job { handle: Handle(h), limits: Mutex::new(JobLimits::default()) };
        job.set_limits(per_process_bytes, job_bytes, kill_on_close)?;
        Ok(job)
    }

    fn apply_extended_limits(&self, l: &JobLimits) -> Result<(), String> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        if l.per_process_bytes > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = l.per_process_bytes as usize;
        }
        if l.job_bytes > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = l.job_bytes as usize;
        }
        if l.kill_on_close {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if l.max_ws_bytes > 0 {
            let min_floor = (l.max_ws_bytes / 4).clamp(20 * 4096, 1 << 20).min(l.max_ws_bytes);
            let min = l.min_ws_bytes.max(min_floor).min(l.max_ws_bytes);
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_WORKINGSET;
            info.BasicLimitInformation.MinimumWorkingSetSize = min as usize;
            info.BasicLimitInformation.MaximumWorkingSetSize = l.max_ws_bytes as usize;
        }
        let ok = unsafe {
            SetInformationJobObject(
                self.handle.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(format!("SetInformationJobObject failed: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn set_limits(&self, per_process_bytes: u64, job_bytes: u64, kill_on_close: bool) -> Result<(), String> {
        let mut limits = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = *limits;
        next.per_process_bytes = per_process_bytes;
        next.job_bytes = job_bytes;
        next.kill_on_close = kill_on_close;
        self.apply_extended_limits(&next)?;
        *limits = next;
        Ok(())
    }

    /// Set or clear working-set bounds on this job object without clobbering commit limits.
    pub fn set_working_set_limits(&self, min_bytes: u64, max_bytes: u64) -> Result<(), String> {
        let mut limits = self.limits.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = *limits;
        next.min_ws_bytes = min_bytes;
        next.max_ws_bytes = max_bytes;
        self.apply_extended_limits(&next)?;
        *limits = next;
        Ok(())
    }

    /// Set or clear a hard CPU rate cap (`1..=100` percent, or `None` / `Some(0)` to disable).
    pub fn set_cpu_rate(&self, max_pct: Option<u32>) -> Result<(), String> {
        let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { zeroed() };
        if let Some(pct) = max_pct.filter(|&p| p > 0) {
            info.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            info.Anonymous.CpuRate = pct.clamp(1, 100) * 100;
        }
        let ok = unsafe {
            SetInformationJobObject(
                self.handle.0,
                JobObjectCpuRateControlInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(format!("SetInformationJobObject(CpuRate) failed: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Add a running process. True if it is in the job afterwards.
    pub fn assign(&self, pid: u32) -> bool {
        let Some(h) = open(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION) else {
            return false;
        };
        self.assign_handle(h.0)
    }

    pub fn assign_handle(&self, process: HANDLE) -> bool {
        let mut in_job = 0;
        if unsafe { IsProcessInJob(process, self.handle.0, &mut in_job) } != 0 && in_job != 0 {
            return true;
        }
        unsafe { AssignProcessToJobObject(self.handle.0, process) != 0 }
    }
}

// ---------------------------------------------------------------- single instance + stop signal + wait + IPC

pub const WAIT_INFINITE: u32 = INFINITE;

/// Opaque borrowed synchronization handle usable with `wait_any` / `msg_wait_any`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitHandle(isize);

impl WaitHandle {
    fn raw(self) -> HANDLE {
        self.0 as HANDLE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    Signaled(usize),
    Message,
    Timeout,
    Failed,
}

impl Handle {
    pub fn wait_handle(&self) -> WaitHandle {
        WaitHandle(self.0 as isize)
    }
}

/// Take the "AtherMemGuard is running" mutex. Waits up to `wait_ms` for a previous copy to finish.
pub fn acquire_instance(name: impl AsRef<str>, wait_ms: u32) -> Option<Handle> {
    let w = wide(name.as_ref());
    let h = unsafe { CreateMutexW(null(), 0, w.as_ptr()) };
    if h.is_null() {
        return None;
    }
    let h = Handle(h);
    let r = unsafe { WaitForSingleObject(h.0, wait_ms) };
    if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
        Some(h)
    } else {
        None
    }
}

pub fn instance_running(name: impl AsRef<str>) -> bool {
    let w = wide(name.as_ref());
    let h = unsafe { OpenMutexW(SYNCHRONIZE, 0, w.as_ptr()) };
    if h.is_null() {
        return false;
    }
    let h = Handle(h);
    // The mutex object can outlive its owner for a moment; a running copy holds it.
    let r = unsafe { WaitForSingleObject(h.0, 0) };
    if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
        unsafe { windows_sys::Win32::System::Threading::ReleaseMutex(h.0) };
        false
    } else {
        true
    }
}

pub fn create_event(name: impl AsRef<str>) -> Option<Handle> {
    let w = wide(name.as_ref());
    let h = unsafe { CreateEventW(null(), 0, 0, w.as_ptr()) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

pub fn create_manual_event(name: Option<&str>, initial_state: bool) -> Option<Handle> {
    let w = name.map(wide);
    let ptr = w.as_ref().map_or(null(), |v| v.as_ptr());
    let h = unsafe { CreateEventW(null(), 1, if initial_state { 1 } else { 0 }, ptr) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

pub fn set_event_handle(h: &Handle) -> bool {
    unsafe { SetEvent(h.0) != 0 }
}

pub fn reset_event_handle(h: &Handle) -> bool {
    unsafe { ResetEvent(h.0) != 0 }
}

/// True if the event is signalled (and resets it, if it is auto-reset).
pub fn event_is_set(h: &Handle) -> bool {
    unsafe { WaitForSingleObject(h.0, 0) == WAIT_OBJECT_0 }
}

/// Signal a named event if it exists. Returns false if nothing is listening.
pub fn signal_event(name: impl AsRef<str>) -> bool {
    let w = wide(name.as_ref());
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) };
    if h.is_null() {
        return false;
    }
    let h = Handle(h);
    unsafe { SetEvent(h.0) != 0 }
}

pub fn wait_any(handles: &[WaitHandle], timeout_ms: u32) -> WaitOutcome {
    if handles.is_empty() {
        if timeout_ms == INFINITE {
            return WaitOutcome::Failed;
        }
        std::thread::sleep(Duration::from_millis(timeout_ms as u64));
        return WaitOutcome::Timeout;
    }
    let raw: Vec<HANDLE> = handles.iter().map(|h| h.raw()).collect();
    let n = raw.len() as u32;
    let r = unsafe { WaitForMultipleObjects(n, raw.as_ptr(), 0, timeout_ms) };
    if r >= WAIT_OBJECT_0 && r < WAIT_OBJECT_0 + n {
        WaitOutcome::Signaled((r - WAIT_OBJECT_0) as usize)
    } else if r >= WAIT_ABANDONED && r < WAIT_ABANDONED + n {
        WaitOutcome::Signaled((r - WAIT_ABANDONED) as usize)
    } else if r == WAIT_TIMEOUT {
        WaitOutcome::Timeout
    } else {
        WaitOutcome::Failed
    }
}

pub fn msg_wait_any(handles: &[WaitHandle], timeout_ms: u32) -> WaitOutcome {
    let raw: Vec<HANDLE> = handles.iter().map(|h| h.raw()).collect();
    let n = raw.len() as u32;
    let ptr = if raw.is_empty() { null() } else { raw.as_ptr() };
    let r = unsafe { MsgWaitForMultipleObjectsEx(n, ptr, timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
    if r >= WAIT_OBJECT_0 && r < WAIT_OBJECT_0 + n {
        WaitOutcome::Signaled((r - WAIT_OBJECT_0) as usize)
    } else if r == WAIT_OBJECT_0 + n {
        WaitOutcome::Message
    } else if r >= WAIT_ABANDONED && r < WAIT_ABANDONED + n {
        WaitOutcome::Signaled((r - WAIT_ABANDONED) as usize)
    } else if r == WAIT_TIMEOUT {
        WaitOutcome::Timeout
    } else {
        WaitOutcome::Failed
    }
}

pub fn set_coalescable_timer(hwnd: HWND, id: usize, elapse_ms: u32, tolerance_ms: u32) -> bool {
    unsafe { SetCoalescableTimer(hwnd, id, elapse_ms, None, tolerance_ms) != 0 }
}

/// Let whichever process we wake up next bring its window to the front (a signalled tray
/// opening its settings window). Only works while this process may set the foreground.
pub fn allow_any_foreground() {
    const ASFW_ANY: u32 = u32::MAX;
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(ASFW_ANY) };
}

pub fn null_handle() -> HANDLE {
    null_mut()
}

/// Handle signalled by Windows whenever physical memory drops into a low-memory state.
pub fn low_memory_event() -> Option<Handle> {
    let h = unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification) };
    if h.is_null() { None } else { Some(Handle(h)) }
}

/// Directory change-notification handle (`FindFirstChangeNotificationW`), closed on drop.
pub struct DirWatch(HANDLE);

unsafe impl Send for DirWatch {}

impl Drop for DirWatch {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { FindCloseChangeNotification(self.0) };
        }
    }
}

impl DirWatch {
    pub fn raw_handle(&self) -> HANDLE {
        self.0
    }

    pub fn wait_handle(&self) -> WaitHandle {
        WaitHandle(self.0 as isize)
    }

    pub fn is_signaled(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }

    pub fn next(&self) -> bool {
        unsafe { FindNextChangeNotification(self.0) != 0 }
    }
}

/// Watch `path` (non-recursively) for file creations/deletions/renames/writes.
pub fn watch_dir_handle(path: &Path) -> Option<DirWatch> {
    let w = wide(&path.to_string_lossy());
    let h = unsafe {
        FindFirstChangeNotificationW(w.as_ptr(), 0, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE)
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(DirWatch(h))
    }
}

// ---------------------------------------------------------------- overlapped named pipes & shared status

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const ERROR_NO_DATA_CODE: u32 = 232;

struct SyncHandle(Handle);

unsafe impl Send for SyncHandle {}
unsafe impl Sync for SyncHandle {}

#[derive(Clone)]
pub struct PipeShutdown(Arc<SyncHandle>);

impl PipeShutdown {
    pub fn signal(&self) {
        let _ = set_event_handle(&self.0 .0);
    }

    pub fn is_signaled(&self) -> bool {
        unsafe { WaitForSingleObject((self.0 .0).0, 0) == WAIT_OBJECT_0 }
    }
}

pub struct PipeServer {
    wide_name: Vec<u16>,
    shutdown: PipeShutdown,
    pending: Option<Handle>,
}

fn create_pipe_instance(wide_name: &[u16]) -> Result<Handle, String> {
    let h = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            5000,
            null(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(format!("CreateNamedPipeW failed (error {})", unsafe { GetLastError() }))
    } else {
        Ok(Handle(h))
    }
}

impl PipeServer {
    pub fn bind(name: &str) -> Result<(Self, PipeShutdown), String> {
        let wide_name = wide(name);
        let ev = create_manual_event(None, false).ok_or_else(|| "create_manual_event failed".to_string())?;
        let shutdown = PipeShutdown(Arc::new(SyncHandle(ev)));
        let first = create_pipe_instance(&wide_name)?;
        Ok((
            Self { wide_name, shutdown: shutdown.clone(), pending: Some(first) },
            shutdown,
        ))
    }

    pub fn accept(&mut self) -> Result<Option<PipeConn>, String> {
        loop {
            if self.shutdown.is_signaled() {
                return Ok(None);
            }
            let inst = match self.pending.take() {
                Some(h) => h,
                None => create_pipe_instance(&self.wide_name)?,
            };
            let ov_ev = create_manual_event(None, false).ok_or_else(|| "create_manual_event failed".to_string())?;
            let mut ov: OVERLAPPED = unsafe { zeroed() };
            ov.hEvent = ov_ev.0;

            let ok = unsafe { ConnectNamedPipe(inst.0, &mut ov) };
            let connected = if ok != 0 {
                true
            } else {
                let err = unsafe { GetLastError() };
                if err == ERROR_PIPE_CONNECTED {
                    true
                } else if err == ERROR_IO_PENDING {
                    let handles = [ov_ev.0, (self.shutdown.0 .0).0];
                    let w = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                    if w == WAIT_OBJECT_0 + 1 {
                        unsafe { CancelIoEx(inst.0, &ov) };
                        let mut dummy = 0u32;
                        unsafe { GetOverlappedResult(inst.0, &ov, &mut dummy, 1) };
                        return Ok(None);
                    }
                    if w != WAIT_OBJECT_0 {
                        unsafe { CancelIoEx(inst.0, &ov) };
                        let mut dummy = 0u32;
                        unsafe { GetOverlappedResult(inst.0, &ov, &mut dummy, 1) };
                        return Err("WaitForMultipleObjects on ConnectNamedPipe failed".to_string());
                    }
                    let mut dummy = 0u32;
                    let res = unsafe { GetOverlappedResult(inst.0, &ov, &mut dummy, 0) };
                    res != 0
                } else if err == ERROR_NO_DATA_CODE {
                    false
                } else {
                    return Err(format!("ConnectNamedPipe failed (error {})", err));
                }
            };

            if !connected {
                continue;
            }
            // Pre-arm next instance immediately so back-to-back clients never hit a gap.
            self.pending = create_pipe_instance(&self.wide_name).ok();
            return Ok(Some(PipeConn { handle: inst }));
        }
    }
}

pub struct PipeConn {
    handle: Handle,
}

impl Drop for PipeConn {
    fn drop(&mut self) {
        unsafe { DisconnectNamedPipe(self.handle.0) };
    }
}

impl PipeConn {
    pub fn read_line_timeout(&self, max_bytes: usize, timeout_ms: u32) -> Result<String, String> {
        let mut out = Vec::with_capacity(256);
        let mut chunk = [0u8; 512];
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let ov_ev = create_manual_event(None, false).ok_or_else(|| "create_manual_event failed".to_string())?;

        while out.len() < max_bytes {
            let rem_ms = deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128) as u32;
            if rem_ms == 0 {
                return Err("pipe read timeout".to_string());
            }
            let _ = reset_event_handle(&ov_ev);
            let mut ov: OVERLAPPED = unsafe { zeroed() };
            ov.hEvent = ov_ev.0;
            let to_read = chunk.len().min(max_bytes - out.len()) as u32;
            let mut read = 0u32;
            let ok = unsafe { ReadFile(self.handle.0, chunk.as_mut_ptr(), to_read, &mut read, &mut ov) };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_IO_PENDING {
                    let w = unsafe { WaitForSingleObject(ov_ev.0, rem_ms) };
                    if w != WAIT_OBJECT_0 {
                        unsafe { CancelIoEx(self.handle.0, &ov) };
                        let mut dummy = 0u32;
                        unsafe { GetOverlappedResult(self.handle.0, &ov, &mut dummy, 1) };
                        return Err("pipe read timeout".to_string());
                    }
                    if unsafe { GetOverlappedResult(self.handle.0, &ov, &mut read, 0) } == 0 {
                        let ge = unsafe { GetLastError() };
                        if ge == ERROR_BROKEN_PIPE {
                            break;
                        }
                        return Err(format!("GetOverlappedResult(ReadFile) failed (error {})", ge));
                    }
                } else if err == ERROR_BROKEN_PIPE {
                    break;
                } else {
                    return Err(format!("ReadFile failed (error {})", err));
                }
            }
            if read == 0 {
                break;
            }
            let slice = &chunk[..read as usize];
            if let Some(pos) = slice.iter().position(|&b| b == b'\n') {
                out.extend_from_slice(&slice[..pos]);
                break;
            }
            out.extend_from_slice(slice);
        }
        String::from_utf8(out).map_err(|e| format!("invalid utf-8 on pipe: {}", e))
    }

    pub fn write_all_flush(&self, data: &[u8], timeout_ms: u32) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let ov_ev = create_manual_event(None, false).ok_or_else(|| "create_manual_event failed".to_string())?;
        let mut off = 0usize;

        while off < data.len() {
            let rem_ms = deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128) as u32;
            if rem_ms == 0 {
                return Err("pipe write timeout".to_string());
            }
            let _ = reset_event_handle(&ov_ev);
            let mut ov: OVERLAPPED = unsafe { zeroed() };
            ov.hEvent = ov_ev.0;
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    self.handle.0,
                    data[off..].as_ptr(),
                    (data.len() - off) as u32,
                    &mut written,
                    &mut ov,
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_IO_PENDING {
                    let w = unsafe { WaitForSingleObject(ov_ev.0, rem_ms) };
                    if w != WAIT_OBJECT_0 {
                        unsafe { CancelIoEx(self.handle.0, &ov) };
                        let mut dummy = 0u32;
                        unsafe { GetOverlappedResult(self.handle.0, &ov, &mut dummy, 1) };
                        return Err("pipe write timeout".to_string());
                    }
                    if unsafe { GetOverlappedResult(self.handle.0, &ov, &mut written, 0) } == 0 {
                        return Err(format!("GetOverlappedResult(WriteFile) failed (error {})", unsafe { GetLastError() }));
                    }
                } else {
                    return Err(format!("WriteFile failed (error {})", err));
                }
            }
            if written == 0 {
                return Err("pipe wrote 0 bytes".to_string());
            }
            off += written as usize;
        }
        unsafe { FlushFileBuffers(self.handle.0) };
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeError {
    NotFound,
    Busy,
    Other(String),
}

pub fn pipe_call(name: &str, request: &[u8], timeout_ms: u32) -> Result<Vec<u8>, PipeError> {
    let w = wide(name);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    let handle = loop {
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            break Handle(h);
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_FILE_NOT_FOUND {
            return Err(PipeError::NotFound);
        }
        if err == ERROR_PIPE_BUSY {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(PipeError::Busy);
            }
            let wait_ms = (rem.as_millis() as u32).clamp(10, 250);
            unsafe { WaitNamedPipeW(w.as_ptr(), wait_ms) };
            continue;
        }
        return Err(PipeError::Other(format!("CreateFileW({}) failed (error {})", name, err)));
    };

    let mut off = 0usize;
    while off < request.len() {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle.0,
                request[off..].as_ptr(),
                (request.len() - off) as u32,
                &mut written,
                null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            return Err(PipeError::Other(format!("WriteFile failed (error {})", unsafe { GetLastError() })));
        }
        off += written as usize;
    }

    let mut resp = Vec::with_capacity(256);
    let mut buf = [0u8; 512];
    while resp.len() < 64 * 1024 {
        let mut read = 0u32;
        let ok = unsafe { ReadFile(handle.0, buf.as_mut_ptr(), buf.len() as u32, &mut read, null_mut()) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_BROKEN_PIPE {
                break;
            }
            return Err(PipeError::Other(format!("ReadFile failed (error {})", err)));
        }
        if read == 0 {
            break;
        }
        let slice = &buf[..read as usize];
        if let Some(pos) = slice.iter().position(|&b| b == b'\n') {
            resp.extend_from_slice(&slice[..pos]);
            break;
        }
        resp.extend_from_slice(slice);
    }
    Ok(resp)
}

const STATUS_SHM_MAGIC: u32 = 0x414D_4753; // "AMGS"
const STATUS_SHM_HDR_LEN: usize = 16;
const STATUS_SHM_CAP: usize = 256 * 1024;

struct MappedView(*mut c_void);

impl Drop for MappedView {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.0 }) };
        }
    }
}

pub struct SharedMappingWriter {
    _mapping: Handle,
    view: *mut u8,
}

unsafe impl Send for SharedMappingWriter {}

impl Drop for SharedMappingWriter {
    fn drop(&mut self) {
        if !self.view.is_null() {
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut c_void }) };
        }
    }
}

impl SharedMappingWriter {
    pub fn create(name: &str) -> Option<Self> {
        let w = wide(name);
        let h = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                0,
                STATUS_SHM_CAP as u32,
                w.as_ptr(),
            )
        };
        if h.is_null() {
            return None;
        }
        let mapping = Handle(h);
        let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_WRITE, 0, 0, STATUS_SHM_CAP) };
        if view.Value.is_null() {
            return None;
        }
        let ptr = view.Value as *mut u8;
        unsafe {
            std::ptr::write_volatile(ptr as *mut u32, STATUS_SHM_MAGIC);
            let seq = &*(ptr.add(4) as *const AtomicU32);
            seq.store(0, Ordering::Release);
            std::ptr::write_volatile(ptr.add(8) as *mut u32, 0);
            std::ptr::write_volatile(ptr.add(12) as *mut u32, 0);
        }
        Some(Self { _mapping: mapping, view: ptr })
    }

    pub fn write(&mut self, data: &[u8]) -> bool {
        if data.len() > STATUS_SHM_CAP - STATUS_SHM_HDR_LEN {
            return false;
        }
        unsafe {
            let seq = &*(self.view.add(4) as *const AtomicU32);
            let cur = seq.load(Ordering::Relaxed);
            let odd = (cur.wrapping_add(1)) | 1;
            seq.store(odd, Ordering::Release);
            fence(Ordering::SeqCst);
            std::ptr::write_volatile(self.view.add(8) as *mut u32, data.len() as u32);
            if !data.is_empty() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), self.view.add(STATUS_SHM_HDR_LEN), data.len());
            }
            fence(Ordering::SeqCst);
            seq.store(odd.wrapping_add(1), Ordering::Release);
        }
        true
    }
}

pub fn read_shared_mapping(name: &str) -> Option<Vec<u8>> {
    let w = wide(name);
    let h = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, w.as_ptr()) };
    if h.is_null() {
        return None;
    }
    let mapping = Handle(h);
    let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, STATUS_SHM_CAP) };
    if view.Value.is_null() {
        return None;
    }
    let _guard = MappedView(view.Value);
    let ptr = view.Value as *const u8;

    for _ in 0..4 {
        let magic = unsafe { std::ptr::read_volatile(ptr as *const u32) };
        if magic != STATUS_SHM_MAGIC {
            return None;
        }
        let seq = unsafe { &*(ptr.add(4) as *const AtomicU32) };
        let s1 = seq.load(Ordering::Acquire);
        if s1 == 0 || (s1 & 1) != 0 {
            std::thread::yield_now();
            continue;
        }
        fence(Ordering::SeqCst);
        let len = unsafe { std::ptr::read_volatile(ptr.add(8) as *const u32) } as usize;
        if len > STATUS_SHM_CAP - STATUS_SHM_HDR_LEN {
            std::thread::yield_now();
            continue;
        }
        let mut out = vec![0u8; len];
        if len > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(ptr.add(STATUS_SHM_HDR_LEN), out.as_mut_ptr(), len);
            }
        }
        fence(Ordering::SeqCst);
        let s2 = seq.load(Ordering::Acquire);
        if s1 == s2 {
            return Some(out);
        }
    }
    None
}

// ---------------------------------------------------------------- process launch & env broadcast

/// Tell running programs that environment variables changed (after a PATH edit).
pub fn broadcast_environment_change() {
    let env = wide("Environment");
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            env.as_ptr() as LPARAM,
            SMTO_ABORTIFHUNG,
            2000,
            &mut result,
        );
    }
}

/// Start a program with no inherited handles, so it can't hold a caller's output pipe open.
pub fn launch_detached(file: &Path, params: &str, hidden: bool) -> bool {
    let (op, f, p) = (wide("open"), wide(&file.display().to_string()), wide(params));
    let show = if hidden { 0 } else { SW_SHOWNORMAL };
    let r = unsafe { ShellExecuteW(null_mut(), op.as_ptr(), f.as_ptr(), p.as_ptr(), null(), show) };
    r as usize > 32
}

// ---------------------------------------------------------------- LZNT1 compression

const COMPRESSION_FORMAT_LZNT1: u16 = 0x0002;
const COMPRESSION_ENGINE_MAXIMUM: u16 = 0x0100;
const LZNT1_FORMAT_ENGINE: u16 = COMPRESSION_FORMAT_LZNT1 | COMPRESSION_ENGINE_MAXIMUM;
const LZNT1_CHUNK_SIZE: u32 = 4096;

pub fn compress_lznt1(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let len = u32::try_from(data.len()).map_err(|_| "input exceeds 4 GB".to_string())?;
    let mut ws_size = 0u32;
    let mut frag_size = 0u32;
    let status = unsafe { RtlGetCompressionWorkSpaceSize(LZNT1_FORMAT_ENGINE, &mut ws_size, &mut frag_size) };
    if status < 0 {
        return Err(format!("RtlGetCompressionWorkSpaceSize failed: {:#x}", status as u32));
    }
    let mut workspace = vec![0u8; ws_size as usize];
    let cap = data.len() + (data.len() / LZNT1_CHUNK_SIZE as usize + 1) * 4 + 64;
    let mut out = vec![0u8; cap];
    let out_cap = u32::try_from(out.len()).map_err(|_| "compressed buffer exceeds 4 GB".to_string())?;
    let mut final_size = 0u32;
    let status = unsafe {
        RtlCompressBuffer(
            LZNT1_FORMAT_ENGINE,
            data.as_ptr(),
            len,
            out.as_mut_ptr(),
            out_cap,
            LZNT1_CHUNK_SIZE,
            &mut final_size,
            workspace.as_mut_ptr() as *mut c_void,
        )
    };
    if status < 0 {
        return Err(format!("RtlCompressBuffer failed: {:#x}", status as u32));
    }
    out.truncate(final_size as usize);
    Ok(out)
}

/// Decompress LZNT1 `data` into a buffer of up to `uncompressed_len` bytes, returning the exact
/// `final_size` bytes written (callers expecting a fixed size must check `out.len()`).
pub fn decompress_lznt1(data: &[u8], uncompressed_len: usize) -> Result<Vec<u8>, String> {
    if uncompressed_len == 0 {
        return if data.is_empty() {
            Ok(Vec::new())
        } else {
            Err("unexpected compressed bytes for zero uncompressed length".to_string())
        };
    }
    let out_len = u32::try_from(uncompressed_len).map_err(|_| "output exceeds 4 GB".to_string())?;
    let in_len = u32::try_from(data.len()).map_err(|_| "input exceeds 4 GB".to_string())?;
    let mut out = vec![0u8; uncompressed_len];
    let mut final_size = 0u32;
    let status = unsafe {
        RtlDecompressBuffer(
            COMPRESSION_FORMAT_LZNT1,
            out.as_mut_ptr(),
            out_len,
            data.as_ptr(),
            in_len,
            &mut final_size,
        )
    };
    if status < 0 {
        return Err(format!("RtlDecompressBuffer failed: {:#x}", status as u32));
    }
    out.truncate(final_size as usize);
    Ok(out)
}

// ---------------------------------------------------------------- drivers & pool tags

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPoolTag {
    pub tag: [u8; 4],
    pub paged_used: u64,
    pub nonpaged_used: u64,
}

#[cfg(target_pointer_width = "64")]
fn parse_pool_tags(buf: &[u8]) -> Vec<RawPoolTag> {
    const ENTRY_SIZE: usize = 40;
    const ENTRIES_OFFSET: usize = 8;
    let u32_at = |o: usize| buf.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u64_at = |o: usize| buf.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let Some(count) = u32_at(0) else { return Vec::new() };
    let fits = buf.len().saturating_sub(ENTRIES_OFFSET) / ENTRY_SIZE;
    let n = (count as usize).min(fits);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let o = ENTRIES_OFFSET + i * ENTRY_SIZE;
        let (Some(paged_used), Some(nonpaged_used)) = (u64_at(o + 16), u64_at(o + 32)) else { break };
        let mut tag = [0u8; 4];
        tag.copy_from_slice(&buf[o..o + 4]);
        out.push(RawPoolTag { tag, paged_used, nonpaged_used });
    }
    out
}

#[cfg(not(target_pointer_width = "64"))]
fn parse_pool_tags(_buf: &[u8]) -> Vec<RawPoolTag> {
    Vec::new()
}

/// Live kernel pool tag allocations from `NtQuerySystemInformation(SystemPoolTagInformation = 22)`.
pub fn pool_tag_info() -> Option<Vec<RawPoolTag>> {
    const SYSTEM_POOL_TAG_INFORMATION: u32 = 22;
    let mut buf = vec![0u64; 64 * 1024]; // 512 KB
    for _ in 0..8 {
        let bytes = (buf.len() * 8) as u32;
        let mut ret = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(SYSTEM_POOL_TAG_INFORMATION, buf.as_mut_ptr() as *mut c_void, bytes, &mut ret)
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            let want = (ret as usize).max(buf.len() * 16) + 64 * 1024;
            buf = vec![0u64; want / 8 + 1];
            continue;
        }
        if status < 0 {
            return None;
        }
        let used = (ret as usize).min(buf.len() * 8);
        let all = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
        let slice = if used >= 8 { &all[..used] } else { all };
        return Some(parse_pool_tags(slice));
    }
    None
}

fn normalize_nt_driver_path(raw: &str, sys_root: &str) -> PathBuf {
    let stripped = if raw.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"\??\")) {
        &raw[4..]
    } else {
        raw
    };
    if stripped.get(..12).is_some_and(|p| p.eq_ignore_ascii_case(r"\SystemRoot\")) {
        return PathBuf::from(format!("{}\\{}", sys_root.trim_end_matches('\\'), &stripped[12..]));
    }
    if stripped.get(..9).is_some_and(|p| p.eq_ignore_ascii_case(r"System32\")) {
        return PathBuf::from(format!("{}\\{}", sys_root.trim_end_matches('\\'), stripped));
    }
    PathBuf::from(stripped)
}

/// Paths of currently loaded kernel drivers via `NtQuerySystemInformation(SystemModuleInformation = 11)`.
pub fn loaded_driver_paths() -> Vec<PathBuf> {
    const SYSTEM_MODULE_INFORMATION: u32 = 11;
    #[cfg(target_pointer_width = "64")]
    const BASE_OFFSET: usize = 8;
    #[cfg(not(target_pointer_width = "64"))]
    const BASE_OFFSET: usize = 4;
    #[cfg(target_pointer_width = "64")]
    const ENTRY_STRIDE: usize = 296;
    #[cfg(not(target_pointer_width = "64"))]
    const ENTRY_STRIDE: usize = 284;
    #[cfg(target_pointer_width = "64")]
    const PATH_OFFSET: usize = 40;
    #[cfg(not(target_pointer_width = "64"))]
    const PATH_OFFSET: usize = 28;

    let mut buf = vec![0u64; 32 * 1024]; // 256 KB
    let mut ret = 0u32;
    let mut status = STATUS_INFO_LENGTH_MISMATCH;
    for _ in 0..8 {
        let bytes = (buf.len() * 8) as u32;
        status = unsafe {
            NtQuerySystemInformation(SYSTEM_MODULE_INFORMATION, buf.as_mut_ptr() as *mut c_void, bytes, &mut ret)
        };
        if status != STATUS_INFO_LENGTH_MISMATCH {
            break;
        }
        let want = (ret as usize).max(buf.len() * 16) + 64 * 1024;
        buf = vec![0u64; want.div_ceil(8)];
    }
    if status < 0 {
        return Vec::new();
    }
    let used = (ret as usize).min(buf.len() * 8);
    let raw_bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
    let slice = if used >= BASE_OFFSET { &raw_bytes[..used] } else { raw_bytes };
    let Some(count_bytes) = slice.get(0..4) else {
        return Vec::new();
    };
    let count = u32::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
    let fits = slice.len().saturating_sub(BASE_OFFSET) / ENTRY_STRIDE;
    let n = count.min(fits);
    let sys_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let mut seen: HashSet<String> = HashSet::with_capacity(n);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let entry_off = BASE_OFFSET + i * ENTRY_STRIDE;
        let path_slice = &slice[entry_off + PATH_OFFSET..entry_off + PATH_OFFSET + 256];
        let end = path_slice.iter().position(|&b| b == 0).unwrap_or(path_slice.len());
        if end == 0 {
            continue;
        }
        let raw_path = String::from_utf8_lossy(&path_slice[..end]);
        let path = normalize_nt_driver_path(raw_path.as_ref(), &sys_root);
        let key = path.to_string_lossy().to_ascii_lowercase();
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

// ---------------------------------------------------------------- safe registry wrapper

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegHive {
    Hkcu,
    Hklm,
}

impl RegHive {
    fn raw(self) -> HKEY {
        match self {
            RegHive::Hkcu => HKEY_CURRENT_USER,
            RegHive::Hklm => HKEY_LOCAL_MACHINE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegError {
    NotFound,
    Os(u32, String),
}

impl fmt::Display for RegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegError::NotFound => write!(f, "registry key or value not found"),
            RegError::Os(code, ctx) => write!(f, "{} (Win32 error {})", ctx, code),
        }
    }
}

impl std::error::Error for RegError {}

fn reg_err(code: u32, ctx: &str) -> RegError {
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
        RegError::NotFound
    } else {
        RegError::Os(code, ctx.to_string())
    }
}

pub struct RegKey(HKEY);

unsafe impl Send for RegKey {}

impl Drop for RegKey {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { RegCloseKey(self.0) };
        }
    }
}

fn utf16_bytes_to_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() / 2);
    for r in char::decode_utf16(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]])),
    ) {
        let ch = r.unwrap_or(char::REPLACEMENT_CHARACTER);
        if ch == '\0' {
            break;
        }
        s.push(ch);
    }
    s
}

pub const REG_EXPAND_SZ_TYPE: u32 = REG_EXPAND_SZ;

pub fn decode_reg_string(bytes: &[u8]) -> String {
    utf16_bytes_to_string(bytes)
}

pub fn reg_value_to_string(vtype: u32, bytes: &[u8]) -> Option<String> {
    (vtype == REG_SZ || vtype == REG_EXPAND_SZ).then(|| utf16_bytes_to_string(bytes))
}

fn utf16_bytes_to_multi_string(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for r in char::decode_utf16(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]])),
    ) {
        let ch = r.unwrap_or(char::REPLACEMENT_CHARACTER);
        if ch == '\0' {
            if cur.is_empty() {
                break;
            }
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl RegKey {
    pub fn open_with_access(hive: RegHive, subkey: &str, access: u32) -> Result<RegKey, RegError> {
        let w = wide(subkey);
        let mut h: HKEY = null_mut();
        let rc = unsafe { RegOpenKeyExW(hive.raw(), w.as_ptr(), 0, access, &mut h) };
        if rc != 0 {
            return Err(reg_err(rc, "RegOpenKeyExW"));
        }
        Ok(RegKey(h))
    }

    pub fn open_read(hive: RegHive, subkey: &str) -> Result<RegKey, RegError> {
        Self::open_with_access(hive, subkey, KEY_READ)
    }

    pub fn open_write(hive: RegHive, subkey: &str) -> Result<RegKey, RegError> {
        Self::open_with_access(hive, subkey, KEY_READ | KEY_WRITE)
    }

    pub fn open_set_value(hive: RegHive, subkey: &str) -> Result<RegKey, RegError> {
        Self::open_with_access(hive, subkey, KEY_SET_VALUE)
    }

    pub fn create(hive: RegHive, subkey: &str) -> Result<RegKey, RegError> {
        let w = wide(subkey);
        let mut h: HKEY = null_mut();
        let rc = unsafe {
            RegCreateKeyExW(
                hive.raw(),
                w.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                null(),
                &mut h,
                null_mut(),
            )
        };
        if rc != 0 {
            return Err(reg_err(rc, "RegCreateKeyExW"));
        }
        Ok(RegKey(h))
    }

    pub fn get_raw(&self, name: &str) -> Result<(u32, Vec<u8>), RegError> {
        let w = wide(name);
        let mut vtype = 0u32;
        let mut size = 0u32;
        let rc = unsafe { RegQueryValueExW(self.0, w.as_ptr(), null(), &mut vtype, null_mut(), &mut size) };
        if rc != 0 {
            return Err(reg_err(rc, "RegQueryValueExW(size)"));
        }
        let mut buf = vec![0u8; size as usize];
        loop {
            let mut len = buf.len() as u32;
            let rc = unsafe { RegQueryValueExW(self.0, w.as_ptr(), null(), &mut vtype, buf.as_mut_ptr(), &mut len) };
            if rc == ERROR_MORE_DATA {
                buf.resize(len as usize, 0);
                continue;
            }
            if rc != 0 {
                return Err(reg_err(rc, "RegQueryValueExW(data)"));
            }
            buf.truncate(len as usize);
            return Ok((vtype, buf));
        }
    }

    pub fn get_string(&self, name: &str) -> Result<String, RegError> {
        let (vtype, bytes) = self.get_raw(name)?;
        if vtype != REG_SZ && vtype != REG_EXPAND_SZ {
            return Err(RegError::Os(0, format!("unexpected registry value type {}", vtype)));
        }
        Ok(utf16_bytes_to_string(&bytes))
    }

    pub fn get_string_with_expand(&self, name: &str) -> Result<(String, bool), RegError> {
        let (vtype, bytes) = self.get_raw(name)?;
        if vtype != REG_SZ && vtype != REG_EXPAND_SZ {
            return Err(RegError::Os(0, format!("unexpected registry value type {}", vtype)));
        }
        Ok((utf16_bytes_to_string(&bytes), vtype == REG_EXPAND_SZ))
    }

    pub fn get_multi_string(&self, name: &str) -> Result<Vec<String>, RegError> {
        let (vtype, bytes) = self.get_raw(name)?;
        if vtype != REG_MULTI_SZ {
            return Err(RegError::Os(0, format!("unexpected registry value type {}", vtype)));
        }
        Ok(utf16_bytes_to_multi_string(&bytes))
    }

    pub fn get_dword(&self, name: &str) -> Result<u32, RegError> {
        let (vtype, bytes) = self.get_raw(name)?;
        if vtype != REG_DWORD || bytes.len() < 4 {
            return Err(RegError::Os(0, format!("unexpected registry value type {}", vtype)));
        }
        Ok(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
    }

    pub fn set_raw(&self, name: &str, reg_type: u32, data: &[u8]) -> Result<(), RegError> {
        let w = wide(name);
        let rc = unsafe { RegSetValueExW(self.0, w.as_ptr(), 0, reg_type, data.as_ptr(), data.len() as u32) };
        if rc != 0 {
            return Err(reg_err(rc, "RegSetValueExW"));
        }
        Ok(())
    }

    pub fn set_string(&self, name: &str, value: &str, expand: bool) -> Result<(), RegError> {
        let w = wide(value);
        let bytes: Vec<u8> = w.into_iter().flat_map(u16::to_le_bytes).collect();
        self.set_raw(name, if expand { REG_EXPAND_SZ } else { REG_SZ }, &bytes)
    }

    pub fn set_dword(&self, name: &str, value: u32) -> Result<(), RegError> {
        self.set_raw(name, REG_DWORD, &value.to_le_bytes())
    }

    pub fn set_binary(&self, name: &str, data: &[u8]) -> Result<(), RegError> {
        self.set_raw(name, REG_BINARY, data)
    }

    pub fn delete_value(&self, name: &str) -> Result<(), RegError> {
        let w = wide(name);
        let rc = unsafe { RegDeleteValueW(self.0, w.as_ptr()) };
        if rc != 0 {
            return Err(reg_err(rc, "RegDeleteValueW"));
        }
        Ok(())
    }

    pub fn enum_keys(&self) -> Result<Vec<String>, RegError> {
        let mut out = Vec::new();
        let mut idx = 0u32;
        let mut name_buf = vec![0u16; 260];
        loop {
            let mut name_len = name_buf.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    idx,
                    name_buf.as_mut_ptr(),
                    &mut name_len,
                    null(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc == ERROR_MORE_DATA {
                name_buf.resize(name_buf.len() * 2, 0);
                continue;
            }
            if rc != 0 {
                return Err(reg_err(rc, "RegEnumKeyExW"));
            }
            out.push(String::from_utf16_lossy(&name_buf[..name_len as usize]));
            idx += 1;
        }
        Ok(out)
    }

    pub fn enum_values(&self) -> Result<Vec<(String, u32, Vec<u8>)>, RegError> {
        let mut out = Vec::new();
        let mut idx = 0u32;
        let mut name_buf = vec![0u16; 256];
        let mut data_buf = vec![0u8; 512];
        loop {
            let mut name_len = name_buf.len() as u32;
            let mut vtype = 0u32;
            let mut data_len = data_buf.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    self.0,
                    idx,
                    name_buf.as_mut_ptr(),
                    &mut name_len,
                    null(),
                    &mut vtype,
                    data_buf.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc == ERROR_MORE_DATA {
                let mut grew = false;
                if data_len as usize > data_buf.len() {
                    data_buf.resize(data_len as usize, 0);
                    grew = true;
                }
                if name_len as usize + 1 >= name_buf.len() || !grew {
                    if name_buf.len() >= 32_768 {
                        return Err(reg_err(rc, "RegEnumValueW"));
                    }
                    name_buf.resize((name_buf.len() * 2).min(32_768), 0);
                }
                continue;
            }
            if rc != 0 {
                return Err(reg_err(rc, "RegEnumValueW"));
            }
            let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            out.push((name, vtype, data_buf[..data_len as usize].to_vec()));
            idx += 1;
        }
        Ok(out)
    }
}

pub fn reg_get_string(hive: RegHive, subkey: &str, name: &str) -> Result<String, RegError> {
    RegKey::open_read(hive, subkey)?.get_string(name)
}

pub fn reg_get_dword(hive: RegHive, subkey: &str, name: &str) -> Result<u32, RegError> {
    RegKey::open_read(hive, subkey)?.get_dword(name)
}

pub fn reg_set_string(hive: RegHive, subkey: &str, name: &str, value: &str, expand: bool) -> Result<(), RegError> {
    RegKey::create(hive, subkey)?.set_string(name, value, expand)
}

pub fn reg_set_dword(hive: RegHive, subkey: &str, name: &str, value: u32) -> Result<(), RegError> {
    RegKey::create(hive, subkey)?.set_dword(name, value)
}

pub fn reg_delete_value(hive: RegHive, subkey: &str, name: &str) -> Result<(), RegError> {
    RegKey::open_write(hive, subkey)?.delete_value(name)
}

pub fn reg_delete_tree(hive: RegHive, subkey: &str) -> Result<(), RegError> {
    let w = wide(subkey);
    let rc = unsafe { RegDeleteTreeW(hive.raw(), w.as_ptr()) };
    if rc != 0 {
        return Err(reg_err(rc, "RegDeleteTreeW"));
    }
    Ok(())
}

pub fn reg_delete_key(hive: RegHive, subkey: &str) -> Result<(), RegError> {
    let w = wide(subkey);
    let rc = unsafe { RegDeleteKeyW(hive.raw(), w.as_ptr()) };
    if rc != 0 {
        return Err(reg_err(rc, "RegDeleteKeyW"));
    }
    Ok(())
}

pub fn reg_enum_values(hive: RegHive, subkey: &str) -> Result<Vec<(String, u32, Vec<u8>)>, RegError> {
    RegKey::open_read(hive, subkey)?.enum_values()
}

pub fn reg_enum_keys(hive: RegHive, subkey: &str) -> Result<Vec<String>, RegError> {
    RegKey::open_read(hive, subkey)?.enum_keys()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushing_the_cache_needs_admin() {
        // Elevated, this would really empty the cache: only the refusal is tested.
        if !is_elevated() {
            assert_eq!(flush_file_cache(), Err("needs administrator rights".to_string()));
            assert_eq!(flush_modified_list(), Err("needs administrator rights".to_string()));
        }
    }

    #[test]
    fn kernel_counters_check_out() {
        let s = sys_info();
        // The layout check passed, so the resident counters are filled and plausible.
        assert!(s.resident_paged_pool > 0 && s.resident_paged_pool <= s.paged_pool + (64 << 20), "{:?}", s);
        assert!(s.resident_system_cache < s.ram_total);
    }

    const SPI: usize = size_of::<SystemProcessInformation>();
    const MB: u64 = 1024 * 1024;

    /// A SystemProcessInformation buffer laid out like the kernel's: each entry, room for
    /// "threads", then its name, with the name pointers aimed into the buffer.
    fn synthetic(entries: &[(u32, u32, Option<&str>, u64, u64, i64)]) -> Vec<u64> {
        let mut buf = vec![0u64; 1024];
        let base = buf.as_mut_ptr() as *mut u8;
        let mut off = 0usize;
        for (i, &(pid, ppid, name, ws, commit, created)) in entries.iter().enumerate() {
            let name16: Vec<u16> = name.map(|n| n.encode_utf16().collect()).unwrap_or_default();
            let name_off = off + SPI + 64; // 64 bytes of pretend thread entries
            let size = (name_off - off + name16.len() * 2 + 7) & !7;
            let mut e: SystemProcessInformation = unsafe { zeroed() };
            e.next_entry_offset = if i + 1 == entries.len() { 0 } else { size as u32 };
            e.unique_process_id = pid as usize;
            e.inherited_from_unique_process_id = ppid as usize;
            e.working_set_size = ws as usize;
            e.pagefile_usage = commit as usize;
            e.create_time = created;
            if name.is_some() {
                e.image_name_length = (name16.len() * 2) as u16;
                e.image_name_maximum_length = e.image_name_length + 2;
                e.image_name_buffer = base as usize + name_off;
            }
            unsafe {
                std::ptr::write_unaligned(base.add(off) as *mut SystemProcessInformation, e);
                for (j, c) in name16.iter().enumerate() {
                    std::ptr::write_unaligned(base.add(name_off + j * 2) as *mut u16, *c);
                }
            }
            off += size;
        }
        buf
    }

    fn bytes(buf: &[u64]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn spi_layout_matches_the_nt_headers() {
        assert_eq!(SPI, 208);
        assert_eq!(std::mem::offset_of!(SystemProcessInformation, create_time), 32);
        assert_eq!(std::mem::offset_of!(SystemProcessInformation, image_name_buffer), 64);
        assert_eq!(std::mem::offset_of!(SystemProcessInformation, unique_process_id), 80);
        assert_eq!(std::mem::offset_of!(SystemProcessInformation, inherited_from_unique_process_id), 88);
        assert_eq!(std::mem::offset_of!(SystemProcessInformation, working_set_size), 144);
        assert_eq!(std::mem::offset_of!(SystemProcessInformation, pagefile_usage), 184);
    }

    #[test]
    fn parse_spi_walks_entries() {
        let buf = synthetic(&[
            (0, 0, None, 8192, 0, 0),
            (4, 0, Some("System"), 1 << 20, 2 << 20, 5),
            (1234, 4, Some("Chrome.EXE"), 300 << 20, 200 << 20, 1_000_000),
            (5678, 1234, Some("UnrealEditor-Cmd.exe"), 1, 2, 3),
            (91, 1, Some("exe"), 0, 0, 7),
        ]);
        let got = parse_spi(bytes(&buf));
        let short: Vec<(u32, u32, &str, u64, u64, u64)> =
            got.iter().map(|e| (e.pid, e.ppid, e.name.as_str(), e.ram, e.committed, e.created)).collect();
        assert_eq!(
            short,
            [
                (0, 0, "[System Process]", 8192, 0, 0),
                (4, 0, "System", 1 << 20, 2 << 20, 5),
                (1234, 4, "Chrome", 300 << 20, 200 << 20, 1_000_000),
                (5678, 1234, "UnrealEditor-Cmd", 1, 2, 3),
                (91, 1, "exe", 0, 0, 7),
            ]
        );
        // A truncated buffer yields what fits and never reads past the end.
        assert_eq!(parse_spi(&bytes(&buf)[..SPI + 10]).len(), 1);
        assert!(parse_spi(&[]).is_empty());
    }

    #[test]
    fn live_list_matches_the_per_process_calls() {
        let me = current_pid();
        let procs = list_processes();
        let p = procs.iter().find(|p| p.pid == me).expect("our own process is listed");
        assert!(p.queryable && p.ram > 0 && p.committed > 0 && p.created > 0, "{:?}", p);
        // PagefileUsage is the same counter as PrivateUsage (commit charge). Other tests
        // allocate in parallel, so read it on both sides of the list and allow a few tries.
        let h = open(me, PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        let close = (0..10).any(|_| {
            let before = memory_counters(&h).unwrap().1;
            let listed = list_processes().into_iter().find(|p| p.pid == me).unwrap().committed;
            let after = memory_counters(&h).unwrap().1;
            listed + MB >= before.min(after) && listed <= before.max(after) + MB
        });
        assert!(close, "committed {} vs PrivateUsage {}", p.committed, memory_counters(&h).unwrap().1);
        assert!(procs.iter().any(|p| p.pid == 0 && p.name == "[System Process]" && !p.queryable));
        let old = list_processes_toolhelp();
        let o = old.iter().find(|o| o.pid == me).unwrap();
        assert_eq!((o.ppid, &o.name, o.created), (p.ppid, &p.name, p.created));
        // Whatever we could open before, we still can; whatever we couldn't, we still can't.
        for o in old.iter().filter(|o| o.pid != 0) {
            if let Some(n) = procs.iter().find(|n| n.pid == o.pid && n.created == o.created) {
                assert_eq!(n.queryable, o.queryable, "{:?} vs {:?}", n, o);
            }
        }
    }

    #[test]
    fn spawned_child_reports_us_as_parent() {
        let mut child = std::process::Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start ping");
        let found = list_processes().into_iter().find(|p| p.pid == child.id());
        let sid = found.as_ref().and_then(|p| process_session_id(p.pid, p.created));
        let _ = child.kill();
        let _ = child.wait();
        let p = found.expect("ping is listed");
        assert_eq!(p.ppid, current_pid());
        assert_eq!(p.name.to_ascii_lowercase(), "ping");
        assert!(p.queryable && p.created > 0);
        assert!(sid.is_some(), "expected session_id for live ping child");
    }

    #[test]
    fn lznt1_roundtrip_works() {
        assert_eq!(compress_lznt1(&[]).unwrap(), Vec::<u8>::new());
        assert_eq!(decompress_lznt1(&[], 0).unwrap(), Vec::<u8>::new());
        let payload: Vec<u8> = b"AtherMemGuard LZNT1 compression test payload! ".repeat(200);
        let compressed = compress_lznt1(&payload).unwrap();
        assert!(compressed.len() < payload.len(), "{} >= {}", compressed.len(), payload.len());
        assert!(decompress_lznt1(&compressed, 0).is_err());
        let restored = decompress_lznt1(&compressed, payload.len()).unwrap();
        assert_eq!(restored, payload);
    }

    #[test]
    fn normalize_nt_driver_path_handles_utf8_boundaries() {
        assert_eq!(
            normalize_nt_driver_path("D:\\\u{FFFD}\\drv.sys", r"C:\Windows"),
            PathBuf::from("D:\\\u{FFFD}\\drv.sys")
        );
        assert_eq!(
            normalize_nt_driver_path("C:\\Drivers\\\u{FFFD}\\drv.sys", r"C:\Windows"),
            PathBuf::from("C:\\Drivers\\\u{FFFD}\\drv.sys")
        );
        assert_eq!(
            normalize_nt_driver_path("C:\\Driv\\\u{FFFD}\\drv.sys", r"C:\Windows"),
            PathBuf::from("C:\\Driv\\\u{FFFD}\\drv.sys")
        );
        assert_eq!(
            normalize_nt_driver_path(r"\??\C:\Drivers\foo.sys", r"C:\Windows"),
            PathBuf::from(r"C:\Drivers\foo.sys")
        );
        assert_eq!(
            normalize_nt_driver_path(r"\SystemRoot\System32\ntoskrnl.exe", r"C:\Windows\"),
            PathBuf::from(r"C:\Windows\System32\ntoskrnl.exe")
        );
        assert_eq!(
            normalize_nt_driver_path(r"System32\drivers\bar.sys", r"C:\Windows"),
            PathBuf::from(r"C:\Windows\System32\drivers\bar.sys")
        );
    }

    #[test]
    fn working_set_cap_hard_clear_and_soft_floor_verified_with_win32() {
        use windows_sys::Win32::System::Memory::GetProcessWorkingSetSizeEx;

        let mut child = std::process::Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start ping");
        let pid = child.id();

        let query_ws = || {
            let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION).expect("open child for query");
            let mut min_ws = 0usize;
            let mut max_ws = 0usize;
            let mut flags = 0u32;
            let ok = unsafe { GetProcessWorkingSetSizeEx(h.0, &mut min_ws, &mut max_ws, &mut flags) != 0 };
            assert!(ok, "GetProcessWorkingSetSizeEx failed");
            (min_ws, max_ws, flags)
        };

        assert!(set_working_set_cap(pid, 0, 64 * 1024 * 1024, true));
        let (_min1, max1, flags1) = query_ws();
        assert_eq!(max1, 64 * 1024 * 1024);
        assert_ne!(flags1 & QUOTA_LIMITS_HARDWS_MAX_ENABLE, 0, "expected HARDWS_MAX_ENABLE set, flags={:#x}", flags1);

        assert!(set_working_set_cap(pid, 0, 0, true));
        let (_min2, max2, flags2) = query_ws();
        assert_eq!(flags2 & QUOTA_LIMITS_HARDWS_MAX_ENABLE, 0, "expected HARDWS_MAX_ENABLE cleared, flags={:#x}", flags2);
        assert_ne!(max2, 64 * 1024 * 1024);

        assert!(set_working_set_cap(pid, 0, 64 * 1024 * 1024, false), "soft cap with min=0 should succeed");
        let (min3, max3, flags3) = query_ws();
        assert_eq!(max3, 64 * 1024 * 1024);
        assert!(min3 >= 20 * 4096, "expected min >= 20 pages, got {}", min3);
        assert_eq!(flags3 & QUOTA_LIMITS_HARDWS_MAX_ENABLE, 0, "soft cap must not set HARDWS_MAX_ENABLE");

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn io_priority_cpu_rate_ws_cap_and_policy_on_spawned_child() {
        let mut child = std::process::Command::new("ping")
            .args(["-t", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start ping");
        let pid = child.id();

        let io_ok = set_io_priority(pid, true);
        let io_restore_ok = set_io_priority(pid, false);

        let policy_res = apply_process_policy(
            pid,
            true,
            ProcessPolicy {
                memory_priority: Some(2),
                cpu_class_raw: Some(BELOW_NORMAL_PRIORITY_CLASS),
                eco_mode: Some(false),
                io_priority: Some(0),
            },
        );

        let ws_set_ok = set_working_set_cap(pid, 0, 64 * 1024 * 1024, true);
        let ws_clear_ok = set_working_set_cap(pid, 0, 0, true);

        let job = Job::create(None, 0, 0, true).expect("create job");
        let assigned = job.assign(pid);
        let ws_job_ok = job.set_working_set_limits(0, 64 * 1024 * 1024);
        let cpu_ok = job.set_cpu_rate(Some(25));
        let cpu_off_ok = job.set_cpu_rate(None);

        let _ = child.kill();
        let _ = child.wait();

        assert!(io_ok && io_restore_ok);
        assert!(policy_res.opened && policy_res.prev_cpu_class != 0);
        assert!(policy_res.memory_ok && policy_res.cpu_ok && policy_res.eco_ok && policy_res.io_ok);
        assert!(ws_set_ok, "set_working_set_cap failed");
        assert!(ws_clear_ok, "set_working_set_cap clear failed");
        assert!(assigned);
        if is_elevated() {
            assert!(ws_job_ok.is_ok(), "{:?}", ws_job_ok);
        } else if let Err(e) = &ws_job_ok {
            assert!(e.contains("1314") || e.to_ascii_lowercase().contains("privilege"), "unexpected error: {}", e);
        }
        assert!(cpu_ok.is_ok(), "{:?}", cpu_ok);
        assert!(cpu_off_ok.is_ok(), "{:?}", cpu_off_ok);
    }

    #[test]
    fn low_memory_event_and_dir_watch_work() {
        let ev = low_memory_event();
        assert!(ev.is_some());

        let dir = std::env::temp_dir().join(format!("amg-dirwatch-test-{}", current_pid()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let watch = watch_dir_handle(&dir).expect("watch_dir_handle");
        assert!(!watch.raw_handle().is_null());
        std::fs::write(dir.join("probe.toml"), "interval_sec = 10\n").unwrap();
        let r = unsafe { WaitForSingleObject(watch.raw_handle(), 2000) };
        assert_eq!(r, WAIT_OBJECT_0);
        assert!(watch.next());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loaded_driver_paths_and_pool_tags() {
        let paths = loaded_driver_paths();
        assert!(!paths.is_empty());
        assert!(paths.iter().any(|p| {
            let s = p.to_string_lossy().to_ascii_lowercase();
            s.ends_with(".sys") || s.ends_with("ntoskrnl.exe")
        }));
        assert!(paths.len() > 1, "expected multiple loaded modules, got {:?}", paths);

        if let Some(tags) = pool_tag_info() {
            assert!(!tags.is_empty());
        }
    }

    #[test]
    fn standby_purge_and_combine_need_admin() {
        if !is_elevated() {
            assert_eq!(purge_low_priority_standby(), Err("needs administrator rights".to_string()));
            assert_eq!(combine_physical_memory(), Err("needs administrator rights".to_string()));
        }
    }

    #[test]
    fn reg_read_only_checks() {
        let build = reg_get_string(
            RegHive::Hklm,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "CurrentBuild",
        )
        .expect("CurrentBuild should exist");
        assert!(!build.is_empty() && build.chars().all(|c| c.is_ascii_digit()), "{}", build);

        let missing = RegKey::open_read(
            RegHive::Hklm,
            r"SOFTWARE\AtherMemGuard_DoesNotExist_TestKey_99999",
        );
        assert!(matches!(missing, Err(RegError::NotFound)));
    }

    #[test]
    fn wait_any_handles_events_and_timeouts() {
        let ev1 = create_manual_event(None, false).unwrap();
        let ev2 = create_event(&format!(r"Local\AtherMemGuard.WinWaitTest.{}", current_pid())).unwrap();
        assert_eq!(wait_any(&[ev1.wait_handle(), ev2.wait_handle()], 5), WaitOutcome::Timeout);
        assert!(set_event_handle(&ev2));
        assert_eq!(wait_any(&[ev1.wait_handle(), ev2.wait_handle()], 500), WaitOutcome::Signaled(1));
        assert_eq!(wait_any(&[ev1.wait_handle(), ev2.wait_handle()], 5), WaitOutcome::Timeout);
        assert!(set_event_handle(&ev1));
        assert_eq!(wait_any(&[ev1.wait_handle(), ev2.wait_handle()], 500), WaitOutcome::Signaled(0));
        assert!(reset_event_handle(&ev1));
        assert_eq!(wait_any(&[ev1.wait_handle()], 5), WaitOutcome::Timeout);
    }

    #[test]
    fn overlapped_pipe_server_roundtrip_back_to_back_and_clean_shutdown() {
        let pipe_name = format!(r"\\.\pipe\AtherMemGuard.WinPipeTest.{}.{}", current_pid(), now_filetime());
        assert_eq!(pipe_call(&pipe_name, b"ping\n", 100), Err(PipeError::NotFound));

        let (mut server, shutdown) = PipeServer::bind(&pipe_name).expect("bind PipeServer");
        let t = std::thread::spawn(move || {
            while let Ok(Some(conn)) = server.accept() {
                let line = conn.read_line_timeout(1024, 2000).expect("read request");
                let reply = format!("echo:{}\n", line);
                conn.write_all_flush(reply.as_bytes(), 2000).expect("write reply");
            }
        });

        for i in 0..5 {
            let req = format!("cmd-{}\n", i);
            let resp = pipe_call(&pipe_name, req.as_bytes(), 2000).expect("pipe_call");
            assert_eq!(String::from_utf8(resp).unwrap(), format!("echo:cmd-{}", i));
        }

        // Server is currently blocked in ConnectNamedPipe; shutdown must wake it immediately.
        shutdown.signal();
        t.join().expect("server thread joined cleanly");
    }

    #[test]
    fn shared_mapping_seqlock_roundtrip_and_missing() {
        let name = format!(r"Local\AtherMemGuard.WinShmTest.{}.{}", current_pid(), now_filetime());
        assert_eq!(read_shared_mapping(&name), None);

        let mut writer = SharedMappingWriter::create(&name).expect("create shared mapping");
        assert_eq!(read_shared_mapping(&name), None); // seq == 0 before first write

        let p1 = br#"{"paused":false,"ue":{"workers_paused":0}}"#;
        assert!(writer.write(p1));
        assert_eq!(read_shared_mapping(&name).as_deref(), Some(&p1[..]));

        let p2 = br#"{"paused":true,"ue":{"workers_paused":3}}"#;
        assert!(writer.write(p2));
        assert_eq!(read_shared_mapping(&name).as_deref(), Some(&p2[..]));
    }

    #[test]
    fn t5_lznt1_corrupt_payload_and_short_output_contract() {
        // Corrupt compressed stream must fail rather than panic or OOB-write.
        assert!(decompress_lznt1(&[0xFF; 16], 1024).is_err());

        // Upper-bound uncompressed_len contract: decompress_lznt1 truncates to final_size (256),
        // so callers validating exact uncompressed_len (like decode_embedded_cli) must check len().
        let sample = vec![0x41u8; 256];
        let comp = compress_lznt1(&sample).expect("compress");
        let out = decompress_lznt1(&comp, 8192).expect("decompress");
        assert_eq!(out.len(), 256);
        assert_eq!(out, sample);
    }

    #[test]
    fn t5_pipe_server_survives_malformed_truncated_and_oversized_clients() {
        use std::io::Write;
        let pipe_name = format!(r"\\.\pipe\amg-t5-pipe-abuse-{}-{}", current_pid(), now_filetime());
        let (mut srv, shutdown) = PipeServer::bind(&pipe_name).expect("bind test pipe");

        let srv_thread = std::thread::spawn(move || {
            let mut handled = 0usize;
            let mut ping_ok = 0usize;
            while let Ok(Some(conn)) = srv.accept() {
                handled += 1;
                if let Ok(line) = conn.read_line_timeout(4096, 1000) {
                    if line.contains("\"ping\"") && conn.write_all_flush(b"{\"ok\":true}\n", 1000).is_ok() {
                        ping_ok += 1;
                    } else {
                        let _ = conn.write_all_flush(b"{\"ok\":false}\n", 1000);
                    }
                }
            }
            (handled, ping_ok)
        });

        let call_retry = |req: &[u8]| -> Result<Vec<u8>, PipeError> {
            let t0 = Instant::now();
            loop {
                match pipe_call(&pipe_name, req, 2000) {
                    Err(PipeError::NotFound) if t0.elapsed() < Duration::from_secs(2) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    other => return other,
                }
            }
        };

        // 1. Client writes invalid UTF-8.
        let _ = call_retry(&[0xFF, 0xFE, b'\n']);
        // 2. Client writes > 4096 bytes.
        let big_req = format!("{{\"cmd\":\"{}\"}}\n", "x".repeat(5000));
        let _ = call_retry(big_req.as_bytes());
        // 3. Client writes truncated bytes without '\n' and disconnects immediately.
        let t_open = Instant::now();
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&pipe_name) {
                Ok(mut raw) => {
                    let _ = raw.write_all(b"{\"partial\":");
                    let _ = raw.flush();
                    break;
                }
                Err(_) if t_open.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("failed to open pipe for truncated write: {}", e),
            }
        }
        // 4. Subsequent valid request must still succeed cleanly once server re-arms accept().
        let ok_res = call_retry(b"{\"cmd\":\"ping\"}\n").expect("valid call after abuse");
        assert!(ok_res.starts_with(b"{\"ok\":true}"), "unexpected reply: {:?}", ok_res);

        shutdown.signal();
        let (handled, ping_ok) = srv_thread.join().unwrap();
        assert_eq!(ping_ok, 1);
        assert!((3..=4).contains(&handled), "expected 3 or 4 accepted connections, got {}", handled);
    }
}

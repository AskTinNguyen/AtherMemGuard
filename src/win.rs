//! Safe wrappers over the Win32 / NT calls AtherMemGuard uses. All `unsafe` lives here and in `tray`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, SYSTEMTIME, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx;
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS,
    PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimeAsFileTime};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcessId, GetPriorityClass, GetProcessTimes, OpenEventW,
    OpenMutexW, OpenProcess, ProcessMemoryPriority, ProcessPowerThrottling, SetEvent, SetPriorityClass,
    SetProcessInformation, TerminateProcess, WaitForSingleObject, BELOW_NORMAL_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, MEMORY_PRIORITY_INFORMATION, NORMAL_PRIORITY_CLASS,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE,
};

const SYNCHRONIZE: u32 = 0x0010_0000;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const POWER_THROTTLING_VERSION: u32 = 1;
const POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: HANDLE) -> i32;
    fn NtResumeProcess(process: HANDLE) -> i32;
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
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
}

impl SysInfo {
    pub fn commit_free(&self) -> u64 {
        self.commit_limit.saturating_sub(self.commit_used)
    }
}

pub fn sys_info() -> SysInfo {
    let mut pi: PERFORMANCE_INFORMATION = unsafe { zeroed() };
    pi.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
    if unsafe { K32GetPerformanceInfo(&mut pi, pi.cb) } == 0 {
        return SysInfo::default();
    }
    let page = pi.PageSize as u64;
    SysInfo {
        ram_total: pi.PhysicalTotal as u64 * page,
        ram_avail: pi.PhysicalAvailable as u64 * page,
        commit_used: pi.CommitTotal as u64 * page,
        commit_limit: pi.CommitLimit as u64 * page,
        file_cache: pi.SystemCache as u64 * page,
        nonpaged_pool: pi.KernelNonpaged as u64 * page,
    }
}

// ---------------------------------------------------------------- processes

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Executable name without `.exe`.
    pub name: String,
    /// Physical RAM in use (working set), bytes.
    pub ram: u64,
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

/// One entry of a SystemProcessInformation buffer, before the "can we open it" check.
#[derive(Debug, Clone, PartialEq)]
struct SpiEntry {
    pid: u32,
    ppid: u32,
    name: String,
    ram: u64,
    committed: u64,
    created: u64,
}

/// Walk a SystemProcessInformation buffer. `buf` is the buffer as the kernel filled it: the
/// image name pointers point into it. Entries or names that fall outside it are skipped.
fn parse_spi(buf: &[u8]) -> Vec<SpiEntry> {
    let mut out = Vec::with_capacity(512);
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
                    let units: Vec<u16> =
                        buf[start..start + len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    String::from_utf16_lossy(&units)
                }
                None => String::new(),
            }
        };
        if name.len() >= 4 && name.get(name.len() - 4..).is_some_and(|t| t.eq_ignore_ascii_case(".exe")) {
            name.truncate(name.len() - 4);
        }
        out.push(SpiEntry {
            pid,
            ppid: e.inherited_from_unique_process_id as u32,
            name,
            ram: e.working_set_size as u64,
            committed: e.pagefile_usage as u64,
            created: e.create_time.max(0) as u64,
        });
        if e.next_entry_offset == 0 {
            break;
        }
        off += e.next_entry_offset as usize;
    }
    out
}

/// The query buffer, reused between calls (8-byte aligned), and which processes we could
/// open, by (pid, creation time). Whether a process can be opened doesn't change during its
/// life, so each one is tried once.
struct ListState {
    buf: Vec<u64>,
    queryable: HashMap<(u32, u64), bool>,
}

static LIST_STATE: Mutex<Option<ListState>> = Mutex::new(None);

/// Every process on the machine with its memory use, from one
/// NtQuerySystemInformation(SystemProcessInformation) call.
pub fn list_processes() -> Vec<Proc> {
    let mut guard = LIST_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(|| ListState { buf: vec![0u64; 128 * 1024], queryable: HashMap::new() }); // 1 MB
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
        return Vec::new();
    }
    let len = (ret as usize).min(state.buf.len() * 8);
    // SAFETY: a u64 buffer viewed as bytes, within its length.
    let bytes = unsafe { std::slice::from_raw_parts(state.buf.as_ptr() as *const u8, len) };
    let entries = parse_spi(bytes);

    let mut seen = HashMap::with_capacity(entries.len());
    let out = entries
        .into_iter()
        .map(|e| {
            let key = (e.pid, e.created);
            let queryable = match state.queryable.get(&key) {
                Some(&q) => q,
                None => e.pid != 0 && open(e.pid, PROCESS_QUERY_LIMITED_INFORMATION).is_some(),
            };
            seen.insert(key, queryable);
            Proc { pid: e.pid, ppid: e.ppid, name: e.name, ram: e.ram, committed: e.committed, created: e.created, queryable }
        })
        .collect();
    state.queryable = seen;
    out
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
    // u64 elements keep the UNICODE_STRING header aligned.
    let mut buf = vec![0u64; 8400];
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
    let slice = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
    Some(String::from_utf16_lossy(slice))
}

// ---------------------------------------------------------------- soft controls

/// 1 = very low … 5 = normal. Lower-priority pages are evicted from RAM first.
pub fn set_memory_priority(pid: u32, priority: u32) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    let info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: priority };
    unsafe {
        SetProcessInformation(
            h.0,
            ProcessMemoryPriority,
            &info as *const _ as *const c_void,
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        ) != 0
    }
}

/// Push a process's memory out of RAM (to the compressed store / page file).
pub fn trim(pid: u32) -> bool {
    let Some(h) = open(pid, PROCESS_SET_QUOTA) else { return false };
    unsafe { SetProcessWorkingSetSizeEx(h.0, usize::MAX, usize::MAX, 0) != 0 }
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
pub fn set_eco_mode(pid: u32, on: bool) -> bool {
    let Some(h) = open(pid, PROCESS_SET_INFORMATION) else { return false };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: POWER_THROTTLING_VERSION,
        ControlMask: POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: if on { POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
    };
    unsafe {
        SetProcessInformation(
            h.0,
            ProcessPowerThrottling,
            &state as *const _ as *const c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) != 0
    }
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

// ---------------------------------------------------------------- hard caps (Job Objects)

pub struct Job(Handle);

impl Job {
    /// Create a job with commit limits. 0 means "no limit" for either value.
    pub fn create(name: Option<&str>, per_process_bytes: u64, job_bytes: u64, kill_on_close: bool) -> Result<Job, String> {
        let wname = name.map(wide);
        let h = unsafe { CreateJobObjectW(null(), wname.as_ref().map_or(null(), |w| w.as_ptr())) };
        if h.is_null() {
            return Err(format!("CreateJobObject failed: {}", std::io::Error::last_os_error()));
        }
        let job = Job(Handle(h));
        job.set_limits(per_process_bytes, job_bytes, kill_on_close)?;
        Ok(job)
    }

    pub fn set_limits(&self, per_process_bytes: u64, job_bytes: u64, kill_on_close: bool) -> Result<(), String> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        if per_process_bytes > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = per_process_bytes as usize;
        }
        if job_bytes > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = job_bytes as usize;
        }
        if kill_on_close {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        let ok = unsafe {
            SetInformationJobObject(
                self.0 .0,
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

    /// Add a running process. True if it is in the job afterwards.
    pub fn assign(&self, pid: u32) -> bool {
        let Some(h) = open(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION) else {
            return false;
        };
        self.assign_handle(h.0)
    }

    pub fn assign_handle(&self, process: HANDLE) -> bool {
        let mut in_job = 0;
        if unsafe { IsProcessInJob(process, self.0 .0, &mut in_job) } != 0 && in_job != 0 {
            return true;
        }
        unsafe { AssignProcessToJobObject(self.0 .0, process) != 0 }
    }
}

// ---------------------------------------------------------------- single instance + stop signal

/// Take the "AtherMemGuard is running" mutex. Waits up to `wait_ms` for a previous copy to finish.
pub fn acquire_instance(name: &str, wait_ms: u32) -> Option<Handle> {
    let w = wide(name);
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

pub fn instance_running(name: &str) -> bool {
    let w = wide(name);
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

pub fn create_event(name: &str) -> Option<Handle> {
    let w = wide(name);
    let h = unsafe { CreateEventW(null(), 0, 0, w.as_ptr()) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

/// True if the event is signalled (and resets it, since it is auto-reset).
pub fn event_is_set(h: &Handle) -> bool {
    unsafe { WaitForSingleObject(h.0, 0) == WAIT_OBJECT_0 }
}

/// Signal a named event if it exists. Returns false if nothing is listening.
pub fn signal_event(name: &str) -> bool {
    let w = wide(name);
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) };
    if h.is_null() {
        return false;
    }
    let h = Handle(h);
    unsafe { SetEvent(h.0) != 0 }
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

#[cfg(test)]
mod tests {
    use super::*;

    const SPI: usize = size_of::<SystemProcessInformation>();

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
        let h = open(me, PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        let (_, private) = memory_counters(&h).unwrap();
        // PagefileUsage is the same counter as PrivateUsage (commit charge).
        assert!(p.committed.abs_diff(private) < 1024 * 1024, "{} vs {}", p.committed, private);
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
        let _ = child.kill();
        let _ = child.wait();
        let p = found.expect("ping is listed");
        assert_eq!(p.ppid, current_pid());
        assert_eq!(p.name.to_ascii_lowercase(), "ping");
        assert!(p.queryable && p.created > 0);
    }
}

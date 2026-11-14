//! Safe wrappers over the Win32 / NT calls AtherMemGuard uses. All `unsafe` lives here and in `tray`.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

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
}

/// Every process on the machine with its memory use. Processes we can't open report zeros.
pub fn list_processes() -> Vec<Proc> {
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
        let mut p = Proc { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name, ram: 0, committed: 0, created: 0 };
        if p.pid != 0 {
            if let Some(h) = open(p.pid, PROCESS_QUERY_LIMITED_INFORMATION) {
                let mut mc: PROCESS_MEMORY_COUNTERS_EX = unsafe { zeroed() };
                mc.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
                let got = unsafe {
                    K32GetProcessMemoryInfo(h.0, &mut mc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mc.cb)
                };
                if got != 0 {
                    p.ram = mc.WorkingSetSize as u64;
                    p.committed = mc.PrivateUsage as u64;
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

pub fn null_handle() -> HANDLE {
    null_mut()
}

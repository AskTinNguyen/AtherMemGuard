//! The Win32 calls the doctor needs that `win.rs` doesn't have: who owns a process and in
//! which session it runs, top-level windows and `WM_CLOSE`, the exe path and its version
//! info, and waiting for a process to exit. Each `unsafe` block is small and only passes
//! buffers it owns to the API it documents.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, WAIT_OBJECT_0};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Security::{GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, GWL_EXSTYLE, GW_OWNER, WM_CLOSE, WS_EX_TOOLWINDOW,
};

use crate::win::{wide, Handle};

const SYNCHRONIZE: u32 = 0x0010_0000;

fn open(pid: u32, access: u32) -> Option<Handle> {
    // SAFETY: plain call; a null result means "could not open".
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        None
    } else {
        Some(Handle(h))
    }
}

// ---------------------------------------------------------------- owner and session

/// The terminal-services session a process runs in.
pub fn session_of(pid: u32) -> Option<u32> {
    let mut s = 0u32;
    // SAFETY: `s` is a valid out pointer for the call's duration.
    (unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
}

pub fn current_session() -> Option<u32> {
    session_of(crate::win::current_pid())
}

/// The user SID (as bytes) a process token belongs to.
fn token_user_sid(token: HANDLE) -> Option<Vec<u8>> {
    let mut len = 0u32;
    // SAFETY: first call asks for the size only (null buffer, zero length).
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
    if len == 0 || len > 4096 {
        return None;
    }
    // u64 elements keep TOKEN_USER aligned.
    let mut buf = vec![0u64; (len as usize + 7) / 8];
    // SAFETY: `buf` holds at least `len` bytes and outlives the call.
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, len, &mut len) } == 0 {
        return None;
    }
    // SAFETY: the call filled `buf` with a TOKEN_USER whose SID pointer points inside `buf`.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    if sid.is_null() {
        return None;
    }
    // SAFETY: `sid` is a valid SID inside `buf`; GetLengthSid reads its header.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    let start = sid as usize - buf.as_ptr() as usize;
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
    bytes.get(start..start + sid_len).map(|s| s.to_vec())
}

pub fn process_user_sid(pid: u32) -> Option<Vec<u8>> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut token: HANDLE = null_mut();
    // SAFETY: `h` is an open process handle; `token` is a valid out pointer.
    if unsafe { OpenProcessToken(h.0, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = Handle(token);
    token_user_sid(token.0)
}

pub fn current_user_sid() -> Option<Vec<u8>> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo handle needs no closing; `token` is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = Handle(token);
    token_user_sid(token.0)
}

/// What the doctor needs to know about who a process belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ownership {
    /// Runs as the same user as MemGuard.
    pub owned: bool,
    /// Runs in the same session as MemGuard.
    pub same_session: bool,
}

/// Check one process against this user and session. Unreadable processes are not ours.
pub fn ownership(pid: u32, me: Option<&[u8]>, my_session: Option<u32>) -> Ownership {
    let owned = match (me, process_user_sid(pid)) {
        (Some(me), Some(sid)) => me == sid.as_slice(),
        _ => false,
    };
    let same_session = matches!((my_session, session_of(pid)), (Some(a), Some(b)) if a == b);
    Ownership { owned, same_session }
}

// ---------------------------------------------------------------- windows

#[derive(Debug, Clone)]
pub struct TopWindow {
    pub hwnd: isize,
    pub pid: u32,
    /// Visible on screen as an app window: shown, not cloaked, not a tool window, no owner.
    pub visible: bool,
    /// Has a title and isn't a known helper window (IME, OLE, ...): a candidate main window
    /// for apps that only sit in the tray.
    pub main_candidate: bool,
}

/// Helper window classes that must never get WM_CLOSE: closing them breaks the app's
/// input method or COM plumbing instead of asking it to quit.
const HELPER_CLASSES: &[&str] = &[
    "IME",
    "MSCTFIME UI",
    "OleMainThreadWndClass",
    "GDI+ Hook Window Class",
    "CicMarshalWndClass",
    "tooltips_class32",
    "Base_PowerMessageWindow",
    "Chrome_SystemMessageWindow",
    "Shell_TrayWnd",
    "WorkerW",
    "Progman",
    "UserAdapterWindowClass",
    "DDEMLMom",
    "DDEMLEvent",
    "SystemUserAdapter_WindowClass",
    "MediaContextNotificationWindow",
];

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: `buf` is valid for its length.
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<TopWindow>` passed by `top_level_windows`, alive
    // for the whole EnumWindows call.
    let out = &mut *(lparam as *mut Vec<TopWindow>);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    let shown = IsWindowVisible(hwnd) != 0;
    let mut cloaked = 0u32;
    let cloak_ok =
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut c_void, size_of::<u32>() as u32);
    let cloaked = cloak_ok == 0 && cloaked != 0;
    let tool = (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW != 0;
    let owned = !GetWindow(hwnd, GW_OWNER).is_null();
    let titled = GetWindowTextLengthW(hwnd) > 0;
    let class = class_name(hwnd);
    let helper = HELPER_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(&class));
    out.push(TopWindow {
        hwnd: hwnd as isize,
        pid,
        visible: shown && !cloaked && !tool && !owned && !helper,
        main_candidate: titled && !owned && !helper,
    });
    1
}

/// Every top-level window on this desktop.
pub fn top_level_windows() -> Vec<TopWindow> {
    let mut out: Vec<TopWindow> = Vec::with_capacity(256);
    // SAFETY: the callback only runs during this call and receives a pointer to `out`.
    unsafe { EnumWindows(Some(collect_window), &mut out as *mut Vec<TopWindow> as LPARAM) };
    out
}

/// Ask a window to close (WM_CLOSE). Posted, so a window that shows a "save?" prompt
/// doesn't block us. False if the window is gone.
pub fn post_close(hwnd: isize) -> bool {
    // SAFETY: posting to a stale handle just fails.
    unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) != 0 }
}

// ---------------------------------------------------------------- exe path, version info

pub fn image_path(pid: u32) -> Option<String> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units.
    if unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// `FileDescription` and `CompanyName` from an exe's version resource.
pub fn version_strings(path: &str) -> Option<(String, String)> {
    let wpath = wide(path);
    let mut handle = 0u32;
    // SAFETY: `wpath` is NUL-terminated.
    let size = unsafe { GetFileVersionInfoSizeW(wpath.as_ptr(), &mut handle) };
    if size == 0 || size > 4 * 1024 * 1024 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: `data` holds `size` bytes.
    if unsafe { GetFileVersionInfoW(wpath.as_ptr(), 0, size, data.as_mut_ptr() as *mut c_void) } == 0 {
        return None;
    }
    let query = |sub: &str, unit_bytes: usize| -> Option<&[u8]> {
        let w = wide(sub);
        let mut ptr: *mut c_void = null_mut();
        let mut len = 0u32;
        // SAFETY: VerQueryValueW returns a pointer into `data`, valid while `data` lives.
        if unsafe { VerQueryValueW(data.as_ptr() as *const c_void, w.as_ptr(), &mut ptr, &mut len) } == 0
            || ptr.is_null()
            || len == 0
        {
            return None;
        }
        let start = (ptr as usize).checked_sub(data.as_ptr() as usize)?;
        let byte_len = (len as usize).checked_mul(unit_bytes)?;
        let end = start.checked_add(byte_len)?.min(data.len());
        data.get(start..end)
    };
    // Translations: pairs of (language, codepage); fall back to US English / Unicode.
    // VerQueryValueW returns byte length for `\VarFileInfo\Translation`.
    let mut langs = Vec::new();
    if let Some(t) = query(r"\VarFileInfo\Translation", 1) {
        for c in t.chunks_exact(4).take(4) {
            langs.push(format!("{:04x}{:04x}", u16::from_le_bytes([c[0], c[1]]), u16::from_le_bytes([c[2], c[3]])));
        }
    }
    langs.extend(["040904b0".to_string(), "040904e4".to_string()]);
    let read = |field: &str| -> String {
        for lang in &langs {
            // VerQueryValueW returns UTF-16 WCHAR length (including NUL) for `\StringFileInfo\*`.
            if let Some(bytes) = query(&format!(r"\StringFileInfo\{}\{}", lang, field), 2) {
                let units: Vec<u16> =
                    bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
                let s = String::from_utf16_lossy(&units).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
        String::new()
    };
    let (desc, company) = (read("FileDescription"), read("CompanyName"));
    if desc.is_empty() && company.is_empty() {
        None
    } else {
        Some((desc, company))
    }
}

// ---------------------------------------------------------------- waiting for exit

/// A handle we can wait on to see a process exit. Opened before asking it to close, so a
/// recycled PID can't fool the wait.
pub fn open_for_wait(pid: u32) -> Option<Handle> {
    open(pid, SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION)
}

pub fn has_exited(h: &Handle) -> bool {
    // SAFETY: `h` is an open process handle with SYNCHRONIZE access.
    unsafe { WaitForSingleObject(h.0, 0) == WAIT_OBJECT_0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_strings_on_ping_exe_are_untruncated() {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
        let ping = format!(r"{}\System32\PING.EXE", windir.trim_end_matches('\\'));
        let (desc, company) = version_strings(&ping).expect("PING.EXE has version strings");
        assert!(!desc.is_empty(), "expected non-empty FileDescription");
        assert_eq!(company, "Microsoft Corporation");
        assert!(!desc.contains("Microsoft Corporation"), "desc should not leak adjacent version fields: {:?}", desc);
    }
}

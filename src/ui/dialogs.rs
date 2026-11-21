//! `IFileOpenDialog` / `IFileSaveDialog` through hand-written COM vtables (windows-sys has
//! the GUIDs but no interface methods). Only the methods we call are typed.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::null_mut;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};

use crate::win::wide;

const CLSID_FILE_OPEN: GUID = GUID { data1: 0xDC1C_5A9C, data2: 0xE88A, data3: 0x4DDE, data4: [0xA5, 0xA1, 0x60, 0xF8, 0x2A, 0x20, 0xAE, 0xF7] };
const CLSID_FILE_SAVE: GUID = GUID { data1: 0xC0B4_E2F3, data2: 0xBA21, data3: 0x4773, data4: [0x8D, 0xBA, 0x33, 0x5E, 0xC9, 0x46, 0xEB, 0x8B] };
const IID_FILE_OPEN: GUID = GUID { data1: 0xD57C_7288, data2: 0xD4AD, data3: 0x4768, data4: [0xBE, 0x02, 0x9D, 0x96, 0x95, 0x32, 0xD9, 0x60] };
const IID_FILE_SAVE: GUID = GUID { data1: 0x84BC_CD23, data2: 0x5FDE, data3: 0x4CDB, data4: [0xAE, 0xA4, 0xAF, 0x64, 0xB8, 0x3D, 0x78, 0xAB] };

const FOS_OVERWRITEPROMPT: u32 = 0x2;
const FOS_FORCEFILESYSTEM: u32 = 0x40;
const FOS_FILEMUSTEXIST: u32 = 0x1000;
const SIGDN_FILESYSPATH: i32 = 0x8005_8000u32 as i32;

#[repr(C)]
struct FilterSpec {
    name: *const u16,
    spec: *const u16,
}

type Unused = usize;

/// IFileDialog's vtable up to SetDefaultExtension (IUnknown, IModalWindow, IFileDialog).
#[repr(C)]
struct FileDialogVtbl {
    query_interface: Unused,
    add_ref: Unused,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    show: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
    set_file_types: unsafe extern "system" fn(*mut c_void, u32, *const FilterSpec) -> i32,
    set_file_type_index: Unused,
    get_file_type_index: Unused,
    advise: Unused,
    unadvise: Unused,
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    set_default_folder: Unused,
    set_folder: Unused,
    get_folder: Unused,
    get_current_selection: Unused,
    set_file_name: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    get_file_name: Unused,
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    set_ok_button_label: Unused,
    set_file_name_label: Unused,
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    add_place: Unused,
    set_default_extension: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
}

#[repr(C)]
struct ShellItemVtbl {
    query_interface: Unused,
    add_ref: Unused,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    bind_to_handler: Unused,
    get_parent: Unused,
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> i32,
}

struct ComGuard(bool);

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Show a file dialog; `filters` are (label, pattern) pairs such as ("TOML files", "*.toml").
fn run(owner: HWND, save: bool, title: &str, file_name: &str, filters: &[(&str, &str)]) -> Option<PathBuf> {
    unsafe {
        let hr = CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED as u32);
        let _com = ComGuard(hr >= 0);
        let mut obj: *mut c_void = null_mut();
        let (clsid, iid) = if save { (&CLSID_FILE_SAVE, &IID_FILE_SAVE) } else { (&CLSID_FILE_OPEN, &IID_FILE_OPEN) };
        if CoCreateInstance(clsid, null_mut(), CLSCTX_INPROC_SERVER, iid, &mut obj) < 0 || obj.is_null() {
            return None;
        }
        let vt = &**(obj as *mut *const FileDialogVtbl);
        let names: Vec<(Vec<u16>, Vec<u16>)> = filters.iter().map(|(n, s)| (wide(n), wide(s))).collect();
        let specs: Vec<FilterSpec> = names.iter().map(|(n, s)| FilterSpec { name: n.as_ptr(), spec: s.as_ptr() }).collect();
        if !specs.is_empty() {
            (vt.set_file_types)(obj, specs.len() as u32, specs.as_ptr());
        }
        let mut opts = 0u32;
        (vt.get_options)(obj, &mut opts);
        opts |= FOS_FORCEFILESYSTEM | if save { FOS_OVERWRITEPROMPT } else { FOS_FILEMUSTEXIST };
        (vt.set_options)(obj, opts);
        let t = wide(title);
        (vt.set_title)(obj, t.as_ptr());
        if save {
            let ext = wide("toml");
            (vt.set_default_extension)(obj, ext.as_ptr());
            if !file_name.is_empty() {
                let n = wide(file_name);
                (vt.set_file_name)(obj, n.as_ptr());
            }
        }
        let mut result = None;
        if (vt.show)(obj, owner) >= 0 {
            let mut item: *mut c_void = null_mut();
            if (vt.get_result)(obj, &mut item) >= 0 && !item.is_null() {
                let ivt = &**(item as *mut *const ShellItemVtbl);
                let mut p: *mut u16 = null_mut();
                if (ivt.get_display_name)(item, SIGDN_FILESYSPATH, &mut p) >= 0 && !p.is_null() {
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    result = Some(PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(p, n))));
                    CoTaskMemFree(p as *const c_void);
                }
                (ivt.release)(item);
            }
        }
        (vt.release)(obj);
        result
    }
}

pub fn open_file(owner: HWND, title: &str, filters: &[(&str, &str)]) -> Option<PathBuf> {
    run(owner, false, title, "", filters)
}

pub fn save_file(owner: HWND, title: &str, file_name: &str, filters: &[(&str, &str)]) -> Option<PathBuf> {
    run(owner, true, title, file_name, filters)
}

//! "Windows and drivers": which kernel pool tags, drivers and Windows processes hold the RAM
//! the flyout's "Windows and drivers" row counts (see `engine::ram_breakdown`).
//!
//! Per-driver detail comes from `NtQuerySystemInformation(SystemPoolTagInformation)` via `win`:
//! kernel memory by 4-letter pool tag. A tag is tied to loaded driver `.sys` files (queried via
//! `NtQuerySystemInformation(SystemModuleInformation)`) that contain the tag's 4 bytes.
//! That is a hint, not proof.

use crate::report_page::{esc, esc_rich, page};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::win;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const TOP_PROCESSES: usize = 8;
const TOP_TAGS: usize = 15;
const DRIVERS_PER_TAG: usize = 3;
/// Memory Windows can't page out above this is unusual.
const NONPAGED_HIGH: u64 = 4 << 30;
/// One tag holding more than this stands out.
const TAG_HIGH: u64 = 1 << 30;

// ---------------------------------------------------------------- report types

#[derive(Debug, Clone, Serialize)]
pub struct DriverReport {
    /// Memory Windows can't page out (nonpaged pool), GB.
    pub nonpaged_pool_gb: f64,
    /// Kernel memory that can be paged out and is in RAM now (resident paged pool), GB.
    pub paged_pool_gb: f64,
    /// Windows' own kernel code in RAM, GB.
    pub system_code_gb: f64,
    /// Driver code in RAM, GB.
    pub drivers_code_gb: f64,
    /// RAM only Windows processes use (System included), GB.
    pub windows_processes_gb: f64,
    /// The sum: the flyout's "Windows and drivers" row, GB.
    pub total_gb: f64,
    /// Largest Windows processes, by name.
    pub processes: Vec<WindowsProcess>,
    /// False when the pool tag query failed (then `tags` is empty).
    pub tags_available: bool,
    /// Largest pool tags by nonpaged + paged bytes.
    pub tags: Vec<PoolTag>,
    /// Plain-English verdict lines.
    pub verdict: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowsProcess {
    pub name: String,
    pub processes: usize,
    pub ram_gb: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolTag {
    /// The 4 tag characters, trailing spaces kept (e.g. "Irp ").
    pub tag: String,
    pub nonpaged_gb: f64,
    pub paged_gb: f64,
    /// What the tag is, for well-known tags.
    pub description: Option<String>,
    /// Owned by the Windows kernel itself (no driver file scan).
    pub windows_core: bool,
    /// Driver files that contain the tag (likely owners).
    pub drivers: Vec<DriverFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriverFile {
    pub file: String,
    /// FileDescription from the version resource, if any.
    pub name: Option<String>,
}

impl DriverFile {
    fn label(&self) -> String {
        match &self.name {
            Some(n) => format!("{} ({})", n, self.file),
            None => self.file.clone(),
        }
    }
}

fn gb1(bytes: u64) -> f64 {
    (bytes as f64 / GB * 10.0).round() / 10.0
}

fn gb2(bytes: u64) -> f64 {
    (bytes as f64 / GB * 100.0).round() / 100.0
}

// ---------------------------------------------------------------- pool tags

/// One SYSTEM_POOLTAG entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawTag {
    tag: [u8; 4],
    paged_used: u64,
    nonpaged_used: u64,
}

impl RawTag {
    fn total(&self) -> u64 {
        self.paged_used.saturating_add(self.nonpaged_used)
    }
}

/// x64 SYSTEM_POOLTAG: Tag[4], PagedAllocs, PagedFrees, pad, PagedUsed (8), NonPagedAllocs,
/// NonPagedFrees, NonPagedUsed (8).
#[cfg(test)]
const ENTRY_SIZE: usize = 40;
#[cfg(test)]
const ENTRIES_OFFSET: usize = 8;

/// Parse a SYSTEM_POOLTAG_INFORMATION buffer. Entries past the end of `buf` are dropped.
#[cfg(test)]
fn parse_pool_tags(buf: &[u8]) -> Vec<RawTag> {
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
        out.push(RawTag { tag, paged_used, nonpaged_used });
    }
    out
}

/// The live pool tag list, or None when Windows refuses (e.g. not allowed).
fn query_pool_tags() -> Option<Vec<RawTag>> {
    win::pool_tag_info().map(|tags| {
        tags.into_iter()
            .map(|t| RawTag { tag: t.tag, paged_used: t.paged_used, nonpaged_used: t.nonpaged_used })
            .collect()
    })
}

/// Tag bytes as text: printable ASCII kept (spaces too), anything else as '.'.
fn tag_text(tag: &[u8; 4]) -> String {
    tag.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect()
}

/// Tag text with trailing spaces made visible, for the text and HTML views ("Irp " -> "Irp·").
fn tag_shown(tag: &str) -> String {
    let trimmed = tag.trim_end_matches(' ');
    format!("{}{}", trimmed, "\u{b7}".repeat(tag.len() - trimmed.len()))
}

/// (tag, windows_core, description). Exact matches first, then `PREFIXES`.
const KNOWN_TAGS: &[(&str, bool, &str)] = &[
    ("MmSt", true, "Windows memory manager: bookkeeping for mapped files (grows with open and cached files)"),
    ("MmCa", true, "Windows memory manager: control areas for mapped files"),
    ("MmRe", true, "Windows memory manager: address layout randomization (ASLR) data"),
    ("MmSm", true, "Windows memory manager: segments of mapped files"),
    ("Mdl ", true, "I/O buffers in flight (memory descriptor lists)"),
    ("Cont", true, "Contiguous memory that device drivers asked for"),
    ("File", true, "Open file objects"),
    ("Thre", true, "Thread objects"),
    ("Proc", true, "Process objects"),
    ("Toke", true, "Security tokens"),
    ("Even", true, "Event objects"),
    ("Sect", true, "Section objects (shared and mapped memory)"),
    ("Key ", true, "Open registry keys"),
    ("Irp ", true, "I/O requests in flight (IRPs)"),
    ("Vad ", true, "Windows memory manager: process address space records"),
    ("VadS", true, "Windows memory manager: process address space records"),
    ("ObNm", true, "Windows object manager: object names"),
    ("IoNm", true, "Windows I/O manager: file name buffers"),
    ("ALPC", true, "Windows inter-process messages (ALPC)"),
    ("EtwB", true, "Event tracing (ETW) buffers: grows with running traces and loggers"),
    ("EtwR", true, "Event tracing (ETW) buffers"),
    ("CM31", true, "Registry (configuration manager)"),
    ("CM25", true, "Registry (configuration manager)"),
    ("smNp", true, "Memory compression store (store manager)"),
    ("SmMs", true, "Memory compression store (store manager)"),
    ("CcSc", true, "Windows file cache bookkeeping (cache manager)"),
    ("Ntfx", false, "NTFS file system (ntfs.sys)"),
    ("NtFs", false, "NTFS file system (ntfs.sys)"),
    ("Ntff", false, "NTFS file system: open file records (ntfs.sys)"),
    ("FMfn", false, "Filter Manager: file names (fltmgr.sys; grows with antivirus and other file filters)"),
    ("FMfc", false, "Filter Manager: file contexts (fltmgr.sys)"),
    ("FMsl", false, "Filter Manager: stream lists (fltmgr.sys)"),
    ("AleE", false, "Windows network filtering (tcpip.sys)"),
    ("DxgK", false, "DirectX graphics kernel (dxgkrnl.sys)"),
];

const PREFIXES: &[(&str, bool, &str)] = &[
    ("Mm", true, "Windows memory manager"),
    ("Cc", true, "Windows file cache (cache manager)"),
    ("CM", true, "Registry (configuration manager)"),
    ("Etw", true, "Event tracing (ETW)"),
    ("Ob", true, "Windows object manager"),
    ("Se", true, "Windows security"),
    ("Ntf", false, "NTFS file system (ntfs.sys)"),
    ("NtF", false, "NTFS file system (ntfs.sys)"),
    ("FM", false, "Filter Manager (fltmgr.sys)"),
    ("Dxg", false, "DirectX graphics kernel (dxgkrnl.sys)"),
];

/// (windows_core, description) for a well-known tag.
fn describe(tag: &str) -> Option<(bool, &'static str)> {
    if let Some(&(_, core, d)) = KNOWN_TAGS.iter().find(|(t, _, _)| *t == tag) {
        return Some((core, d));
    }
    PREFIXES.iter().find(|(p, _, _)| tag.starts_with(p)).map(|&(_, core, d)| (core, d))
}

// ---------------------------------------------------------------- driver files

fn drivers_dir() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("drivers")
}

/// For each needle, whether `hay` contains it. One pass over `hay`.
fn contains_each(hay: &[u8], needles: &[[u8; 4]]) -> Vec<bool> {
    let mut found = vec![false; needles.len()];
    let mut first = [false; 256];
    for n in needles {
        first[n[0] as usize] = true;
    }
    let mut left = needles.len();
    if hay.len() < 4 || left == 0 {
        return found;
    }
    for i in 0..=hay.len() - 4 {
        if !first[hay[i] as usize] {
            continue;
        }
        let w = &hay[i..i + 4];
        for (k, n) in needles.iter().enumerate() {
            if !found[k] && w == n {
                found[k] = true;
                left -= 1;
            }
        }
        if left == 0 {
            break;
        }
    }
    found
}

fn loaded_sys_paths() -> Vec<PathBuf> {
    let loaded: Vec<PathBuf> = win::loaded_driver_paths()
        .into_iter()
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sys")) && p.is_file())
        .collect();
    if !loaded.is_empty() {
        return loaded;
    }
    let Ok(rd) = std::fs::read_dir(drivers_dir()) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sys")) && p.is_file())
        .collect()
}

/// Driver files containing each needle, up to DRIVERS_PER_TAG each, in file name order.
/// Returns `(file_name, full_path)` pairs so version info is read from each driver's actual location.
fn scan_driver_paths(paths: &[PathBuf], needles: &[[u8; 4]]) -> Vec<Vec<(String, PathBuf)>> {
    let mut hits = vec![Vec::new(); needles.len()];
    if needles.is_empty() {
        return hits;
    }
    let mut files: Vec<PathBuf> = paths
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sys")) && p.is_file())
        .cloned()
        .collect();
    files.sort_by(|a, b| {
        let ka = a.file_name().map(|n| n.to_ascii_lowercase());
        let kb = b.file_name().map(|n| n.to_ascii_lowercase());
        ka.cmp(&kb).then_with(|| a.to_string_lossy().to_ascii_lowercase().cmp(&b.to_string_lossy().to_ascii_lowercase()))
    });
    for path in files {
        let Ok(data) = std::fs::read(&path) else { continue };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for (k, yes) in contains_each(&data, needles).into_iter().enumerate() {
            if yes && hits[k].len() < DRIVERS_PER_TAG {
                hits[k].push((name.clone(), path.clone()));
            }
        }
        if hits.iter().all(|h| h.len() >= DRIVERS_PER_TAG) {
            break;
        }
    }
    hits
}

/// FileDescription from a file's version resource.
fn file_description(path: &Path) -> Option<String> {
    crate::doctor_win::version_strings(&path.to_string_lossy())
        .map(|(desc, _)| desc)
        .filter(|d| !d.is_empty())
}

// ---------------------------------------------------------------- report

fn top_windows_processes(procs: &[win::Proc]) -> (u64, Vec<WindowsProcess>) {
    let mut total = 0u64;
    // By lowercase name: (display name, processes, bytes), as engine::ram_breakdown counts them.
    let mut by: HashMap<String, (String, usize, u64)> = HashMap::new();
    for p in procs {
        let lower = p.name.to_ascii_lowercase();
        if p.pid == 0 || lower == "memory compression" {
            continue;
        }
        if p.pid == 4 || crate::breakdown::is_windows_process(&lower) {
            total += p.private;
            let e = by.entry(lower).or_insert_with(|| (p.name.clone(), 0, 0));
            e.1 += 1;
            e.2 += p.private;
        }
    }
    let mut v: Vec<(String, usize, u64)> = by.into_values().collect();
    v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    v.truncate(TOP_PROCESSES);
    (total, v.into_iter().map(|(name, processes, b)| WindowsProcess { name, processes, ram_gb: gb2(b) }).collect())
}

fn top_tags(mut raw: Vec<RawTag>) -> Vec<RawTag> {
    raw.sort_by(|a, b| b.total().cmp(&a.total()).then_with(|| a.tag.cmp(&b.tag)));
    raw.truncate(TOP_TAGS);
    raw.retain(|t| t.total() > 0);
    raw
}

/// Build the tag rows; `drivers[i]` are the files found for `raw[i]`.
fn tag_rows(raw: &[RawTag], drivers: Vec<Vec<DriverFile>>) -> Vec<PoolTag> {
    raw.iter()
        .zip(drivers)
        .map(|(t, d)| {
            let tag = tag_text(&t.tag);
            let known = describe(&tag);
            PoolTag {
                nonpaged_gb: gb2(t.nonpaged_used),
                paged_gb: gb2(t.paged_used),
                description: known.map(|k| k.1.to_string()),
                windows_core: known.map(|k| k.0).unwrap_or(false),
                drivers: d,
                tag,
            }
        })
        .collect()
}

/// Who holds a tag, in words.
fn holder(t: &PoolTag) -> String {
    if let Some(d) = &t.description {
        return d.clone();
    }
    match t.drivers.first() {
        Some(f) => match &f.name {
            Some(n) => format!("{} ({}, pool tag \"{}\")", n, f.file, t.tag),
            None => format!("{} (pool tag \"{}\")", f.file, t.tag),
        },
        None => format!("Pool tag \"{}\" (owner unknown)", t.tag),
    }
}

/// The verdict lines, from the bytes Windows can't page out and the top tags.
fn verdicts(nonpaged: u64, tags: &[PoolTag], tags_available: bool) -> Vec<String> {
    let mut v = Vec::new();
    let over = |g: f64| g * GB > TAG_HIGH as f64;
    for t in tags {
        if over(t.nonpaged_gb) {
            v.push(format!(
                "{} holds {:.1} GB of memory Windows can't page out. That is usually a driver leak: update or reinstall it, or restart Windows to get it back.",
                holder(t),
                t.nonpaged_gb
            ));
        } else if over(t.nonpaged_gb + t.paged_gb) {
            v.push(format!(
                "{} holds {:.1} GB of kernel memory. If it keeps growing, update or reinstall it, or restart Windows to get it back.",
                holder(t),
                t.nonpaged_gb + t.paged_gb
            ));
        }
    }
    let flagged: f64 = tags.iter().filter(|t| over(t.nonpaged_gb)).map(|t| t.nonpaged_gb).sum();
    if nonpaged > NONPAGED_HIGH && !v.is_empty() && flagged * GB < nonpaged as f64 / 2.0 {
        v.push(format!(
            "In all, Windows holds {:.1} GB of memory it can't page out, where 0.5 to 2 GB is usual. Restarting Windows gets it back.",
            gb1(nonpaged)
        ));
    }
    if nonpaged > NONPAGED_HIGH && v.is_empty() {
        v.push(format!(
            "Windows holds {:.1} GB of memory it can't page out, where 0.5 to 2 GB is usual. That is usually a driver leak; restarting Windows gets it back, and updating the driver stops it coming back.",
            gb1(nonpaged)
        ));
    }
    if !tags_available {
        v.push("Which driver holds it needs admin: run `memguard drivers` from a terminal opened as administrator.".to_string());
    }
    if v.is_empty() {
        v.push("Kernel memory looks normal: no driver holds an unusual amount.".to_string());
    }
    v
}

/// Read the counters and scan the driver files. Takes a second or two.
pub fn report() -> DriverReport {
    let sys = win::sys_info();
    let procs = win::list_processes();
    let (proc_bytes, processes) = top_windows_processes(&procs);
    let raw = query_pool_tags();
    let tags_available = raw.is_some();
    let top = top_tags(raw.unwrap_or_default());

    // Scan only for tags the Windows kernel doesn't own itself.
    let scan: Vec<usize> = (0..top.len()).filter(|&i| !describe(&tag_text(&top[i].tag)).map(|k| k.0).unwrap_or(false)).collect();
    let needles: Vec<[u8; 4]> = scan.iter().map(|&i| top[i].tag).collect();
    let paths = loaded_sys_paths();
    let hits = scan_driver_paths(&paths, &needles);
    let mut names: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut drivers = vec![Vec::new(); top.len()];
    for (k, &i) in scan.iter().enumerate() {
        for (file, full_path) in &hits[k] {
            let name = names.entry(full_path.clone()).or_insert_with(|| file_description(full_path)).clone();
            drivers[i].push(DriverFile { file: file.clone(), name });
        }
    }
    let tags = tag_rows(&top, drivers);

    let total = sys.nonpaged_pool + sys.resident_paged_pool + sys.resident_system_code + sys.resident_drivers + proc_bytes;
    DriverReport {
        nonpaged_pool_gb: gb1(sys.nonpaged_pool),
        paged_pool_gb: gb1(sys.resident_paged_pool),
        system_code_gb: gb1(sys.resident_system_code),
        drivers_code_gb: gb1(sys.resident_drivers),
        windows_processes_gb: gb1(proc_bytes),
        total_gb: gb1(total),
        processes,
        verdict: verdicts(sys.nonpaged_pool, &tags, tags_available),
        tags_available,
        tags,
    }
}

fn parts(r: &DriverReport) -> [(&'static str, f64); 5] {
    [
        ("Memory Windows can't page out (nonpaged pool)", r.nonpaged_pool_gb),
        ("Kernel memory that can be paged, in RAM now", r.paged_pool_gb),
        ("Windows' own code", r.system_code_gb),
        ("Driver code", r.drivers_code_gb),
        ("Windows processes", r.windows_processes_gb),
    ]
}

fn tag_owner(t: &PoolTag) -> String {
    let files = t.drivers.iter().map(|d| d.label()).collect::<Vec<_>>().join(", ");
    match (&t.description, files.is_empty()) {
        (Some(d), true) => d.clone(),
        (Some(d), false) => format!("{}; found in {}", d, files),
        (None, false) => format!("Likely {}", files),
        (None, true) => "Unknown".to_string(),
    }
}

fn is_bad(r: &DriverReport) -> bool {
    r.nonpaged_pool_gb * GB > NONPAGED_HIGH as f64 || r.tags.iter().any(|t| t.nonpaged_gb * GB > TAG_HIGH as f64)
}

/// What `memguard drivers` prints.
pub fn render_text(r: &DriverReport) -> String {
    let mut o = String::new();
    o.push_str("WINDOWS AND DRIVERS\n");
    o.push_str(&format!("{:.1} GB of RAM in use by Windows itself and its drivers.\n\n", r.total_gb));
    for (label, g) in parts(r) {
        o.push_str(&format!("  {:<48} {:>6.1} GB\n", label, g));
    }
    if !r.processes.is_empty() {
        o.push_str("\nLargest Windows processes:\n");
        for p in &r.processes {
            let name = if p.processes > 1 { format!("{} ({})", p.name, p.processes) } else { p.name.clone() };
            o.push_str(&format!("  {:<34} {:>6.2} GB\n", name, p.ram_gb));
        }
    }
    if r.tags_available {
        o.push_str("\nLargest kernel memory users (by pool tag):\n");
        o.push_str(&format!("  {:<6} {:>14} {:>9}  {}\n", "Tag", "Can't page out", "Paged", "What it is / likely driver"));
        for t in &r.tags {
            o.push_str(&format!("  {:<6} {:>11.2} GB {:>6.2} GB  {}\n", tag_shown(&t.tag), t.nonpaged_gb, t.paged_gb, tag_owner(t)));
        }
    }
    o.push_str("\nWhat it means:\n");
    for line in &r.verdict {
        o.push_str(&format!("  - {}\n", line));
    }
    o
}

/// A self-contained page (inline CSS, no scripts, nothing external) in the Ather look, like
/// the RAM and BIOS check page.
pub fn render_html(r: &DriverReport) -> String {
    let (cls, word) = if is_bad(r) { ("bad", "Needs a look") } else { ("ok", "Looks normal") };
    let mut h = String::new();
    h.push_str("<p class=\"eyebrow\">Ather MemGuard · Windows and drivers</p>\n");
    h.push_str(&format!("<p class=\"pill {}\">{}</p>\n", cls, word));
    h.push_str("<h1>Windows and drivers</h1>\n");
    h.push_str(&format!("<p class=\"summary\">{:.1} GB of RAM is in use by Windows itself and its drivers. Here is where it goes.</p>\n", r.total_gb));

    h.push_str("<section class=\"card\">\n<h2>What it means</h2>\n<ul class=\"findings\">\n");
    let li = if is_bad(r) { "problem" } else { "info" };
    for v in &r.verdict {
        h.push_str(&format!("<li class=\"{}\"><p>{}</p></li>\n", li, esc_rich(v)));
    }
    h.push_str("</ul>\n</section>\n");

    h.push_str("<section class=\"card\">\n<h2>Where it goes</h2>\n<dl>\n");
    for (label, g) in parts(r) {
        h.push_str(&format!("<dt>{}</dt><dd>{:.1} GB</dd>\n", esc(label), g));
    }
    h.push_str(&format!("<dt><strong>In all</strong></dt><dd><strong>{:.1} GB</strong></dd>\n</dl>\n</section>\n", r.total_gb));

    h.push_str("<section class=\"card\">\n<h2>Largest kernel memory users</h2>\n");
    if r.tags_available {
        h.push_str("<p class=\"muted\">By pool tag, the 4-letter label drivers put on their memory. Driver files are the ones that contain the tag: a strong hint, not proof.</p>\n");
        h.push_str("<div class=\"scroll\"><table class=\"nums\">\n<thead><tr><th>Tag</th><th>Can't page out</th><th>Paged</th><th>What it is / likely driver</th></tr></thead>\n<tbody>\n");
        for t in &r.tags {
            h.push_str(&format!(
                "<tr><td class=\"mono\">{}</td><td>{:.2} GB</td><td>{:.2} GB</td><td>{}</td></tr>\n",
                esc(&tag_shown(&t.tag)),
                t.nonpaged_gb,
                t.paged_gb,
                esc(&tag_owner(t))
            ));
        }
        h.push_str("</tbody></table></div>\n");
    } else {
        h.push_str("<p class=\"muted\">Which driver holds it needs admin: run <code>memguard drivers</code> from a terminal opened as administrator.</p>\n");
    }
    h.push_str("</section>\n");

    if !r.processes.is_empty() {
        h.push_str("<section class=\"card\">\n<h2>Largest Windows processes</h2>\n<div class=\"scroll\"><table class=\"nums\">\n<thead><tr><th>Process</th><th>Count</th><th>RAM only it uses</th></tr></thead>\n<tbody>\n");
        for p in &r.processes {
            h.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{:.2} GB</td></tr>\n", esc(&p.name), p.processes, p.ram_gb));
        }
        h.push_str("</tbody></table></div>\n</section>\n");
    }
    h.push_str("<p class=\"foot\">Read from Windows' own counters and the loaded driver files. Nothing was changed.</p>\n");
    page("Windows and drivers", &h)
}


// ---------------------------------------------------------------- unit tests

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    fn buffer(entries: &[(&[u8; 4], u64, u64)], count: u32) -> Vec<u8> {
        let mut b = vec![0u8; ENTRIES_OFFSET];
        b[..4].copy_from_slice(&count.to_le_bytes());
        for (tag, paged, nonpaged) in entries {
            let mut e = [0u8; ENTRY_SIZE];
            e[..4].copy_from_slice(*tag);
            e[4..8].copy_from_slice(&7u32.to_le_bytes()); // allocs
            e[8..12].copy_from_slice(&3u32.to_le_bytes()); // frees
            e[16..24].copy_from_slice(&paged.to_le_bytes());
            e[24..28].copy_from_slice(&9u32.to_le_bytes());
            e[28..32].copy_from_slice(&1u32.to_le_bytes());
            e[32..40].copy_from_slice(&nonpaged.to_le_bytes());
            b.extend_from_slice(&e);
        }
        b
    }

    #[test]
    fn parses_a_synthetic_buffer() {
        let b = buffer(&[(b"Ntfx", 100, 200), (b"Irp ", 0, 5 * G)], 2);
        let t = parse_pool_tags(&b);
        assert_eq!(t, [RawTag { tag: *b"Ntfx", paged_used: 100, nonpaged_used: 200 }, RawTag { tag: *b"Irp ", paged_used: 0, nonpaged_used: 5 * G }]);
    }

    #[test]
    fn truncated_buffers_are_safe() {
        let b = buffer(&[(b"Ntfx", 100, 200), (b"Abcd", 1, 2)], 2);
        assert_eq!(parse_pool_tags(&b[..b.len() - 1]).len(), 1);
        assert_eq!(parse_pool_tags(&b[..ENTRIES_OFFSET + 3]).len(), 0);
        assert!(parse_pool_tags(&b[..2]).is_empty());
        assert!(parse_pool_tags(&[]).is_empty());
        // A Count larger than the buffer holds.
        assert_eq!(parse_pool_tags(&buffer(&[(b"Ntfx", 1, 1)], 1_000_000)).len(), 1);
    }

    #[test]
    fn tag_text_formatting() {
        assert_eq!(tag_text(b"Irp "), "Irp ");
        assert_eq!(tag_text(&[b'A', 0, 0xff, b'z']), "A..z");
        assert_eq!(tag_shown("Irp "), "Irp\u{b7}");
        assert_eq!(tag_shown("Ntf0"), "Ntf0");
        assert_eq!(tag_shown("Vi  "), "Vi\u{b7}\u{b7}");
        assert_eq!(describe("Ntfx").map(|d| d.0), Some(false));
        assert_eq!(describe("MmZz").map(|d| d.0), Some(true));
        assert!(describe("Zzzz").is_none());
    }

    #[test]
    fn finds_every_needle_in_one_pass() {
        let hay = b"xxNtfxyyIrp zz";
        assert_eq!(contains_each(hay, &[*b"Irp ", *b"Ntfx", *b"Nope"]), [true, true, false]);
        assert_eq!(contains_each(b"abc", &[*b"abcd"]), [false]);
    }

    fn tag(t: &str, nonpaged_gb: f64, paged_gb: f64, drivers: &[&str]) -> PoolTag {
        let d = describe(t);
        PoolTag {
            tag: t.into(),
            nonpaged_gb,
            paged_gb,
            description: d.map(|x| x.1.to_string()),
            windows_core: d.map(|x| x.0).unwrap_or(false),
            drivers: drivers.iter().map(|f| DriverFile { file: f.to_string(), name: Some("Acme Widget".into()) }).collect(),
        }
    }

    #[test]
    fn verdict_thresholds() {
        let calm = [tag("Ntfx", 0.2, 0.3, &[])];
        let v = verdicts(G, &calm, true);
        assert_eq!(v.len(), 1);
        assert!(v[0].starts_with("Kernel memory looks normal"));

        let leak = [tag("Wdgt", 3.5, 0.0, &["acme.sys"]), tag("Ntfx", 0.2, 0.3, &[])];
        let v = verdicts(5 * G, &leak, true);
        assert_eq!(v.len(), 1);
        assert!(v[0].starts_with("Acme Widget (acme.sys, pool tag \"Wdgt\") holds 3.5 GB of memory Windows can't page out."), "{}", v[0]);
        // A flagged tag that explains less than half of a high total: also the general line.
        let v = verdicts(8 * G, &leak, true);
        assert_eq!(v.len(), 2);
        assert!(v[1].starts_with("In all, Windows holds 8.0 GB"), "{}", v[1]);

        // High total but spread out: the general line.
        let v = verdicts(5 * G, &calm, true);
        assert!(v[0].starts_with("Windows holds 5.0 GB of memory it can't page out"), "{}", v[0]);

        // Paged-only big tag.
        let v = verdicts(G, &[tag("Zzzz", 0.1, 1.5, &[])], true);
        assert!(v[0].contains("1.6 GB of kernel memory"), "{}", v[0]);

        // No tags: says admin is needed.
        let v = verdicts(5 * G, &[], false);
        assert_eq!(v.len(), 2);
        assert!(v[1].contains("needs admin"));
        // Exactly at a threshold is not over it.
        assert!(verdicts(NONPAGED_HIGH, &[tag("Wdgt", 1.0, 0.0, &[])], true)[0].starts_with("Kernel memory looks normal"));
    }

    fn sample() -> DriverReport {
        let tags = vec![tag("Wdgt", 3.5, 0.0, &["acme.sys"]), tag("Irp ", 0.1, 0.0, &[])];
        DriverReport {
            nonpaged_pool_gb: 5.6,
            paged_pool_gb: 1.2,
            system_code_gb: 0.1,
            drivers_code_gb: 0.1,
            windows_processes_gb: 2.4,
            total_gb: 9.4,
            processes: vec![WindowsProcess { name: "svchost".into(), processes: 80, ram_gb: 1.2 }],
            verdict: verdicts(6 * G, &tags, true),
            tags_available: true,
            tags,
        }
    }

    #[test]
    fn html_shows_the_top_tag() {
        let h = render_html(&sample());
        assert!(h.contains("<title>Windows and drivers</title>"));
        assert!(h.contains("Wdgt"));
        assert!(h.contains("Acme Widget (acme.sys)"));
        assert!(h.contains("Irp\u{b7}"));
        assert!(h.contains("pill bad"));
        let t = render_text(&sample());
        assert!(t.contains("Wdgt") && t.contains("9.4 GB"));
        for s in [&h, &t] {
            let l = s.to_lowercase();
            assert!(!l.contains("working set") && !l.contains("commit charge"));
        }
        let mut no = sample();
        no.tags_available = false;
        no.tags.clear();
        assert!(render_html(&no).contains("needs admin"));
    }

    #[test]
    fn live_report_has_positive_totals() {
        let r = report();
        assert!(r.nonpaged_pool_gb > 0.0 && r.total_gb > 0.0, "{:?}", r);
        assert!(r.windows_processes_gb > 0.0);
        if r.tags_available {
            assert!(!r.tags.is_empty());
        }
    }

    #[test]
    fn scan_driver_paths_handles_custom_dirs_and_filters_non_sys() {
        let dir = std::env::temp_dir().join(format!("amg-drivers-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub_a = dir.join("DriverStore").join("a");
        let sub_b = dir.join("Vendor").join("b");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();

        let sys_z = sub_a.join("z_first_tag.sys");
        let sys_a = sub_b.join("a_driver.sys");
        let sys_b = sub_a.join("b_driver.SYS");
        let sys_c = sub_b.join("c_driver.sys");
        let sys_d = sub_a.join("d_driver.sys");
        let dll = sub_a.join("ignored.dll");
        let exe = sub_b.join("ignored.exe");
        let missing = sub_a.join("missing.sys");

        std::fs::write(&sys_z, b"xxAbCdxx").unwrap();
        std::fs::write(&sys_a, b"xxAbCdxx").unwrap();
        std::fs::write(&sys_b, b"xxAbCdxx").unwrap();
        std::fs::write(&sys_c, b"xxAbCdxx").unwrap();
        std::fs::write(&sys_d, b"xxAbCdxx").unwrap();
        std::fs::write(&dll, b"xxAbCdxx").unwrap();
        std::fs::write(&exe, b"xxAbCdxx").unwrap();

        let paths = vec![sys_z.clone(), dll, exe, missing, sys_d, sys_c, sys_b.clone(), sys_a.clone()];
        let hits = scan_driver_paths(&paths, &[*b"AbCd", *b"Nope"]);
        assert_eq!(hits.len(), 2);
        // Sorted case-insensitively by filename, capped at DRIVERS_PER_TAG (3).
        assert_eq!(
            hits[0],
            vec![
                ("a_driver.sys".to_string(), sys_a),
                ("b_driver.SYS".to_string(), sys_b),
                ("c_driver.sys".to_string(), sub_b.join("c_driver.sys")),
            ]
        );
        assert!(hits[1].is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

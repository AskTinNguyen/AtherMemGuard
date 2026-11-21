//! The disk checkup (report only) and `memguard disk plan`, built on the accounting in
//! `disk_space` (proposal sections 3, 4 D and 5).
//!
//! Finding items is cheap and runs where it is asked for: the catalogue (`disk_catalog.toml`)
//! names folders inside Unreal projects and well-known caches, and lanes are other checkouts
//! of the same project (git worktrees, or sibling folders with the same `.uproject`). Counting
//! them is not cheap, so it never runs in the tray app: `memguard disk scan-child` does it in a
//! child process at background priority, with a thread limit and a time budget, and stops if
//! a build or cook starts. Results are cached per item (keyed by the root's file id and the
//! newest write seen under it); a cached answer is only ever an `estimate` once it is older
//! than `fresh_sec`.
//!
//! Every "frees" number is a prediction from the volume's reference counts. It has not been
//! confirmed against a real delete, and on ReFS deleted space can take a long time to show up
//! as free, so the reports say `basis: "predicted"` next to every one of them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Config, DiskSettings};
use crate::disk_space::{self, gb2, norm, total_confidence, Confidence, ScanFacts};
use crate::disk_win::{self, WalkStats};

pub const CATALOG_TOML: &str = include_str!("disk_catalog.toml");

/// Said next to every "frees" number.
pub const BASIS: &str = "predicted";
/// The volume note for ReFS (the controlled delete test on the team PC saw no space come
/// back for more than 20 minutes).
pub const REFS_DELAY_NOTE: &str = "Deleting on this drive may take a long time to show up as free space.";
pub const DO_NOT_SUM: &str = "logical_gb, allocated_gb and footprint_gb overlap between items, and frees_alone_gb under-counts removing several items. \
Quote totals[].frees_together_gb, or call memguard_disk_plan. Every frees number is predicted from the drive's reference counts, not measured: \
never promise the space right after a delete.";

/// Folders never searched for project roots (big trees that hold none).
const NO_DESCEND: &[&str] = &["content", "intermediate", "saved", "binaries", "deriveddatacache", "plugins", "source", "node_modules", "build", "config", "target", "$recycle.bin", "system volume information"];

// ---------------------------------------------------------------- the catalogue

#[derive(Debug, Clone, Deserialize)]
struct CatalogFile {
    project: Vec<CatalogEntry>,
    global: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct CatalogEntry {
    key: String,
    class: String,
    title: String,
    path: String,
    #[serde(default)]
    plugins: bool,
    #[serde(default)]
    older_than_days: Option<u64>,
    /// The tool whose own clean command can empty it (`npm`, `pip`, `dotnet`, `git-lfs`, ...).
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    diagnostic: bool,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    marker: Option<String>,
    #[serde(default)]
    no_walk: bool,
}

fn catalog() -> &'static CatalogFile {
    static C: OnceLock<CatalogFile> = OnceLock::new();
    C.get_or_init(|| toml::from_str(CATALOG_TOML).expect("the built-in disk catalogue is valid"))
}

/// What the catalogue says about one key: the cleanup's allow-list.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogInfo {
    pub class: String,
    /// Relative to the project root, or (global) starting with a variable like `%LOCALAPPDATA%`.
    pub path: String,
    pub plugins: bool,
    pub global: bool,
    pub older_than_days: Option<u64>,
    pub tool: Option<String>,
    pub diagnostic: bool,
    pub glob: Option<String>,
    pub marker: Option<String>,
    pub no_walk: bool,
}

pub fn catalog_info(key: &str) -> Option<CatalogInfo> {
    let c = catalog();
    let f = |e: &CatalogEntry, global: bool| CatalogInfo {
        class: e.class.clone(),
        path: e.path.clone(),
        plugins: e.plugins,
        global,
        older_than_days: e.older_than_days,
        tool: e.tool.clone(),
        diagnostic: e.diagnostic,
        glob: e.glob.clone(),
        marker: e.marker.clone(),
        no_walk: e.no_walk,
    };
    c.project.iter().find(|e| e.key == key).map(|e| f(e, false)).or_else(|| c.global.iter().find(|e| e.key == key).map(|e| f(e, true)))
}

/// Class order in reports.
pub fn class_rank(class: &str) -> usize {
    match class {
        "regenerable" => 0,
        "disposable" => 1,
        "rebuild_cost" => 2,
        _ => 3,
    }
}

/// One thing the checkup lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatItem {
    pub key: String,
    pub class: String,
    pub title: String,
    pub paths: Vec<String>,
    /// The lane this item sits in, when the lane is listed too (its numbers exclude this item).
    #[serde(default)]
    pub lane: Option<String>,
}

fn is_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.is_dir() && !m.file_type().is_symlink()).unwrap_or(false)
}

fn is_reparse(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p).map(|m| m.file_attributes() & disk_win::A_REPARSE != 0).unwrap_or(false)
}

/// Does `dir` hold a real `.uproject` file directly (see `is_real_uproject`)?
pub fn has_uproject(dir: &Path) -> bool {
    !uprojects(dir).is_empty()
}

/// Does `dir` hold any file named `*.uproject`, real or not? (For refusals: a folder that looks
/// like a project is never deleted.)
pub fn has_any_uproject(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".uproject"))).unwrap_or(false)
}

/// The biggest `.uproject` read (real ones are a few KB).
pub const UPROJECT_MAX_BYTES: u64 = 1 << 20;

/// Is `path` a real Unreal project file: a regular file of at most 1 MB that parses as a JSON
/// object with `FileVersion` or `EngineAssociation`? A stray or empty `.uproject` (`{}`, "not
/// json") would otherwise make its siblings cleanable.
pub fn is_real_uproject(path: &Path) -> bool {
    let Ok(m) = std::fs::symlink_metadata(path) else { return false };
    if !m.is_file() || m.len() > UPROJECT_MAX_BYTES {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else { return false };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<Value>(text).ok().and_then(|v| v.as_object().map(|o| o.contains_key("FileVersion") || o.contains_key("EngineAssociation"))).unwrap_or(false)
}

/// The real `.uproject` file names directly in `dir`.
fn uprojects(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            (n.to_ascii_lowercase().ends_with(".uproject") && is_real_uproject(&e.path())).then_some(n)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct DiscoveredRoot {
    pub path: PathBuf,
    pub has_uproject: bool,
    pub is_engine: bool,
    pub has_git: bool,
    pub is_bare_git: bool,
}

fn is_ue_engine_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Engine"))
        && dir.join(r"Build\Build.version").is_file()
}

fn has_git_entry(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
}

fn is_bare_git_dir(dir: &Path) -> bool {
    !has_git_entry(dir) && dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

fn is_kept_path(p: &Path, keep: &[String]) -> bool {
    if keep.is_empty() {
        return false;
    }
    let np = norm(&p.display().to_string());
    keep.iter().any(|k| disk_space::is_within(&np, k))
}

/// Project, UE Engine, and Git repository roots at or under `root`, up to `depth` levels down.
pub fn find_roots(root: &Path, depth: u32) -> Vec<DiscoveredRoot> {
    find_roots_with_keep(root, depth, &[])
}

fn find_roots_with_keep(root: &Path, depth: u32, keep: &[String]) -> Vec<DiscoveredRoot> {
    let mut out: Vec<DiscoveredRoot> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut record = |cand: DiscoveredRoot| {
        let k = norm(&cand.path.display().to_string());
        if let Some(&idx) = seen.get(&k) {
            out[idx].has_uproject |= cand.has_uproject;
            out[idx].is_engine |= cand.is_engine;
            out[idx].has_git |= cand.has_git;
            out[idx].is_bare_git |= cand.is_bare_git;
        } else {
            seen.insert(k, out.len());
            out.push(cand);
        }
    };

    let mut level = vec![root.to_path_buf()];
    for d in 0..=depth {
        let mut next = Vec::new();
        for dir in level {
            if d > 0 && is_reparse(&dir) {
                continue;
            }
            if is_kept_path(&dir, keep) {
                continue;
            }
            let has_up = !uprojects(&dir).is_empty();
            let is_eng = is_ue_engine_dir(&dir);
            let has_g = has_git_entry(&dir);
            let is_bare = !has_g && !has_up && !is_eng && is_bare_git_dir(&dir);
            let has_marker = !has_up && !is_eng && !has_g && dir.join("Cargo.toml").is_file() && dir.join("target").is_dir();

            if has_up || is_eng || has_g || is_bare || has_marker {
                record(DiscoveredRoot {
                    path: dir.clone(),
                    has_uproject: has_up,
                    is_engine: is_eng,
                    has_git: has_g,
                    is_bare_git: is_bare,
                });
            }

            // Standalone UE source trees: emit `<dir>\Engine` directly as `is_engine` root so
            // `is_recognised_root` matches `Engine` even when `dir` is at `d == depth` or is a
            // `.git` monorepo root.
            if !has_up && !is_eng {
                let eng = dir.join("Engine");
                if !is_kept_path(&eng, keep) && eng.join(r"Build\Build.version").is_file() && !is_reparse(&eng) {
                    record(DiscoveredRoot {
                        path: eng,
                        has_uproject: false,
                        is_engine: true,
                        has_git: false,
                        is_bare_git: false,
                    });
                }
            }

            if has_up || is_eng || is_bare || d == depth {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                if n.starts_with('.') || n.starts_with('$') || NO_DESCEND.contains(&n.as_str()) {
                    continue;
                }
                let p = e.path();
                if !is_kept_path(&p, keep) && is_dir(&p) {
                    next.push(p);
                }
            }
        }
        level = next;
        if level.is_empty() {
            break;
        }
    }
    out
}

/// Unreal project and repository roots at or under `root`, at most `depth` levels down.
pub fn find_projects(root: &Path, depth: u32) -> Vec<PathBuf> {
    find_roots(root, depth).into_iter().map(|r| r.path).collect()
}

/// Does a Dev Drive hold Unreal projects near its root (two levels)? Cached per process.
pub fn has_projects_near(root: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let m = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = m.lock().unwrap().get(root) {
        return *v;
    }
    let v = !find_projects(Path::new(root), 2).is_empty();
    m.lock().unwrap().insert(root.to_string(), v);
    v
}

/// Collapse `.` and `..` components lexically without introducing `\\?\`.
pub fn normalize_lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let s = p.to_string_lossy().replace('/', "\\");
    let mut out: Vec<Component<'_>> = Vec::new();
    let p_norm = PathBuf::from(&s);
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    let mut prefix: Option<std::ffi::OsString> = None;
    let mut has_root = false;
    for comp in p_norm.components() {
        match comp {
            Component::Prefix(pr) => prefix = Some(pr.as_os_str().to_os_string()),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if !stack.is_empty() && stack.last().is_some_and(|x| x != "..") {
                    stack.pop();
                } else if !has_root {
                    stack.push("..".into());
                }
            }
            Component::Normal(x) => stack.push(x.to_os_string()),
        }
    }
    let _ = &mut out;
    let mut res = PathBuf::new();
    if let Some(pr) = prefix {
        res.push(pr);
    }
    if has_root {
        res.push(std::path::MAIN_SEPARATOR_STR);
    }
    for s in stack {
        res.push(s);
    }
    res
}

fn resolve_raw_git_path(base_dir: &Path, raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let rest = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("gitdir:") {
        trimmed[7..].trim()
    } else {
        trimmed
    };
    if rest.is_empty() {
        return None;
    }
    let replaced = rest.replace('/', "\\");
    let p = Path::new(&replaced);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base_dir.join(p) };
    Some(normalize_lexical(&joined))
}

/// Resolve a `gitdir` file target (supporting Git 2.48+ relative `gitdir:` paths) against `base_dir`,
/// stripping a trailing `.git` component.
pub fn resolve_gitdir_target(base_dir: &Path, text: &str) -> Option<PathBuf> {
    let mut p = resolve_raw_git_path(base_dir, text)?;
    if p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(".git")) {
        p.pop();
    }
    (!p.as_os_str().is_empty()).then_some(p)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeOrigin {
    pub wt_gitdir: PathBuf,
    pub common_git_dir: PathBuf,
    pub main_repo: PathBuf,
    pub is_bare: bool,
}

/// Reverse-resolve a worktree `.git` file (`gitdir: ...`) -> `wt_gitdir` -> `commondir` -> `main_repo`.
/// Returns `None` for submodules (which lack a `commondir` file).
pub fn resolve_worktree_links(wt_root: &Path) -> Option<WorktreeOrigin> {
    let dot_git = wt_root.join(".git");
    let m = std::fs::symlink_metadata(&dot_git).ok()?;
    if !m.is_file() || m.len() > 65_536 {
        return None;
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let line = text.lines().next()?.trim();
    if !(line.len() >= 7 && line[..7].eq_ignore_ascii_case("gitdir:")) {
        return None;
    }
    let wt_gitdir = resolve_raw_git_path(wt_root, line)?;
    let commondir_file = wt_gitdir.join("commondir");
    if !commondir_file.is_file() {
        // Submodules point into `.git\modules\<name>` and do NOT have a `commondir` file.
        return None;
    }
    let common_raw = std::fs::read_to_string(&commondir_file).ok()?;
    let common_git_dir = resolve_raw_git_path(&wt_gitdir, common_raw.lines().next().unwrap_or(""))?;
    let (main_repo, is_bare) = if common_git_dir.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(".git")) {
        (common_git_dir.parent()?.to_path_buf(), false)
    } else {
        (common_git_dir.clone(), true)
    };
    Some(WorktreeOrigin { wt_gitdir, common_git_dir, main_repo, is_bare })
}

/// A git worktree: `.git` is a file (`gitdir: ...`) pointing to a worktree admin dir with `commondir`
/// (distinguishing worktrees from submodules).
pub fn is_worktree(root: &Path) -> bool {
    let dot_git = root.join(".git");
    let Ok(m) = std::fs::symlink_metadata(&dot_git) else { return false };
    if !m.is_file() {
        return false;
    }
    if resolve_worktree_links(root).is_some() {
        return true;
    }
    // If target dir is gone or unreadable, distinguish submodule (`\modules\`) vs worktree (`\worktrees\`).
    if let Ok(text) = std::fs::read_to_string(&dot_git) {
        let lower = text.to_ascii_lowercase().replace('/', "\\");
        if lower.contains("\\modules\\") {
            return false;
        }
        return lower.contains("\\worktrees\\");
    }
    false
}

/// The worktrees git knows for a common git dir (`<repo>\.git\worktrees\*\gitdir`), resolving
/// both absolute and Git 2.48+ relative `gitdir:` paths.
pub fn worktrees_of_gitdir(common_git_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(common_git_dir.join("worktrees")) else { return out };
    for e in rd.flatten() {
        let wt_admin = e.path();
        if let Ok(text) = std::fs::read_to_string(wt_admin.join("gitdir")) {
            if let Some(p) = resolve_gitdir_target(&wt_admin, &text) {
                if is_dir(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// The worktrees git knows for the repo at `root` (from `.git\worktrees\*\gitdir`).
pub fn worktrees_of(root: &Path) -> Vec<PathBuf> {
    worktrees_of_gitdir(&root.join(".git"))
}

/// `D:/lanes/x/.git` in a worktree's `gitdir` file -> `D:\lanes\x`.
pub fn parse_gitdir_target(text: &str) -> Option<PathBuf> {
    let t = text.trim().replace('/', "\\");
    let t = t.strip_suffix(r"\.git").unwrap_or(&t);
    (!t.is_empty()).then(|| PathBuf::from(t))
}

fn lane_title(p: &Path) -> String {
    format!("Lane {}", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string()))
}

fn is_regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.is_file() && !m.file_type().is_symlink()).unwrap_or(false)
}

fn is_dir_or_file(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|m| !m.file_type().is_symlink() && (m.file_attributes() & disk_win::A_REPARSE == 0) && (m.is_dir() || m.is_file()))
        .unwrap_or(false)
}

fn wildcard_match_ci(pattern: &str, name: &str) -> bool {
    let p: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let s: Vec<u8> = name.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut match_si) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            match_si = si;
        } else if let Some(st) = star {
            pi = st + 1;
            match_si += 1;
            si = match_si;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn collect_ellipsis_dirs(dir: &Path, suffix_parts: &[&str], depth: u32, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !is_dir(&p) || is_reparse(&p) {
            continue;
        }
        if suffix_parts.len() == 1 && e.file_name().to_string_lossy().eq_ignore_ascii_case(suffix_parts[0]) {
            out.push(p.display().to_string());
        } else if depth > 0 {
            collect_ellipsis_dirs(&p, suffix_parts, depth - 1, out);
        }
    }
}

fn dir_older_than(p: &Path, days: u64) -> bool {
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(days * 86400);
    let mtime_ok = |q: &Path| {
        std::fs::symlink_metadata(q)
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(true)
    };
    if !mtime_ok(p) {
        return false;
    }
    for sub in ["debug", "release"] {
        let sp = p.join(sub);
        if is_dir(&sp) && !mtime_ok(&sp) {
            return false;
        }
    }
    true
}

/// The catalogue's items inside one project root.
fn project_items(root: &Path, project: &str, lane: Option<&str>) -> Vec<CatItem> {
    let mut out = Vec::new();
    for e in &catalog().project {
        if e.diagnostic {
            continue;
        }
        if let Some(m) = &e.marker {
            if !root.join(m).is_file() {
                continue;
            }
        }
        let paths: Vec<String> = if e.plugins {
            let mut v = Vec::new();
            plugin_dirs(&root.join("Plugins"), &e.path, 3, &mut v);
            v
        } else if let Some((prefix, suffix)) = e.path.split_once(r"\...\") {
            let mut v = Vec::new();
            let base = root.join(prefix);
            if is_dir(&base) && !is_reparse(&base) {
                let suffix_parts: Vec<&str> = suffix.split('\\').filter(|s| !s.is_empty()).collect();
                collect_ellipsis_dirs(&base, &suffix_parts, 4, &mut v);
            }
            v
        } else if let Some(glob) = &e.glob {
            let dir = root.join(&e.path);
            let mut v = Vec::new();
            if is_dir(&dir) {
                let cutoff = e.older_than_days.map(|d| std::time::SystemTime::now() - Duration::from_secs(d * 86400));
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for ent in rd.flatten() {
                        let fp = ent.path();
                        let name = ent.file_name().to_string_lossy().to_string();
                        if !wildcard_match_ci(glob, &name) || !is_regular_file(&fp) || is_reparse(&fp) {
                            continue;
                        }
                        if let Some(cut) = cutoff {
                            let ok = std::fs::symlink_metadata(&fp)
                                .and_then(|m| m.modified())
                                .map(|t| t < cut)
                                .unwrap_or(false);
                            if !ok {
                                continue;
                            }
                        }
                        v.push(fp.display().to_string());
                    }
                }
            }
            v
        } else {
            let p = root.join(&e.path);
            if is_dir(&p) && e.older_than_days.is_none_or(|d| dir_older_than(&p, d)) {
                vec![p.display().to_string()]
            } else {
                Vec::new()
            }
        };
        if !paths.is_empty() {
            out.push(CatItem { key: e.key.clone(), class: e.class.clone(), title: format!("{} for {}", e.title, project), paths, lane: lane.map(str::to_string) });
        }
    }
    out
}

/// `Plugins\**\<leaf>` folders of plugins (a folder with a `.uplugin`), at most `depth` deep.
fn plugin_dirs(dir: &Path, leaf: &str, depth: u32, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !is_dir(&p) || is_reparse(&p) {
            continue;
        }
        let is_plugin = std::fs::read_dir(&p).map(|r| r.flatten().any(|f| f.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".uplugin"))).unwrap_or(false);
        if is_plugin {
            let l = p.join(leaf);
            if is_dir(&l) {
                out.push(l.display().to_string());
            }
        } else if depth > 0 {
            plugin_dirs(&p, leaf, depth - 1, out);
        }
    }
}

fn non_empty_env_dir(var: &str) -> Option<String> {
    let raw = std::env::var(var).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("None") {
        return None;
    }
    let expanded = crate::disk_relocate::expand_vars(trimmed);
    let t = expanded.trim().trim_end_matches(['\\', '/']).to_string();
    (!t.is_empty() && !t.contains('%')).then_some(t)
}

fn relocated_var_dir(var: &str) -> Option<String> {
    let rec = crate::disk_clean::DiskFiles::default().relocate_record().filter(|r| !r.undone)?;
    let entry = rec.entries.iter().find(|e| e.name.eq_ignore_ascii_case(var))?;
    let trimmed = entry.new_value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("None") {
        return None;
    }
    let expanded = crate::disk_relocate::expand_vars(trimmed);
    let t = expanded.trim().trim_end_matches(['\\', '/']).to_string();
    (!t.is_empty() && !t.contains('%')).then_some(t)
}

fn resolve_catalog_var(var: &str) -> Option<String> {
    let (ddc, zen) = crate::disk::ue_cache_dirs();
    let local = || non_empty_env_dir("LOCALAPPDATA");
    let profile = || non_empty_env_dir("USERPROFILE");
    match var.to_ascii_uppercase().as_str() {
        "DDC" => ddc.map(|p| p.display().to_string().trim_end_matches(['\\', '/']).to_string()),
        "ZEN" => zen.map(|p| p.display().to_string().trim_end_matches(['\\', '/']).to_string()),
        "TEMP" => Some(std::env::temp_dir().display().to_string().trim_end_matches(['\\', '/']).to_string()),
        "LOCALAPPDATA" => local(),
        "APPDATA" => non_empty_env_dir("APPDATA"),
        "USERPROFILE" => profile(),
        "PROGRAMDATA" => non_empty_env_dir("ProgramData").or_else(|| non_empty_env_dir("PROGRAMDATA")),
        "SYSTEMROOT" | "WINDIR" => non_empty_env_dir("SystemRoot").or_else(|| non_empty_env_dir("WINDIR")),
        "NPM_CONFIG_CACHE" => non_empty_env_dir("npm_config_cache")
            .or_else(|| relocated_var_dir("npm_config_cache"))
            .or_else(|| local().map(|l| format!(r"{}\npm-cache", l))),
        "PIP_CACHE_DIR" => non_empty_env_dir("PIP_CACHE_DIR")
            .or_else(|| relocated_var_dir("PIP_CACHE_DIR"))
            .or_else(|| local().map(|l| format!(r"{}\pip\cache", l))),
        "UV_CACHE_DIR" => non_empty_env_dir("UV_CACHE_DIR")
            .or_else(|| relocated_var_dir("UV_CACHE_DIR"))
            .or_else(|| local().map(|l| format!(r"{}\uv\cache", l))),
        "PNPM_STORE_DIR" => non_empty_env_dir("PNPM_STORE_DIR")
            .or_else(|| relocated_var_dir("PNPM_STORE_DIR"))
            .or_else(|| local().map(|l| format!(r"{}\pnpm\store", l))),
        "YARN_CACHE_FOLDER" => non_empty_env_dir("YARN_CACHE_FOLDER")
            .or_else(|| relocated_var_dir("YARN_CACHE_FOLDER"))
            .or_else(|| local().map(|l| format!(r"{}\Yarn\Cache", l))),
        "BUN_INSTALL_CACHE_DIR" => non_empty_env_dir("BUN_INSTALL_CACHE_DIR")
            .or_else(|| relocated_var_dir("BUN_INSTALL_CACHE_DIR"))
            .or_else(|| profile().map(|u| format!(r"{}\.bun\install\cache", u))),
        "NUGET_PACKAGES" => non_empty_env_dir("NUGET_PACKAGES")
            .or_else(|| relocated_var_dir("NUGET_PACKAGES"))
            .or_else(|| profile().map(|u| format!(r"{}\.nuget\packages", u))),
        "VCPKG_DEFAULT_BINARY_CACHE" => non_empty_env_dir("VCPKG_DEFAULT_BINARY_CACHE")
            .or_else(|| relocated_var_dir("VCPKG_DEFAULT_BINARY_CACHE"))
            .or_else(|| local().map(|l| format!(r"{}\vcpkg\archives", l))),
        "CARGO_HOME" => non_empty_env_dir("CARGO_HOME").or_else(|| profile().map(|u| format!(r"{}\.cargo", u))),
        "SCCACHE_DIR" => non_empty_env_dir("SCCACHE_DIR")
            .or_else(|| relocated_var_dir("SCCACHE_DIR"))
            .or_else(|| local().map(|l| format!(r"{}\Mozilla\sccache", l))),
        _ => non_empty_env_dir(var).or_else(|| relocated_var_dir(var)),
    }
}

pub(crate) fn is_protected_system_root(p: &Path) -> bool {
    let np = norm(&p.display().to_string());
    if np.is_empty() || np.len() <= 3 && np.as_bytes().get(1) == Some(&b':') {
        return true;
    }
    for var in ["SystemRoot", "WINDIR", "ProgramFiles", "ProgramFiles(x86)", "ProgramData", "USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
        if let Some(val) = non_empty_env_dir(var) {
            if norm(&val) == np {
                return true;
            }
        }
    }
    false
}

/// A `[[global]]` catalogue path with its variable filled in.
pub fn expand_global(path: &str) -> Option<PathBuf> {
    let s = if let Some(rest) = path.strip_prefix('%') {
        if let Some((var, tail)) = rest.split_once('%') {
            let val = resolve_catalog_var(var)?;
            format!("{}{}", val.trim_end_matches(['\\', '/']), tail)
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    let pb = PathBuf::from(&s);
    if is_protected_system_root(&pb) {
        return None;
    }
    Some(pb)
}

/// Expand a `[[global]]` catalogue path (supporting `*` wildcard segments) into existing
/// non-reparse directories or regular files.
pub fn expand_global_paths(path: &str) -> Vec<PathBuf> {
    let Some(expanded) = expand_global(path) else { return Vec::new() };
    let s = expanded.display().to_string();
    if !s.contains('*') {
        return if is_dir_or_file(&expanded) { vec![expanded] } else { Vec::new() };
    }
    let parts: Vec<&str> = s.split('\\').collect();
    let Some((first_wc, _)) = parts.iter().enumerate().find(|(_, p)| p.contains('*')) else {
        return if is_dir_or_file(&expanded) { vec![expanded] } else { Vec::new() };
    };
    if first_wc == 0 {
        return Vec::new();
    }
    let mut base = parts[..first_wc].join("\\");
    if base.len() == 2 && base.ends_with(':') {
        base.push('\\');
    }
    let mut cur = vec![PathBuf::from(base)];
    for &seg in &parts[first_wc..] {
        let mut next = Vec::new();
        for dir in cur {
            if !is_dir(&dir) || is_reparse(&dir) {
                continue;
            }
            if seg.contains('*') {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if wildcard_match_ci(seg, &name) {
                            let ep = e.path();
                            if is_dir_or_file(&ep) {
                                next.push(ep);
                            }
                        }
                    }
                }
            } else {
                let ep = dir.join(seg);
                if is_dir_or_file(&ep) {
                    next.push(ep);
                }
            }
        }
        cur = next;
        if cur.is_empty() {
            break;
        }
    }
    cur
}

fn global_items() -> Vec<CatItem> {
    let mut out = Vec::new();
    for e in &catalog().global {
        let expanded = expand_global_paths(&e.path);
        if expanded.is_empty() {
            continue;
        }
        let mut paths = Vec::new();
        for p in expanded {
            match e.older_than_days {
                Some(days) if is_dir(&p) => paths.extend(old_entries(&p, days)),
                _ => paths.push(p.display().to_string()),
            }
        }
        if !paths.is_empty() {
            out.push(CatItem { key: e.key.clone(), class: e.class.clone(), title: e.title.clone(), paths, lane: None });
        }
    }
    out
}

/// Top-level folders and loose regular files of `dir` not written for `days` days (never
/// following symlinks or reparse points).
fn old_entries(dir: &Path, days: u64) -> Vec<String> {
    use std::os::windows::fs::MetadataExt;
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(days * 86400);
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else { return false };
            if m.file_type().is_symlink() || (m.file_attributes() & disk_win::A_REPARSE != 0) {
                return false;
            }
            if !(m.is_dir() || m.is_file()) {
                return false;
            }
            m.modified().map(|t| t < cutoff).unwrap_or(false)
        })
        .map(|e| e.path().display().to_string())
        .collect()
}

// ---------------------------------------------------------------- git intelligence (F4.1b-d)

#[derive(Debug, Clone, Default)]
struct LaneMeta {
    main_repo: Option<String>,
    branch: Option<String>,
    merged: Option<bool>,
    dirty: Option<bool>,
    dirty_files: Option<u64>,
    unpushed_commits: Option<u64>,
    last_commit_age_days: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct LfsEstimate {
    reclaimable_bytes: Option<u64>,
    retained_bytes: Option<u64>,
    prunable_objects: Option<u64>,
    retained_objects: Option<u64>,
    verify_remote: bool,
    note: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ItemExtra {
    lane: Option<LaneMeta>,
    lfs: Option<LfsEstimate>,
    note: Option<String>,
}

fn parse_size_unit(val: f64, unit: &str) -> u64 {
    let mult = match unit.to_ascii_lowercase().as_str() {
        "b" | "bytes" | "byte" => 1.0,
        "kb" | "kib" | "k" => 1024.0,
        "mb" | "mib" | "m" => (1u64 << 20) as f64,
        "gb" | "gib" | "g" => (1u64 << 30) as f64,
        "tb" | "tib" | "t" => (1u64 << 40) as f64,
        _ => 1.0,
    };
    (val * mult).round().max(0.0) as u64
}

/// Parse `git lfs prune --dry-run --verbose` output for prunable/retained counts and bytes.
fn parse_lfs_prune_dry_run(text: &str) -> LfsEstimate {
    let mut est = LfsEstimate::default();
    let mut prunable_bytes_from_lines = 0u64;
    let mut prunable_lines = 0u64;
    let mut retained_lines = 0u64;
    let mut retained_bytes_from_lines = 0u64;

    for line in text.lines() {
        let l = line.trim();
        let lower = l.to_ascii_lowercase();
        // Summary line e.g. `prune: 5 local objects, 2 retained, pruning 3 files with 12 MB`
        // or `3 local objects, 1 retained, 2 files (4.2 MB) can be deleted`
        if lower.contains("local object") || lower.contains("retained") {
            let tokens: Vec<&str> = l.split_whitespace().collect();
            for (i, w) in tokens.iter().enumerate() {
                let lw = w.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_lowercase();
                if i > 0 {
                    if let Ok(n) = tokens[i - 1].trim_matches(|c: char| !c.is_ascii_digit()).parse::<u64>() {
                        if lw == "retained" {
                            est.retained_objects = Some(n);
                        } else if lw == "pruning" || lw == "prune" || (lw == "files" || lw == "file") {
                            est.prunable_objects = Some(est.prunable_objects.unwrap_or(n).max(n));
                        }
                    }
                }
                if lw == "pruning" && i + 2 < tokens.len() {
                    if let Ok(n) = tokens[i + 1].parse::<u64>() {
                        est.prunable_objects = Some(n);
                    }
                }
            }
            if let Some(open) = l.rfind('(') {
                if let Some(close) = l[open..].find(')') {
                    let inner = l[open + 1..open + close].trim();
                    let mut sp = inner.split_whitespace();
                    if let (Some(num), Some(unit)) = (sp.next(), sp.next()) {
                        if let Ok(v) = num.parse::<f64>() {
                            est.reclaimable_bytes = Some(parse_size_unit(v, unit));
                        }
                    }
                }
            }
        } else if let Some(rest) = l.strip_prefix("* ") {
            // Individual prunable object line: `* <oid> (<size> <unit>)`
            prunable_lines += 1;
            if let (Some(open), Some(close)) = (rest.rfind('('), rest.rfind(')')) {
                if close > open {
                    let inner = rest[open + 1..close].trim();
                    let mut sp = inner.split_whitespace();
                    if let (Some(num), Some(unit)) = (sp.next(), sp.next()) {
                        if let Ok(v) = num.parse::<f64>() {
                            prunable_bytes_from_lines += parse_size_unit(v, unit);
                        }
                    }
                }
            }
        } else if lower.starts_with("✔") || lower.starts_with("keep ") || lower.contains("retained:") {
            retained_lines += 1;
            if let (Some(open), Some(close)) = (l.rfind('('), l.rfind(')')) {
                if close > open {
                    let inner = l[open + 1..close].trim();
                    let mut sp = inner.split_whitespace();
                    if let (Some(num), Some(unit)) = (sp.next(), sp.next()) {
                        if let Ok(v) = num.parse::<f64>() {
                            retained_bytes_from_lines += parse_size_unit(v, unit);
                        }
                    }
                }
            }
        }
    }

    if est.prunable_objects.is_none() && (prunable_lines > 0 || text.contains("local object")) {
        est.prunable_objects = Some(prunable_lines);
    }
    if est.reclaimable_bytes.is_none() && (prunable_bytes_from_lines > 0 || est.prunable_objects == Some(0)) {
        est.reclaimable_bytes = Some(prunable_bytes_from_lines);
    }
    if est.retained_objects.is_none() && retained_lines > 0 {
        est.retained_objects = Some(retained_lines);
    }
    if est.retained_bytes.is_none() && retained_bytes_from_lines > 0 {
        est.retained_bytes = Some(retained_bytes_from_lines);
    }
    est
}

const GIT_SCAN_ENV_CLEARED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
];

fn run_git_scan(args: &[&str], timeout: Duration) -> Option<(i32, String)> {
    let mut env: Vec<(&str, Option<&str>)> = GIT_SCAN_ENV_CLEARED.iter().map(|k| (*k, None)).collect();
    env.push(("GIT_TERMINAL_PROMPT", Some("0")));
    env.push(("GCM_INTERACTIVE", Some("never")));
    match crate::disk_clean::run_cmd(Path::new("git"), args, timeout, &env) {
        Ok((code, out, _)) => Some((code, String::from_utf8_lossy(&out).into_owned())),
        Err(_) => None,
    }
}

fn estimate_lfs_for_repo(repo_or_gitdir: &Path, probe_deadline: Instant) -> LfsEstimate {
    let rem = probe_deadline.saturating_duration_since(Instant::now());
    if rem < Duration::from_millis(200) {
        return LfsEstimate {
            note: Some("git-lfs prune estimate skipped (discovery probe budget elapsed).".into()),
            ..Default::default()
        };
    }
    let rs = repo_or_gitdir.display().to_string();
    match run_git_scan(
        &["-C", &rs, "lfs", "prune", "--dry-run", "--verbose"],
        rem.min(Duration::from_secs(4)),
    ) {
        Some((0, out)) => parse_lfs_prune_dry_run(&out),
        _ => LfsEstimate {
            note: Some("git-lfs prune estimate unavailable (git-lfs missing or timed out).".into()),
            ..Default::default()
        },
    }
}

fn read_head_branch(gitdir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let line = head.lines().next()?.trim();
    if let Some(r) = line.strip_prefix("ref:") {
        let r = r.trim();
        return Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string());
    }
    (!line.is_empty()).then(|| line.chars().take(12).collect())
}

fn gather_lane_meta(wt_root: &Path, main_repo: Option<&Path>, probe_deadline: Instant) -> LaneMeta {
    let mut meta = LaneMeta {
        main_repo: main_repo.map(|p| p.display().to_string()),
        ..Default::default()
    };
    let dot_git = wt_root.join(".git");
    if let Some(origin) = resolve_worktree_links(wt_root) {
        meta.branch = read_head_branch(&origin.wt_gitdir);
        if meta.main_repo.is_none() {
            meta.main_repo = Some(origin.main_repo.display().to_string());
        }
    } else if dot_git.is_dir() {
        meta.branch = read_head_branch(&dot_git);
    } else {
        return meta;
    }

    let run_bounded = |args: &[&str], cap_ms: u64| -> Option<(i32, String)> {
        let rem = probe_deadline.saturating_duration_since(Instant::now());
        if rem < Duration::from_millis(200) {
            return None;
        }
        run_git_scan(args, rem.min(Duration::from_millis(cap_ms)))
    };

    let wt_s = wt_root.display().to_string();
    if meta.branch.is_none() {
        if let Some((0, out)) = run_bounded(&["-c", "core.fsmonitor=false", "-C", &wt_s, "rev-parse", "--abbrev-ref", "HEAD"], 2000) {
            let b = out.trim();
            if !b.is_empty() {
                meta.branch = Some(b.to_string());
            }
        }
    }
    if let Some((0, out)) = run_bounded(&["-c", "core.fsmonitor=false", "-C", &wt_s, "status", "--porcelain=v1"], 3000) {
        let count = out.lines().filter(|l| !l.trim().is_empty()).count() as u64;
        meta.dirty_files = Some(count);
        meta.dirty = Some(count > 0);
    }
    if let Some((0, out)) = run_bounded(&["-c", "core.fsmonitor=false", "-C", &wt_s, "log", "-1", "--format=%ct"], 2000) {
        if let Ok(ts) = out.trim().parse::<u64>() {
            meta.last_commit_age_days = Some(unix_now().saturating_sub(ts) / 86_400);
        }
    }
    if let Some((0, out)) = run_bounded(&["-c", "core.fsmonitor=false", "-C", &wt_s, "rev-list", "--count", "@{upstream}..HEAD"], 2000) {
        if let Ok(n) = out.trim().parse::<u64>() {
            meta.unpushed_commits = Some(n);
        }
    }
    let main_path: Option<PathBuf> = main_repo.map(Path::to_path_buf).or_else(|| meta.main_repo.as_deref().map(PathBuf::from));
    if let Some(main) = main_path {
        let main_s = main.display().to_string();
        let mut base_sha: Option<String> = None;
        for candidate in [
            "refs/remotes/origin/HEAD",
            "refs/heads/main",
            "refs/heads/master",
            "HEAD",
        ] {
            if let Some((0, out)) = run_bounded(
                &["-c", "core.fsmonitor=false", "-C", &main_s, "rev-parse", "--verify", candidate],
                1500,
            ) {
                let sha = out.trim();
                if !sha.is_empty() {
                    base_sha = Some(sha.to_string());
                    break;
                }
            }
        }
        if let Some(base_sha) = base_sha {
            let wt_has_head = run_bounded(
                &["-c", "core.fsmonitor=false", "-C", &wt_s, "rev-parse", "--verify", "HEAD"],
                1500,
            )
            .is_some_and(|(c, out)| c == 0 && !out.trim().is_empty());
            if wt_has_head {
                if let Some((code, _)) = run_bounded(
                    &["-c", "core.fsmonitor=false", "-C", &wt_s, "merge-base", "--is-ancestor", "HEAD", &base_sha],
                    2500,
                ) {
                    if code == 0 || code == 1 {
                        meta.merged = Some(code == 0);
                    }
                }
            }
        }
    }
    meta
}

fn collect_submodule_gitdirs(modules_dir: &Path, depth: u32, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(modules_dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !is_dir(&p) || is_reparse(&p) {
            continue;
        }
        if p.join("HEAD").is_file() && p.join("objects").is_dir() {
            let name = e.file_name().to_string_lossy().to_string();
            out.push((name, p));
        } else if depth > 0 {
            collect_submodule_gitdirs(&p, depth - 1, out);
        }
    }
}

fn git_repo_items(
    repo_label: &str,
    repo_root: &Path,
    common_git_dir: &Path,
    seen_paths: &mut Vec<String>,
    keep: &[String],
    probe_deadline: Instant,
) -> Vec<(CatItem, ItemExtra)> {
    if is_kept_path(common_git_dir, keep) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut push_unique = |item: CatItem, extra: ItemExtra| {
        if let Some(first) = item.paths.first() {
            let k = format!("{}|{}", item.key, norm(first));
            if seen_paths.contains(&k) {
                return;
            }
            seen_paths.push(k);
        }
        out.push((item, extra));
    };

    let mut git_dirs: Vec<(String, PathBuf, &Path)> = vec![(repo_label.to_string(), common_git_dir.to_path_buf(), repo_root)];
    let modules = common_git_dir.join("modules");
    if is_dir(&modules) && !is_kept_path(&modules, keep) {
        let mut subs = Vec::new();
        collect_submodule_gitdirs(&modules, 4, &mut subs);
        for (sub_name, sub_git) in subs {
            if !is_kept_path(&sub_git, keep) {
                git_dirs.push((format!("{} ({})", repo_label, sub_name), sub_git.clone(), repo_root));
            }
        }
    }

    for (label, gdir, est_root) in git_dirs {
        if is_kept_path(&gdir, keep) {
            continue;
        }
        // 1. git:lfs
        let lfs_dir = gdir.join(r"lfs\objects");
        if is_dir(&lfs_dir) && !is_reparse(&lfs_dir) && !is_kept_path(&lfs_dir, keep) {
            let target = if gdir == common_git_dir { est_root } else { gdir.as_path() };
            let est = estimate_lfs_for_repo(target, probe_deadline);
            push_unique(
                CatItem {
                    key: "git:lfs".into(),
                    class: "review".into(),
                    title: format!("Git LFS store for {}", label),
                    paths: vec![lfs_dir.display().to_string()],
                    lane: None,
                },
                ItemExtra { lfs: Some(est), ..Default::default() },
            );
        }

        // 2. git:tmp_packs (stale tmp_pack_* / tmp_idx_* / tmp_obj_* older than 1 hour)
        let pack_dir = gdir.join(r"objects\pack");
        if is_dir(&pack_dir) && !is_reparse(&pack_dir) {
            let stale_cut = std::time::SystemTime::now() - Duration::from_secs(3600);
            let mut stale_tmp = Vec::new();
            let mut pack_count = 0u64;
            let mut pack_bytes = 0u64;
            let mut has_midx = false;
            if let Ok(rd) = std::fs::read_dir(&pack_dir) {
                for ent in rd.flatten() {
                    let fp = ent.path();
                    let name = ent.file_name().to_string_lossy().to_string();
                    let lower = name.to_ascii_lowercase();
                    if lower == "multi-pack-index" || lower.ends_with(".midx") {
                        has_midx = true;
                    }
                    if !is_regular_file(&fp) {
                        continue;
                    }
                    if lower.starts_with("tmp_pack_") || lower.starts_with("tmp_idx_") || lower.starts_with("tmp_obj_") {
                        let old = std::fs::symlink_metadata(&fp)
                            .and_then(|m| m.modified())
                            .map(|t| t < stale_cut)
                            .unwrap_or(false);
                        if old {
                            stale_tmp.push(fp.display().to_string());
                        }
                    } else if lower.ends_with(".pack") {
                        pack_count += 1;
                        pack_bytes += std::fs::symlink_metadata(&fp).map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
            if !stale_tmp.is_empty() {
                let count = stale_tmp.len();
                let paths = if count > 32 { vec![pack_dir.display().to_string()] } else { stale_tmp };
                push_unique(
                    CatItem {
                        key: "git:tmp_packs".into(),
                        class: "review".into(),
                        title: format!("Stale Git temp packs for {}", label),
                        paths,
                        lane: None,
                    },
                    ItemExtra { note: Some(format!("{} stale temporary pack file(s) (>1h old); safe to clean with git prune --expire=1.hour.ago.", count)), ..Default::default() },
                );
            }
            if pack_count >= 12 || (pack_count >= 4 && pack_bytes >= (256 << 20) && !has_midx) {
                push_unique(
                    CatItem {
                        key: "git:packs".into(),
                        class: "review".into(),
                        title: format!("Git packfiles for {}", label),
                        paths: vec![pack_dir.display().to_string()],
                        lane: None,
                    },
                    ItemExtra { note: Some(format!("{} packfiles ({:.1} GB); git repack -ad consolidates packfiles.", pack_count, pack_bytes as f64 / ((1u64 << 30) as f64))), ..Default::default() },
                );
            }
        }

        // 3. git:loose_objects (>= 500 loose objects or >= 64 MiB in objects/[0-9a-f]{2})
        let obj_dir = gdir.join("objects");
        if is_dir(&obj_dir) && !is_reparse(&obj_dir) {
            let mut loose_count = 0u64;
            let mut loose_bytes = 0u64;
            if let Ok(rd) = std::fs::read_dir(&obj_dir) {
                'outer: for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().to_string();
                    if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                        let fan = ent.path();
                        if !is_dir(&fan) || is_reparse(&fan) {
                            continue;
                        }
                        if let Ok(sub) = std::fs::read_dir(&fan) {
                            for obj in sub.flatten() {
                                if let Ok(m) = std::fs::symlink_metadata(obj.path()) {
                                    if m.is_file() {
                                        loose_count += 1;
                                        loose_bytes += m.len();
                                        if loose_count >= 1000 && loose_bytes >= (64 << 20) {
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if loose_count >= 500 || loose_bytes >= (64 << 20) {
                push_unique(
                    CatItem {
                        key: "git:loose_objects".into(),
                        class: "review".into(),
                        title: format!("Git loose objects for {}", label),
                        paths: vec![obj_dir.display().to_string()],
                        lane: None,
                    },
                    ItemExtra { note: Some(format!("At least {} loose objects; run git maintenance to pack them.", loose_count)), ..Default::default() },
                );
            }
        }

        // 4. git:reflogs (>= 64 MiB in logs/)
        let logs_dir = gdir.join("logs");
        if is_dir(&logs_dir) && !is_reparse(&logs_dir) {
            let mut log_bytes = 0u64;
            let mut stack = vec![logs_dir.clone()];
            while let Some(d) = stack.pop() {
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for ent in rd.flatten() {
                        let p = ent.path();
                        if let Ok(m) = std::fs::symlink_metadata(&p) {
                            if m.is_dir() && !is_reparse(&p) {
                                stack.push(p);
                            } else if m.is_file() {
                                log_bytes += m.len();
                            }
                        }
                    }
                }
            }
            if log_bytes >= (64 << 20) {
                push_unique(
                    CatItem {
                        key: "git:reflogs".into(),
                        class: "review".into(),
                        title: format!("Git reflogs for {}", label),
                        paths: vec![logs_dir.display().to_string()],
                        lane: None,
                    },
                    ItemExtra { note: Some(format!("Reflogs hold {:.1} MB; git reflog expire trims old entries.", log_bytes as f64 / ((1u64 << 20) as f64))), ..Default::default() },
                );
            }
        }

        // 5. git:worktrees (stale worktree admin dirs without `locked` whose target doesn't exist)
        let wt_dir = gdir.join("worktrees");
        if is_dir(&wt_dir) && !is_reparse(&wt_dir) {
            let mut stale_wt = false;
            if let Ok(rd) = std::fs::read_dir(&wt_dir) {
                for ent in rd.flatten() {
                    let admin = ent.path();
                    if !is_dir(&admin) || admin.join("locked").exists() {
                        continue;
                    }
                    let missing = std::fs::read_to_string(admin.join("gitdir"))
                        .ok()
                        .and_then(|t| resolve_gitdir_target(&admin, &t))
                        .is_none_or(|target| !is_dir(&target));
                    if missing {
                        stale_wt = true;
                        break;
                    }
                }
            }
            if stale_wt {
                push_unique(
                    CatItem {
                        key: "git:worktrees".into(),
                        class: "review".into(),
                        title: format!("Stale Git worktrees for {}", label),
                        paths: vec![wt_dir.display().to_string()],
                        lane: None,
                    },
                    ItemExtra { note: Some("Contains stale worktree metadata for deleted checkouts; run git worktree prune.".into()), ..Default::default() },
                );
            }
        }
    }

    out
}

/// Caches left behind by `memguard disk relocate`, once the new place has been written to.
fn old_cache_items() -> Vec<CatItem> {
    let Some(rec) = crate::disk_clean::DiskFiles::default().relocate_record() else { return Vec::new() };
    crate::disk_relocate::old_caches(&rec)
        .into_iter()
        .map(|(name, old)| CatItem { key: "cache:old".into(), class: "disposable".into(), title: format!("Old cache, no longer used ({})", name), paths: vec![old], lane: None })
        .collect()
}

/// Ather MemGuard's own bin on each drive: its own item, never part of another.
fn bin_items_with_volumes(vols: &[disk_win::VolumeFacts]) -> Vec<CatItem> {
    let mut out = Vec::new();
    for v in vols.iter().filter(|v| v.drive_type == 3) {
        for root in v.paths.iter().filter(|p| p.len() <= 3) {
            let b = crate::disk_clean::bin_root(root);
            if std::fs::read_dir(&b).map(|mut d| d.next().is_some()).unwrap_or(false) {
                out.push(CatItem { key: "memguard:bin".into(), class: "review".into(), title: format!("Ather MemGuard's bin on {}", root.trim_end_matches('\\')), paths: vec![b.display().to_string()], lane: None });
            }
        }
    }
    out
}

/// Everything the checkup lists for `path` (or, without one, the configured project folders,
/// the projects on Dev Drives and the global caches), sorted by class.
pub fn discover(cfg: &Config, path: Option<&str>) -> Vec<CatItem> {
    discover_detailed(cfg, path).0
}

fn discover_detailed(cfg: &Config, path: Option<&str>) -> (Vec<CatItem>, Vec<ItemExtra>) {
    let s = cfg.disk();
    let keep: Vec<String> = s.keep.iter().map(|k| norm(k)).filter(|k| !k.is_empty()).collect();
    let probe_deadline = Instant::now() + Duration::from_secs(s.scan_budget_sec.clamp(2, 10));
    let mut items: Vec<(CatItem, ItemExtra)> = Vec::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    let cached_vols = path.is_none().then(disk_win::volumes);
    match path {
        Some(p) => {
            let pb = PathBuf::from(p);
            if !is_kept_path(&pb, &keep) {
                roots.push(pb);
            }
        }
        None => {
            for p in &s.project_dirs {
                let pb = PathBuf::from(p);
                if !is_kept_path(&pb, &keep) {
                    roots.push(pb);
                }
            }
            if let Some(vols) = &cached_vols {
                for v in vols.iter().filter(|v| v.dev_volume == Some(true)) {
                    if let Some(r) = v.paths.first() {
                        let pb = PathBuf::from(r);
                        if !is_kept_path(&pb, &keep) {
                            roots.push(pb);
                        }
                    }
                }
            }
        }
    }
    let mut projects: Vec<DiscoveredRoot> = Vec::new();
    let mut proj_map: HashMap<String, usize> = HashMap::new();
    let push_root = |dr: DiscoveredRoot, projects: &mut Vec<DiscoveredRoot>, proj_map: &mut HashMap<String, usize>| {
        if is_kept_path(&dr.path, &keep) {
            return false;
        }
        let k = norm(&dr.path.display().to_string());
        if let Some(&idx) = proj_map.get(&k) {
            projects[idx].has_uproject |= dr.has_uproject;
            projects[idx].is_engine |= dr.is_engine;
            projects[idx].has_git |= dr.has_git;
            projects[idx].is_bare_git |= dr.is_bare_git;
            false
        } else {
            proj_map.insert(k, projects.len());
            projects.push(dr);
            true
        }
    };
    for r in &roots {
        for dr in find_roots_with_keep(r, s.scan_depth, &keep) {
            push_root(dr, &mut projects, &mut proj_map);
        }
    }
    let mut lanes_seen: Vec<String> = Vec::new();
    let mut git_seen: Vec<String> = Vec::new();
    let mut idx = 0usize;
    while idx < projects.len() {
        let dr = projects[idx].clone();
        idx += 1;
        let root = &dr.path;
        if is_kept_path(root, &keep) {
            continue;
        }
        let name = if dr.is_engine {
            root.parent()
                .and_then(|p| p.file_name())
                .map(|p| format!("{}\\Engine", p.to_string_lossy()))
                .unwrap_or_else(|| "Engine".into())
        } else {
            root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        };

        if is_worktree(root) {
            // A lane asked for (or found) by itself: the lane, its own items, and reverse-discovered main repo.
            let lane = root.display().to_string();
            lanes_seen.push(norm(&lane));
            let origin = resolve_worktree_links(root);
            let lane_meta = gather_lane_meta(root, origin.as_ref().map(|o| o.main_repo.as_path()), probe_deadline);
            items.push((
                CatItem { key: "lane".into(), class: "review".into(), title: lane_title(root), paths: vec![lane.clone()], lane: None },
                ItemExtra { lane: Some(lane_meta), ..Default::default() },
            ));
            for it in project_items(root, &name, Some(&lane)) {
                items.push((it, ItemExtra::default()));
            }
            if let Some(orig) = origin {
                if !is_kept_path(&orig.main_repo, &keep) && is_dir(&orig.main_repo) {
                    push_root(
                        DiscoveredRoot {
                            path: orig.main_repo,
                            has_uproject: false,
                            is_engine: false,
                            has_git: !orig.is_bare,
                            is_bare_git: orig.is_bare,
                        },
                        &mut projects,
                        &mut proj_map,
                    );
                }
            }
        } else {
            for it in project_items(root, &name, None) {
                items.push((it, ItemExtra::default()));
            }
            let common_git = if dr.is_bare_git {
                Some(root.clone())
            } else if root.join(".git").is_dir() {
                Some(root.join(".git"))
            } else {
                None
            };
            if let Some(ref gdir) = common_git {
                items.extend(git_repo_items(&name, root, gdir, &mut git_seen, &keep, probe_deadline));
            }
            // Lanes of this project: git worktrees and same-named `.uproject` siblings.
            let up = uprojects(root);
            let mut lanes: Vec<PathBuf> = common_git.as_deref().map(worktrees_of_gitdir).unwrap_or_default();
            if !up.is_empty() {
                let mut dirs: Vec<PathBuf> = root.parent().map(|p| vec![p.to_path_buf()]).unwrap_or_default();
                dirs.extend(s.lane_dirs.iter().map(PathBuf::from));
                for d in dirs {
                    if is_kept_path(&d, &keep) {
                        continue;
                    }
                    if let Ok(rd) = std::fs::read_dir(&d) {
                        for e in rd.flatten() {
                            let p = e.path();
                            if !is_kept_path(&p, &keep) && is_dir(&p) && uprojects(&p).iter().any(|u| up.iter().any(|x| x.eq_ignore_ascii_case(u))) {
                                lanes.push(p);
                            }
                        }
                    }
                }
            }
            for l in lanes {
                if is_kept_path(&l, &keep) {
                    continue;
                }
                let n = norm(&l.display().to_string());
                if n == norm(&root.display().to_string()) || lanes_seen.contains(&n) || proj_map.contains_key(&n) {
                    continue;
                }
                lanes_seen.push(n);
                let lane_meta = gather_lane_meta(&l, Some(root), probe_deadline);
                items.push((
                    CatItem { key: "lane".into(), class: "review".into(), title: lane_title(&l), paths: vec![l.display().to_string()], lane: None },
                    ItemExtra { lane: Some(lane_meta), ..Default::default() },
                ));
            }
        }
    }
    if let Some(vols) = &cached_vols {
        for it in global_items() {
            items.push((it, ItemExtra::default()));
        }
        for it in old_cache_items() {
            items.push((it, ItemExtra::default()));
        }
        for it in bin_items_with_volumes(vols) {
            items.push((it, ItemExtra::default()));
        }
    }
    for (it, _) in &mut items {
        it.paths.retain(|p| !keep.iter().any(|k| disk_space::is_within(&norm(p), k)));
    }
    let mut seen_items: std::collections::HashSet<(String, Vec<String>)> = std::collections::HashSet::new();
    items.retain(|(i, _)| {
        if i.paths.is_empty() {
            return false;
        }
        let mut np: Vec<String> = i.paths.iter().map(|p| norm(p)).collect();
        np.sort();
        seen_items.insert((i.key.clone(), np))
    });
    items.sort_by_key(|(i, _)| class_rank(&i.class));
    items.into_iter().unzip()
}

// ---------------------------------------------------------------- the scan job (child process)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanJob {
    pub items: Vec<Vec<String>>,
    /// Sets of item indexes to compute `frees_together` for.
    pub sets: Vec<Vec<usize>>,
    pub threads: usize,
    pub budget_sec: u64,
    /// Stop (keeping what was counted) if a build or cook starts during the scan.
    pub stop_on_build: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemResult {
    pub volume: Option<String>,
    /// Why the item couldn't be measured (network path, reparse-point root, cloud sync root, missing).
    pub unmeasurable: Option<String>,
    pub root_ids: Vec<u128>,
    pub level1: WalkStats,
    pub level2: Option<WalkStats>,
    pub footprint: u64,
    pub frees_alone: u64,
    pub shared_outside: u64,
    pub shared_with: Vec<(usize, u64)>,
    pub anomalies: bool,
    #[serde(default)]
    pub inconsistent: bool,
    #[serde(default)]
    pub unknown_rc: bool,
    pub dropped_paths: Vec<String>,
    /// The time ran out before the walk got to it: nothing about it is known, not even its size.
    #[serde(default)]
    pub not_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetResult {
    /// Index into the job's `sets`.
    pub set: usize,
    pub items: Vec<usize>,
    pub volume: String,
    pub frees_together: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupResult {
    pub items: Vec<usize>,
    pub shared: u64,
    pub frees_if_all_removed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeResult {
    pub guid: String,
    pub root: String,
    pub fs: String,
    pub block_cloning: bool,
    pub total: u64,
    pub avail: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolumeScanStats {
    pub guid: String,
    pub cluster: u64,
    pub total_files: u64,
    pub total_logical: u64,
    pub total_allocated: u64,
    pub union_bytes: u64,
    pub sum_l2_allocated: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanOutput {
    pub took_sec: f64,
    pub partial: bool,
    pub stopped_for_build: bool,
    pub started_ft: u64,
    pub items: Vec<ItemResult>,
    pub sets: Vec<SetResult>,
    pub groups: Vec<GroupResult>,
    pub volumes: Vec<VolumeResult>,
    #[serde(default)]
    pub volume_stats: Vec<VolumeScanStats>,
}

/// Minimum shared cluster count equivalent to 1 GiB for `cluster_bytes`.
pub(crate) fn min_sharing_clusters(cluster_bytes: u64) -> u64 {
    (1u64 << 30).div_ceil(cluster_bytes.max(1))
}

/// Progress for the caller: (files counted, seconds since the start).
pub type Progress<'a> = &'a mut dyn FnMut(u64, f64);

fn unmeasurable_reason(path: &str) -> Option<String> {
    use std::os::windows::fs::MetadataExt;
    if path.starts_with(r"\\") && !path.starts_with(r"\\?\") {
        return Some("network path".into());
    }
    let Ok(m) = std::fs::symlink_metadata(path) else { return Some("not found".into()) };
    let a = m.file_attributes();
    if a & disk_win::A_REPARSE != 0 {
        return Some("the folder is a link (reparse point) to somewhere else".into());
    }
    if a & (disk_win::A_RECALL_OPEN | disk_win::A_RECALL_DATA | disk_win::A_OFFLINE) != 0 {
        return Some("inside a cloud-synced folder".into());
    }
    for var in ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"] {
        if let Ok(od) = std::env::var(var) {
            if !od.is_empty() && disk_space::is_within(&norm(path), &norm(&od)) {
                return Some("inside a cloud-synced folder".into());
            }
        }
    }
    None
}

/// Has a build or cook started? (Process names only; cheap.)
fn build_running() -> bool {
    let st = crate::engine::analyze(&Config::builtin(), &crate::engine::snapshot());
    st.activities.build || st.activities.cook
}

struct CoordinatorGuard<'a>(&'a AtomicUsize, &'a (Mutex<()>, Condvar));

impl Drop for CoordinatorGuard<'_> {
    fn drop(&mut self) {
        if self.0.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _g = self.1 .0.lock();
            self.1 .1.notify_all();
        }
    }
}

/// Run a scan job in this process. The child process calls this after lowering its priority.
pub fn run_job(job: &ScanJob, progress: Progress) -> ScanOutput {
    let t0 = Instant::now();
    let started_ft = crate::win::now_filetime();
    let deadline = t0 + Duration::from_secs(job.budget_sec.max(1));
    let stop = Arc::new(AtomicBool::new(false));
    let stopped_for_build = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let files = Arc::new(AtomicU64::new(0));
    let watcher = job.stop_on_build.then(|| {
        let (stop, sfb, done) = (stop.clone(), stopped_for_build.clone(), done.clone());
        std::thread::spawn(move || {
            let at_start = build_running();
            while !done.load(Ordering::Relaxed) {
                for _ in 0..50 {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !at_start && build_running() {
                    sfb.store(true, Ordering::SeqCst);
                    stop.store(true, Ordering::SeqCst);
                    return;
                }
            }
        })
    });

    let n = job.items.len();
    let mut out = ScanOutput { started_ft, items: vec![ItemResult::default(); n], ..Default::default() };
    // Resolve every root to its volume; refuse what can't be measured.
    let mut roots: Vec<Vec<String>> = vec![Vec::new(); n];
    for (i, paths) in job.items.iter().enumerate() {
        let r = &mut out.items[i];
        for p in paths {
            if let Some(why) = unmeasurable_reason(p) {
                if r.unmeasurable.is_none() && paths.len() == 1 {
                    r.unmeasurable = Some(why);
                } else {
                    r.dropped_paths.push(p.clone());
                }
                continue;
            }
            let Some((_, guid)) = disk_win::volume_of(p) else {
                r.dropped_paths.push(p.clone());
                continue;
            };
            match &r.volume {
                None => r.volume = Some(guid),
                Some(g) if !g.eq_ignore_ascii_case(&guid) => {
                    r.dropped_paths.push(p.clone());
                    continue;
                }
                _ => {}
            }
            if let Some((_, fid, _)) = disk_win::identity(p) {
                r.root_ids.push(fid);
            }
            roots[i].push(p.clone());
        }
        if roots[i].is_empty() && r.unmeasurable.is_none() {
            r.unmeasurable = Some("not found".into());
        }
    }
    // Deepest-item ownership: each root skips the other items' roots inside it.
    let flat: Vec<(usize, String)> = roots.iter().enumerate().flat_map(|(i, v)| v.iter().map(move |p| (i, p.clone()))).collect();
    let (nested, _dups) = disk_space::nested_roots(&flat);
    let mut skips: Vec<Vec<String>> = vec![Vec::new(); n];
    for (i, inner) in nested {
        skips[i] = inner;
    }

    // Group by volume.
    let mut by_vol: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, r) in out.items.iter().enumerate() {
        if let (Some(g), true) = (&r.volume, !roots[i].is_empty()) {
            by_vol.entry(g.clone()).or_default().push(i);
        }
    }
    let mut last_report = Instant::now();
    let mut tick = |files: &AtomicU64, progress: &mut dyn FnMut(u64, f64)| {
        if last_report.elapsed() >= Duration::from_secs(2) {
            last_report = Instant::now();
            progress(files.load(Ordering::Relaxed), t0.elapsed().as_secs_f64());
        }
    };

    struct VolTask {
        vol_idx: usize,
        guid: String,
        ids: Vec<usize>,
        cfg: disk_win::VolumeWalkConfig,
    }
    struct VolDone {
        vol_idx: usize,
        ids: Vec<usize>,
        walk_results: Vec<disk_win::WalkItemResult>,
        sw: disk_space::Sweep,
        cluster: u64,
        sets: Vec<SetResult>,
        groups: Vec<GroupResult>,
    }

    let all_vols = disk_win::volumes();
    let extent_deadline = if job.budget_sec >= 4 {
        Some(t0 + Duration::from_secs((job.budget_sec * 3 / 4).max(1)))
    } else {
        None
    };

    let mut vol_tasks: Vec<VolTask> = Vec::new();
    let mut disk_groups: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for g in by_vol.keys() {
        let facts = all_vols
            .iter()
            .find(|v| v.guid.eq_ignore_ascii_case(g))
            .cloned()
            .unwrap_or_else(|| disk_win::volume_facts(g, Vec::new()));
        if facts.cluster == 0 {
            for &i in &by_vol[g] {
                if out.items[i].unmeasurable.is_none() {
                    out.items[i].unmeasurable = Some("couldn't read the volume's cluster size".into());
                }
            }
            continue;
        }
        let block_cloning = facts.fs_flags & disk_win::FS_BLOCK_REFCOUNTING != 0;
        let supports_hard_links = facts.fs_flags & disk_win::FS_HARD_LINKS != 0;
        let threads = if facts.seek_penalty == Some(true) {
            job.threads.clamp(1, 2)
        } else {
            job.threads.clamp(1, 64)
        };
        out.volumes.push(VolumeResult {
            guid: g.clone(),
            root: facts.paths.first().cloned().unwrap_or_else(|| g.clone()),
            fs: facts.fs.clone(),
            block_cloning,
            total: facts.total,
            avail: facts.avail,
            free: facts.free,
        });
        let vol_idx = vol_tasks.len();
        let disk_key = disk_win::volume_disk_number(g)
            .map(|d| d as u64)
            .unwrap_or_else(|| (1u64 << 32) | (vol_idx as u64));
        let cfg = disk_win::VolumeWalkConfig {
            cluster: facts.cluster,
            block_cloning,
            supports_hard_links,
            level2: true,
            threads,
            deadline,
            extent_deadline: block_cloning.then_some(extent_deadline).flatten(),
            stop: stop.clone(),
            started_ft,
            progress_files: files.clone(),
        };
        vol_tasks.push(VolTask { vol_idx, guid: g.clone(), ids: by_vol[g].clone(), cfg });
        disk_groups.entry(disk_key).or_default().push(vol_idx);
    }

    let mut all_done: Vec<VolDone> = if !vol_tasks.is_empty() {
        let remaining = AtomicUsize::new(disk_groups.len());
        let done_cv = (Mutex::new(()), Condvar::new());
        let roots_ref = &roots;
        let skips_ref = &skips;
        let tasks_ref = &vol_tasks;
        let sets_ref = &job.sets;
        let rem_ref = &remaining;
        let cv_ref = &done_cv;
        std::thread::scope(|s| {
            let handles: Vec<_> = disk_groups
                .into_values()
                .map(|task_indices| {
                    s.spawn(move || {
                        let _guard = CoordinatorGuard(rem_ref, cv_ref);
                        let mut res_list = Vec::with_capacity(task_indices.len());
                        for ti in task_indices {
                            let vt = &tasks_ref[ti];
                            let inputs: Vec<disk_win::WalkItemInput<'_>> = vt
                                .ids
                                .iter()
                                .map(|&gi| disk_win::WalkItemInput { roots: &roots_ref[gi], skip: &skips_ref[gi] })
                                .collect();
                            let (walk_results, linked) = disk_win::walk_volume(&vt.cfg, &inputs);
                            let local_exts: Vec<disk_space::ItemExtents> =
                                walk_results.iter().map(|r| r.extents.clone()).collect();
                            let sw = disk_space::sweep_with_links(&local_exts, &linked);
                            let c = vt.cfg.cluster;
                            let mut sets_out = Vec::new();
                            for (si, set) in sets_ref.iter().enumerate() {
                                let local_set: Vec<usize> =
                                    set.iter().filter_map(|i| vt.ids.iter().position(|x| x == i)).collect();
                                if !local_set.is_empty() {
                                    let sized_only: u64 = if vt.cfg.block_cloning {
                                        0
                                    } else {
                                        local_set
                                            .iter()
                                            .map(|&lk| &walk_results[lk])
                                            .filter(|wr| wr.level2.is_none() && !wr.not_reached)
                                            .map(|wr| wr.level1.allocated)
                                            .sum()
                                    };
                                    sets_out.push(SetResult {
                                        set: si,
                                        items: local_set.iter().map(|k| vt.ids[*k]).collect(),
                                        volume: vt.guid.clone(),
                                        frees_together: sw.frees_together(&local_set) * c + sized_only,
                                    });
                                }
                            }
                            let groups_out = sw
                                .sharing_groups(min_sharing_clusters(c))
                                .into_iter()
                                .take(12)
                                .map(|grp| GroupResult {
                                    items: grp.items.iter().map(|k| vt.ids[*k]).collect(),
                                    shared: grp.shared * c,
                                    frees_if_all_removed: grp.frees_if_all_removed * c,
                                })
                                .collect();
                            res_list.push(VolDone {
                                vol_idx: vt.vol_idx,
                                ids: vt.ids.clone(),
                                walk_results,
                                sw,
                                cluster: c,
                                sets: sets_out,
                                groups: groups_out,
                            });
                        }
                        res_list
                    })
                })
                .collect();

            loop {
                if rem_ref.load(Ordering::Acquire) == 0 {
                    break;
                }
                let g = cv_ref.0.lock().unwrap_or_else(|p| p.into_inner());
                if rem_ref.load(Ordering::Acquire) == 0 {
                    break;
                }
                let _ = cv_ref.1.wait_timeout(g, Duration::from_secs(2)).unwrap_or_else(|p| p.into_inner());
                tick(&files, progress);
            }

            let mut collected = Vec::with_capacity(vol_tasks.len());
            for h in handles {
                collected.extend(h.join().unwrap());
            }
            collected
        })
    } else {
        Vec::new()
    };

    all_done.sort_by_key(|vd| vd.vol_idx);
    for vd in all_done {
        let c = vd.cluster;
        let guid = out.volumes.get(vd.vol_idx).map(|v| v.guid.clone()).unwrap_or_default();
        let mut total_files = 0u64;
        let mut total_logical = 0u64;
        let mut total_allocated = 0u64;
        let mut sum_l2_allocated = 0u64;
        for (k, (&i, wr)) in vd.ids.iter().zip(vd.walk_results).enumerate() {
            let sp = &vd.sw.items[k];
            let r = &mut out.items[i];
            r.not_reached = wr.not_reached;
            r.level1 = wr.level1;
            r.level2 = wr.level2;
            r.footprint = sp.footprint * c;
            r.frees_alone = sp.frees_alone * c;
            r.shared_outside = sp.shared_outside * c;
            r.shared_with = sp.shared_with.iter().map(|(j, v)| (vd.ids[*j], v * c)).collect();
            r.anomalies = sp.anomalies > 0;
            r.inconsistent = sp.inconsistent > 0;
            r.unknown_rc = sp.unknown_rc > 0;
            let st = r.level2.as_ref().unwrap_or(&r.level1);
            total_files += st.files;
            total_logical += st.logical;
            total_allocated += st.allocated;
            if let Some(l2) = &r.level2 {
                if !l2.cut_short {
                    sum_l2_allocated += l2.allocated;
                }
            }
        }
        out.volume_stats.push(VolumeScanStats {
            guid,
            cluster: c,
            total_files,
            total_logical,
            total_allocated,
            union_bytes: vd.sw.union * c,
            sum_l2_allocated,
        });
        out.sets.extend(vd.sets);
        out.groups.extend(vd.groups);
    }
    done.store(true, Ordering::SeqCst);
    if let Some(w) = watcher {
        let _ = w.join();
    }
    out.stopped_for_build = stopped_for_build.load(Ordering::SeqCst);
    out.partial = out.items.iter().any(|r| r.unmeasurable.is_none() && r.level2.as_ref().map_or(true, |l| l.cut_short) || r.level1.cut_short);
    out.took_sec = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    progress(files.load(Ordering::Relaxed), t0.elapsed().as_secs_f64());
    out
}

/// `memguard disk scan-child`: read a job on stdin, scan at background priority, write the
/// result on stdout, progress lines (`progress <files> <seconds>`) on stderr.
pub fn child_main() -> i32 {
    use std::io::{Read, Write};
    disk_win::enter_background_mode();
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 1;
    }
    let Ok(job) = serde_json::from_str::<ScanJob>(&input) else {
        eprintln!("memguard disk scan-child: the job is not valid JSON");
        return 1;
    };
    let out = run_job(&job, &mut |files, secs| eprintln!("progress {} {:.1}", files, secs));
    let mut so = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut so, &out);
    let _ = so.write_all(b"\n");
    0
}

/// The memguard.exe that runs the scan child: this exe if it is memguard.exe, else the one
/// next to it (the tray app, test binaries one folder down), else the installed copy.
pub fn cli_exe() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    if me.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(crate::paths::CLI_EXE)) {
        return Some(me);
    }
    let dir = me.parent()?;
    [dir.join(crate::paths::CLI_EXE), dir.parent().map(|p| p.join(crate::paths::CLI_EXE)).unwrap_or_default(), crate::paths::install_dir().join(crate::paths::CLI_EXE)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Run a job in the low-priority child process and wait for it.
pub fn run_child(job: &ScanJob, progress: Progress) -> Result<ScanOutput, String> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = cli_exe().ok_or("memguard.exe was not found, so the disk could not be counted")?;
    let mut child = Command::new(&exe)
        .args(["disk", "scan-child"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start the disk counter: {}", e))?;
    let input = serde_json::to_vec(job).map_err(|e| e.to_string())?;
    child.stdin.take().ok_or("no stdin")?.write_all(&input).map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let stderr = child.stderr.take().ok_or("no stderr")?;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        let mut it = line.split_whitespace();
        if it.next() == Some("progress") {
            if let (Some(f), Some(s)) = (it.next().and_then(|v| v.parse().ok()), it.next().and_then(|v| v.parse().ok())) {
                progress(f, s);
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let text = reader.join().map_err(|_| "the disk counter's output was lost".to_string())?;
    if !status.success() {
        return Err(format!("the disk counter stopped with exit code {}", status.code().unwrap_or(-1)));
    }
    serde_json::from_str(text.trim()).map_err(|e| format!("the disk counter's result could not be read: {}", e))
}

/// How a report gets its scan: the child process in the product, in-process in tests.
pub type Runner<'a> = &'a dyn Fn(&ScanJob, Progress) -> Result<ScanOutput, String>;

// ---------------------------------------------------------------- reports

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkup {
    pub scan: Value,
    pub volumes: Vec<Value>,
    pub items: Vec<Value>,
    pub sharing_groups: Vec<Value>,
    pub totals: Vec<Value>,
    pub basis: String,
    pub do_not_sum: String,
}

/// What the cache keeps: the report, and what `plan` needs to find the items again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCheckup {
    /// Names this checkup: a clean confirmed on it carries the id, so ids like `d3` can't
    /// silently mean another item after a newer checkup replaced this one.
    #[serde(default)]
    pub id: String,
    pub scope: String,
    pub unix: u64,
    pub time: String,
    pub items: Vec<CatItem>,
    pub root_ids: Vec<Vec<u128>>,
    pub last_writes: Vec<u64>,
    pub report: Checkup,
}

impl CachedCheckup {
    /// The checkup's id (`c<run id>`; older saved checkups: from their time).
    pub fn ident(&self) -> String {
        if self.id.is_empty() {
            format!("u{}", self.unix)
        } else {
            self.id.clone()
        }
    }

    /// The index of item `d<n>`.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        id.trim().strip_prefix('d').and_then(|n| n.parse::<usize>().ok()).filter(|n| *n >= 1 && *n <= self.items.len()).map(|n| n - 1)
    }
}

static CACHE_WRITES: AtomicBool = AtomicBool::new(false);

/// Let this process write the disk caches in the data folder (the CLI, the MCP server and
/// the tray turn this on; library tests never do).
pub fn allow_cache_writes() {
    CACHE_WRITES.store(true, Ordering::SeqCst);
}

pub fn cache_writes_allowed() -> bool {
    CACHE_WRITES.load(Ordering::SeqCst) && !cfg!(test)
}

fn cache_file() -> PathBuf {
    crate::paths::data_dir().join("disk-checkup.json")
}
fn caches_file() -> PathBuf {
    crate::paths::data_dir().join("disk-caches.json")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn load_cache() -> Option<CachedCheckup> {
    std::fs::read_to_string(cache_file()).ok().and_then(|t| serde_json::from_str(&t).ok())
}

fn save_cache(c: &CachedCheckup) {
    if cache_writes_allowed() {
        if let Ok(bytes) = serde_json::to_vec(c) {
            let _ = crate::disk_clean::write_atomic(&cache_file(), &bytes);
        }
    }
}

/// The caches on the system drive, as the last full checkup sized them (`dev:caches_on_c`).
pub fn cached_system_caches() -> Option<crate::disk::CachesOnSystem> {
    std::fs::read_to_string(caches_file()).ok().and_then(|t| serde_json::from_str(&t).ok())
}

/// The volumes UE processes are working in: project folders named on their command lines.
fn busy_projects() -> Vec<String> {
    busy_state().0
}

/// Project folders an Unreal editor, build or cook is working in (normalised), and, when
/// some Unreal work can't be tied to a project, the words that say why every cleanup waits.
pub fn busy_state() -> (Vec<String>, Option<String>) {
    let mut out = Vec::new();
    let mut unknown: Option<&'static str> = None;
    // Who this is, read once and only when some command line can't be read.
    let mut me: Option<(Option<Vec<u8>>, Option<u32>)> = None;
    for p in crate::win::list_processes() {
        if !unreal_candidate(&p.name) {
            continue;
        }
        let u = unreal_use(&p.name, crate::win::command_line(p.pid).as_deref());
        let own = if u == UnrealUse::Unreadable {
            let (sid, session) = me.get_or_insert_with(|| (crate::doctor_win::current_user_sid(), crate::doctor_win::current_session()));
            crate::doctor_win::ownership(p.pid, sid.as_deref(), *session)
        } else {
            crate::doctor_win::Ownership::default()
        };
        match u {
            UnrealUse::Projects(dirs) => out.extend(dirs),
            other => {
                if let Some(w) = unknown_words(&p.name, &other, own) {
                    // "Unreal is running" (known) wins over "can't read".
                    if unknown.is_none() || w == UNREAL_UNKNOWN {
                        unknown = Some(w);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    (out, unknown.map(str::to_string))
}

/// Unreal work whose command line names no project.
pub const UNREAL_UNKNOWN: &str = "Unreal is running and Ather MemGuard can't tell which project it works in, so it waits.";
/// A build tool (dotnet, UnrealBuildTool, a cook) whose command line can't be read.
pub const BUILD_TOOL_UNREADABLE: &str = "A build tool is running and Ather MemGuard can't read which project it works in, so it waits.";
/// An Unreal editor whose command line can't be read.
pub const EDITOR_UNREADABLE: &str = "An Unreal editor is running and Ather MemGuard can't read which project it works in, so it waits.";

/// Why one process makes every cleanup wait, or `None` when it doesn't. A process whose
/// command line can't be read counts only when it runs in this person's session and as this
/// person (`own`); anything else (a SYSTEM service, another user's build) isn't their Unreal
/// work. An owner or session that can't be read counts as not theirs.
pub fn unknown_words(name: &str, u: &UnrealUse, own: crate::doctor_win::Ownership) -> Option<&'static str> {
    match u {
        UnrealUse::NotUnreal | UnrealUse::Projects(_) => None,
        UnrealUse::Unknown => Some(UNREAL_UNKNOWN),
        UnrealUse::Unreadable if !(own.owned && own.same_session) => None,
        UnrealUse::Unreadable => Some(if name.to_ascii_lowercase().starts_with("unrealeditor") { EDITOR_UNREADABLE } else { BUILD_TOOL_UNREADABLE }),
    }
}

/// What one process means for cleanups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnrealUse {
    NotUnreal,
    /// Unreal work in these project folders (normalised).
    Projects(Vec<String>),
    /// Unreal work whose command line names no project: every cleanup waits ("busy, unknown
    /// project").
    Unknown,
    /// Could be Unreal work, but its command line couldn't be read: every cleanup waits only
    /// when it is this person's own process (`unknown_words`).
    Unreadable,
}

/// Could this process name be Unreal work (an editor, a build, a cook, a shader compile, a
/// pak), or `dotnet` running UnrealBuildTool or AutomationTool? `.exe` is optional.
pub fn unreal_candidate(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let n = n.strip_suffix(".exe").unwrap_or(&n);
    n.starts_with("unrealeditor")
        || n.starts_with("unrealbuildtool")
        || n.starts_with("automationtool")
        || n.starts_with("shadercompileworker")
        || n.starts_with("unrealpak")
        || n == "dotnet"
}

/// What a process with this name and command line (`None`: couldn't be read) is doing.
/// Builds started from a command line run as `dotnet UnrealBuildTool.dll ...` or
/// `dotnet AutomationTool.dll ...`; any `dotnet` that runs neither is not Unreal work.
pub fn unreal_use(name: &str, cmd: Option<&str>) -> UnrealUse {
    if !unreal_candidate(name) {
        return UnrealUse::NotUnreal;
    }
    let n = name.to_ascii_lowercase();
    let dotnet = n.strip_suffix(".exe").unwrap_or(&n) == "dotnet";
    let Some(cmd) = cmd else { return UnrealUse::Unreadable };
    if dotnet {
        let l = cmd.to_ascii_lowercase();
        if !(l.contains("unrealbuildtool.dll") || l.contains("automationtool.dll")) {
            return UnrealUse::NotUnreal;
        }
    }
    let mut dirs = uproject_dirs_in(cmd);
    dirs.extend(project_args_in(cmd));
    dirs.sort();
    dirs.dedup();
    if dirs.is_empty() {
        UnrealUse::Unknown
    } else {
        UnrealUse::Projects(dirs)
    }
}

/// Project folders named by `-project=` or `-ScriptsForProject=` (a `.uproject` or its folder),
/// quoted or not (normalised).
pub fn project_args_in(cmd: &str) -> Vec<String> {
    let lower = cmd.to_ascii_lowercase();
    let mut out = Vec::new();
    for key in ["-project=", "-scriptsforproject="] {
        let mut from = 0;
        while let Some(k) = lower[from..].find(key) {
            let start = from + k + key.len();
            let rest = &cmd[start..];
            let value = match rest.strip_prefix('"') {
                Some(q) => q.split('"').next().unwrap_or(""),
                None => rest.split_whitespace().next().unwrap_or(""),
            };
            let v = value.trim().trim_matches('"');
            if !v.is_empty() {
                let p = norm(v);
                let dir = if p.ends_with(".uproject") { Path::new(&p).parent().map(|d| norm(&d.display().to_string())) } else { Some(p) };
                if let Some(d) = dir.filter(|d| !d.is_empty()) {
                    out.push(d);
                }
            }
            from = start;
        }
    }
    out
}

/// Folders of the `.uproject` files a command line names (normalised).
pub fn uproject_dirs_in(cmd: &str) -> Vec<String> {
    let lower = cmd.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(k) = lower[from..].find(".uproject") {
        let end = from + k;
        let start = lower[..end].rfind(['"', '=']).map(|s| s + 1).unwrap_or_else(|| lower[..end].rfind(' ').map(|s| s + 1).unwrap_or(0));
        let p = norm(cmd[start..end + 9].trim().trim_matches('"'));
        if let Some(dir) = Path::new(&p).parent() {
            out.push(norm(&dir.display().to_string()));
        }
        from = end + 9;
    }
    out
}

fn vol_name(guid: &str, vols: &[VolumeResult]) -> String {
    vols.iter().find(|v| v.guid.eq_ignore_ascii_case(guid)).map(|v| v.root.trim_end_matches('\\').to_string()).unwrap_or_else(|| guid.to_string())
}

/// Per-item confidence and the numbers that go with it.
struct Judged {
    conf: Confidence,
    footprint: Option<u64>,
    frees: Option<u64>,
    frees_max: Option<u64>,
    scanned_pct: f64,
    in_flux: bool,
}

fn judge(r: &ItemResult, block_cloning: bool, in_use: bool, stale: bool) -> Judged {
    let l2 = r.level2.as_ref();
    let l1 = &r.level1;
    let in_flux = in_use || l2.map_or(l1.written_during_scan, |l| l.written_during_scan) > 0;
    let facts = ScanFacts {
        unmeasurable: r.unmeasurable.is_some() || r.not_reached,
        block_cloning,
        files_without_refcount: l2.map_or(0, |l| l.files_without_refcount) + u64::from(r.unknown_rc),
        partial: l2.map_or(true, |l| l.cut_short) || l1.cut_short,
        failures: l2.map_or(l1.dir_failures, |l| l.dir_failures + l.open_failures + l.extent_failures),
        in_flux,
        anomalies: r.anomalies,
        inconsistent: r.inconsistent,
        stale,
    };
    let mut conf = disk_space::confidence(&facts);
    if l2.is_none() && block_cloning && conf == Confidence::Estimate {
        // Only sized on a Dev Drive: nothing is known about sharing, so it's a lower bound (of
        // nothing) with its size as the ceiling, even while it changes. "About 0 GB" would say
        // there is nothing to free.
        conf = Confidence::LowerBound;
    }
    let scanned_pct = match l2 {
        Some(l) if !l.cut_short && !l1.cut_short => 100.0,
        Some(_) if l1.cut_short => 0.0,
        Some(l) if l1.files > 0 => (((l.files + l.hard_links) as f64 / l1.files as f64 * 100.0).min(100.0) * 10.0).round() / 10.0,
        Some(_) => 100.0,
        None => 0.0,
    };
    if conf == Confidence::Unknown {
        return Judged { conf, footprint: None, frees: None, frees_max: None, scanned_pct, in_flux };
    }
    if l2.is_none() && !block_cloning {
        // Only sized, but on a volume where files can't share clusters (no block cloning): an
        // item frees what it allocates, less hard links to files outside it, which only the
        // cluster count would find. So: about its size, or at least its size so far.
        let conf = if l1.cut_short || l1.dir_failures > 0 { Confidence::LowerBound } else { Confidence::Estimate };
        let frees = Some(l1.allocated);
        return Judged { conf, footprint: frees, frees, frees_max: if conf == Confidence::LowerBound { None } else { frees }, scanned_pct, in_flux };
    }
    let footprint = l2.map(|_| r.footprint);
    let proven_shared = r.footprint.saturating_sub(r.frees_alone);
    let ceiling = if facts.partial { l1.allocated.saturating_sub(proven_shared).max(r.frees_alone) } else { r.frees_alone };
    Judged { conf, footprint, frees: Some(r.frees_alone), frees_max: Some(ceiling), scanned_pct, in_flux }
}

fn notes_for(r: &ItemResult, in_use: bool) -> Vec<String> {
    let mut v = Vec::new();
    let st = r.level2.as_ref().unwrap_or(&r.level1);
    if let Some(why) = &r.unmeasurable {
        v.push(format!("Not counted: {}", why));
    }
    if st.reparse_dirs > 0 {
        v.push(format!("{} links to other folders not followed", st.reparse_dirs));
    }
    if st.hard_links > 0 {
        v.push(format!("{} hard links counted once", st.hard_links));
    }
    let fails = st.dir_failures + st.open_failures + st.extent_failures;
    if fails > 0 {
        v.push(format!("{} files or folders couldn't be read", fails));
    }
    if st.written_during_scan > 0 {
        v.push(format!("{} files changed while it was counted", st.written_during_scan));
    }
    if r.anomalies || r.inconsistent {
        v.push("Some cluster reference counts looked inconsistent while scanning; frees is an estimate".into());
    }
    if in_use {
        v.push("Unreal is working in this project".into());
    }
    if r.not_reached {
        v.push("Not counted: the time ran out before it was reached. Count again, or give the checkup more time".into());
    } else if r.level2.is_none() && r.unmeasurable.is_none() {
        v.push("Only sized, not counted cluster by cluster: the time ran out first".into());
    } else if r.level2.as_ref().is_some_and(|l| l.cut_short) {
        v.push("Counting stopped before the end".into());
    }
    for p in &r.dropped_paths {
        v.push(format!("Left out: {}", p));
    }
    v
}

fn opt_gb(v: Option<u64>) -> Value {
    v.map_or(Value::Null, |b| json!(gb2(b)))
}

/// The sets a checkup totals: everything `regenerable` per volume, and each lane with its own items.
fn checkup_sets(items: &[CatItem]) -> Vec<(String, Vec<usize>)> {
    let mut sets = Vec::new();
    let regen: Vec<usize> = items.iter().enumerate().filter(|(_, i)| i.class == "regenerable").map(|(k, _)| k).collect();
    if !regen.is_empty() {
        sets.push(("regenerable".to_string(), regen));
    }
    for (k, it) in items.iter().enumerate().filter(|(_, i)| i.key == "lane") {
        let lane = &it.paths[0];
        let mut set = vec![k];
        set.extend(items.iter().enumerate().filter(|(_, i)| i.lane.as_deref() == Some(lane.as_str())).map(|(j, _)| j));
        if set.len() > 1 {
            sets.push((format!("{} with everything in it", it.title), set));
        }
    }
    sets
}

/// Build the checkup report from a scan. `age_sec` > 0 for a cached answer.
pub fn build_checkup(items: &[CatItem], out: &ScanOutput, set_names: &[(String, Vec<usize>)], s: &DiskSettings, age_sec: u64, from_cache: bool, busy: &[String], time: &str) -> Checkup {
    build_checkup_with_extras(items, &[], out, set_names, s, age_sec, from_cache, busy, time)
}

fn build_checkup_with_extras(
    items: &[CatItem],
    extras: &[ItemExtra],
    out: &ScanOutput,
    set_names: &[(String, Vec<usize>)],
    s: &DiskSettings,
    age_sec: u64,
    from_cache: bool,
    busy: &[String],
    time: &str,
) -> Checkup {
    let ids: Vec<String> = (0..items.len()).map(|i| format!("d{}", i + 1)).collect();
    let stale = age_sec > s.fresh_sec;
    let mut confs: Vec<Option<Confidence>> = Vec::new();
    let mut jitems = Vec::new();
    let vol_block = |g: &Option<String>| g.as_ref().and_then(|g| out.volumes.iter().find(|v| v.guid.eq_ignore_ascii_case(g))).is_some_and(|v| v.block_cloning);
    for (k, (it, r)) in items.iter().zip(&out.items).enumerate() {
        let in_use = it.paths.iter().any(|p| busy.iter().any(|b| disk_space::is_within(&norm(p), b) || disk_space::is_within(b, &norm(p))));
        let j = judge(r, vol_block(&r.volume), in_use, stale);
        confs.push(Some(j.conf));
        let l1 = &r.level1;
        let l2 = r.level2.as_ref();
        let excludes: Vec<String> = items
            .iter()
            .enumerate()
            .filter(|(o, _)| *o != k)
            .flat_map(|(_, other)| other.paths.iter())
            .filter(|op| it.paths.iter().any(|p| norm(op) != norm(p) && disk_space::is_within(&norm(op), &norm(p))))
            .cloned()
            .collect();
        let (logical, alloc, files) = match l2.filter(|l| !l.cut_short) {
            Some(l) => (l.logical, l.allocated, l.files),
            None => (l1.logical, l1.allocated, l1.files),
        };
        let known = j.frees.is_some();
        let cat_tool = catalog_info(&it.key).and_then(|c| c.tool);
        let mut item_notes = notes_for(r, in_use);
        let extra = extras.get(k);
        if let Some(ex) = extra {
            if let Some(n) = &ex.note {
                item_notes.push(n.clone());
            }
            if let Some(lm) = &ex.lane {
                let mut parts = Vec::new();
                if let Some(b) = &lm.branch {
                    parts.push(format!("branch {}", b));
                }
                if let Some(dirty) = lm.dirty {
                    if dirty {
                        parts.push(format!("dirty ({} files)", lm.dirty_files.unwrap_or(0)));
                    } else {
                        parts.push("clean".into());
                    }
                }
                if let Some(merged) = lm.merged {
                    parts.push(if merged { "merged" } else { "unmerged" }.into());
                }
                if let Some(up) = lm.unpushed_commits.filter(|&n| n > 0) {
                    parts.push(format!("{} unpushed commit(s)", up));
                }
                if let Some(age) = lm.last_commit_age_days {
                    parts.push(format!("last commit {}d ago", age));
                }
                if !parts.is_empty() {
                    item_notes.push(parts.join(", "));
                }
            }
            if let Some(lfs) = &ex.lfs {
                if let (Some(rec), Some(cnt)) = (lfs.reclaimable_bytes, lfs.prunable_objects) {
                    item_notes.push(format!(
                        "LFS prune estimate{}: {} prunable object(s) (~{:.2} GB reclaimable).",
                        if lfs.verify_remote { " (remote verified)" } else { " (local)" },
                        cnt,
                        gb2(rec)
                    ));
                }
                if let Some(n) = &lfs.note {
                    item_notes.push(n.clone());
                }
            }
        }
        let mut item_obj = json!({
            "id": ids[k], "key": it.key, "class": it.class, "title": it.title,
            "volume": r.volume.as_deref().map(|g| vol_name(g, &out.volumes)),
            "paths": it.paths, "excludes": excludes, "files": files,
            "logical_gb": gb2(logical), "allocated_gb": gb2(alloc),
            "footprint_gb": opt_gb(j.footprint),
            "frees_alone_gb": opt_gb(j.frees), "frees_max_gb": opt_gb(j.frees_max),
            "frees_basis": BASIS, "frees_confidence": j.conf.name(), "scanned_pct": j.scanned_pct,
            "stays_shared_gb": if l2.is_some() && known { json!(gb2(r.footprint.saturating_sub(r.frees_alone))) } else { Value::Null },
            "shared_with": r.shared_with.iter().filter(|(_, b)| *b >= 10 << 20).map(|(o, b)| json!({ "id": ids[*o], "gb": gb2(*b) })).collect::<Vec<_>>(),
            "shared_outside_scan_gb": if l2.is_some() && known { json!(gb2(r.shared_outside)) } else { Value::Null },
            "last_write": disk_win::filetime_local(l2.map_or(l1.last_write, |l| l.last_write.max(l1.last_write))),
            "in_use": in_use, "in_flux": j.in_flux, "notes": item_notes,
            "actions": crate::disk_clean::actions_json(&it.key, &it.class, cat_tool.as_deref(), r.volume.is_some(), j.frees.map(gb2)),
            "no_action": crate::disk_clean::no_action_reason_for(&it.key, &it.class, cat_tool.as_deref()),
            "recommended": false,
        });
        if let Some(ex) = extra {
            if let Some(lm) = &ex.lane {
                item_obj["worktree"] = json!({
                    "main_repo": lm.main_repo,
                    "branch": lm.branch,
                    "merged": lm.merged,
                    "dirty": lm.dirty,
                    "dirty_files": lm.dirty_files,
                    "unpushed_commits": lm.unpushed_commits,
                    "last_commit_age_days": lm.last_commit_age_days,
                });
            }
            if let Some(lfs) = &ex.lfs {
                item_obj["git"] = json!({
                    "reclaimable_gb": lfs.reclaimable_bytes.map(gb2),
                    "retained_gb": lfs.retained_bytes.map(gb2),
                    "prunable_objects": lfs.prunable_objects,
                    "retained_objects": lfs.retained_objects,
                    "verify_remote": lfs.verify_remote,
                });
            }
        }
        jitems.push(item_obj);
    }
    let mut totals = Vec::new();
    for (si, (name, _)) in set_names.iter().enumerate() {
        for sr in out.sets.iter().filter(|sr| sr.set == si) {
            let member_conf: Vec<Confidence> = sr.items.iter().filter_map(|i| confs[*i]).collect();
            let tc = total_confidence(&member_conf);
            totals.push(json!({
                "set": name, "volume": vol_name(&sr.volume, &out.volumes),
                "item_ids": sr.items.iter().map(|i| ids[*i].clone()).collect::<Vec<_>>(),
                "frees_together_gb": tc.map(|_| gb2(sr.frees_together)),
                "frees_confidence": tc.map_or("none", |c| c.name()),
                "frees_basis": BASIS,
                "reason": if tc.is_none() { json!("A member could only be bounded from above or not measured, so there is no total.") } else { Value::Null },
            }));
        }
    }
    let groups = out
        .groups
        .iter()
        .map(|g| json!({ "item_ids": g.items.iter().map(|i| ids[*i].clone()).collect::<Vec<_>>(), "shared_gb": gb2(g.shared), "frees_if_all_removed_gb": gb2(g.frees_if_all_removed), "frees_basis": BASIS }))
        .collect();
    let volumes = out
        .volumes
        .iter()
        .map(|v| {
            let basis = if v.block_cloning { "refs_refcount" } else { "allocated_and_links" };
            let mut notes = Vec::new();
            if v.fs.eq_ignore_ascii_case("ReFS") {
                notes.push(REFS_DELAY_NOTE.to_string());
            }
            if v.block_cloning {
                notes.push("Files on this drive share space, so \"holds\" numbers overlap. Only \"frees\" is real, and only totals may be added up.".to_string());
            }
            let vs = out.volume_stats.iter().find(|x| x.guid.eq_ignore_ascii_case(&v.guid));
            let mut vobj = json!({
                "volume_id": v.guid, "paths": [v.root], "fs": v.fs, "free_gb": gb2(v.avail), "basis": basis,
                "used_gb": gb2(v.total.saturating_sub(v.free)), "unattributed_gb": Value::Null, "notes": notes
            });
            if let Some(vs) = vs {
                if vs.cluster > 0 {
                    vobj["cluster_kb"] = json!(vs.cluster / 1024);
                }
                if v.block_cloning {
                    let cow_saved = vs.sum_l2_allocated.saturating_sub(vs.union_bytes);
                    vobj["cow_saved_gb"] = json!(gb2(cow_saved));
                    if cow_saved >= (100 << 20) {
                        notes.push(format!("Copy-on-write cloning saves about {:.1} GB across scanned items.", gb2(cow_saved)));
                    }
                }
                if vs.cluster >= 65_536 && vs.total_files >= 10_000 {
                    let slack = vs.total_allocated.saturating_sub(vs.total_logical);
                    if slack >= (1u64 << 30) {
                        notes.push(format!("64 KB clusters add about {:.1} GB of cluster slack across {} files.", gb2(slack), vs.total_files));
                    }
                }
                vobj["notes"] = json!(notes);
            }
            vobj
        })
        .collect();
    let files: u64 = out.items.iter().map(|r| r.level2.as_ref().map_or(r.level1.files, |l| l.files.max(r.level1.files))).sum();
    Checkup {
        scan: json!({ "time": time, "age_sec": age_sec, "from_cache": from_cache, "partial": out.partial, "stopped_for_build": out.stopped_for_build,
                      "took_sec": out.took_sec, "files": files }),
        volumes,
        items: jitems,
        sharing_groups: groups,
        totals,
        basis: BASIS.into(),
        do_not_sum: DO_NOT_SUM.into(),
    }
}

/// Whether an old result's frees number turns into "about": exact and lower-bound numbers do,
/// except a lower bound of about nothing (only sized on a Dev Drive), which stays a lower
/// bound so it reads "up to X" (its ceiling) or "at least 0", never "about 0 GB".
pub fn ages_to_estimate(it: &Value, frees_key: &str) -> bool {
    match it["frees_confidence"].as_str() {
        Some("exact") => true,
        Some("lower_bound") => it[frees_key].as_f64().is_some_and(|g| g >= 0.05),
        _ => false,
    }
}

pub fn scope_key(path: Option<&str>) -> String {
    path.map(norm).unwrap_or_else(|| "*".into())
}

/// The disk checkup. Answers from the cache (with its age) unless `refresh`, there is none,
/// it was for another path, or an item's root changed (its file id).
pub fn checkup(cfg: &Config, path: Option<&str>, refresh: bool, budget_sec: Option<u64>, runner: Runner, progress: Progress) -> Result<Checkup, String> {
    let s = cfg.disk();
    if let Some(p) = path {
        if !Path::new(p).is_dir() {
            return Err(format!("{} is not a folder on this PC.", p));
        }
    }
    let scope = scope_key(path);
    if !refresh {
        if let Some(c) = load_cache().filter(|c| c.scope == scope) {
            let still = c.items.iter().zip(&c.root_ids).all(|(it, ids)| it.paths.first().and_then(|p| disk_win::identity(p)).map(|(_, f, _)| ids.contains(&f)).unwrap_or(false) || ids.is_empty());
            if still {
                let age = unix_now().saturating_sub(c.unix);
                let mut rep = c.report.clone();
                rep.scan["checkup_id"] = json!(c.ident());
                rep.scan["age_sec"] = json!(age);
                rep.scan["from_cache"] = json!(true);
                if age > s.fresh_sec {
                    for it in &mut rep.items {
                        if ages_to_estimate(it, "frees_alone_gb") {
                            it["frees_confidence"] = json!("estimate");
                        }
                    }
                    for t in &mut rep.totals {
                        if ages_to_estimate(t, "frees_together_gb") {
                            t["frees_confidence"] = json!("estimate");
                        }
                    }
                }
                return Ok(rep);
            }
        }
    }
    let (items, extras) = discover_detailed(cfg, path);
    let sets = checkup_sets(&items);
    let no_walk_mask: Vec<bool> = items
        .iter()
        .map(|i| catalog_info(&i.key).is_some_and(|c| c.no_walk))
        .collect();
    let job_items: Vec<Vec<String>> = items
        .iter()
        .zip(&no_walk_mask)
        .map(|(i, &nw)| if nw { Vec::new() } else { i.paths.clone() })
        .collect();
    let job = ScanJob {
        items: job_items,
        sets: sets.iter().map(|s| s.1.clone()).collect(),
        threads: s.scan_threads,
        budget_sec: budget_sec.unwrap_or(s.scan_budget_sec).clamp(1, 3600),
        stop_on_build: true,
    };
    let mut out = runner(&job, progress)?;
    for (i, &nw) in no_walk_mask.iter().enumerate() {
        if nw {
            if let Some(r) = out.items.get_mut(i) {
                r.unmeasurable = Some("component store is hard-linked; use DISM to inspect safely".into());
                if let Some(p) = items[i].paths.first() {
                    r.volume = disk_win::volume_of(p).map(|(_, g)| g);
                }
            }
        }
    }
    let time = crate::win::local_time_string();
    let rep = build_checkup_with_extras(&items, &extras, &out, &sets, &s, 0, false, &busy_projects(), &time);
    if path.is_none() {
        record_system_caches(&items, &out, &time);
    }
    let id = format!("c{}", crate::disk_clean::run_id());
    let mut rep = rep;
    rep.scan["checkup_id"] = json!(id);
    save_cache(&CachedCheckup {
        id,
        scope,
        unix: unix_now(),
        time,
        root_ids: out.items.iter().map(|r| r.root_ids.clone()).collect(),
        last_writes: out.items.iter().map(|r| r.level2.as_ref().map_or(r.level1.last_write, |l| l.last_write.max(r.level1.last_write))).collect(),
        items,
        report: rep.clone(),
    });
    Ok(rep)
}

fn record_system_caches(items: &[CatItem], out: &ScanOutput, time: &str) {
    let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let Some((_, sys_guid)) = disk_win::volume_of(&format!("{}\\", sys)) else { return };
    let bytes: u64 = items
        .iter()
        .zip(&out.items)
        .filter(|(i, r)| {
            i.key != "lane"
                && i.lane.is_none()
                && r.volume.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(&sys_guid))
                && (i.key.starts_with("ue:global")
                    || i.key == "ue:zen"
                    || i.key.starts_with("ue:uba")
                    || i.key == "ue:vault_cache"
                    || i.key == "ue:swarm_cache"
                    || i.key.starts_with("cache:")
                    || i.key.starts_with("gpu:")
                    || i.key == "sys:crashdumps"
                    || i.key == "temp:old")
        })
        .map(|(_, r)| r.level1.allocated)
        .sum();
    let slack_64k: u64 = out
        .volume_stats
        .iter()
        .filter(|vs| vs.cluster >= 65_536 && vs.total_files >= 10_000)
        .map(|vs| vs.total_allocated.saturating_sub(vs.total_logical))
        .max()
        .unwrap_or(0);
    let cow_saved: u64 = out
        .volume_stats
        .iter()
        .zip(&out.volumes)
        .filter(|(_, v)| v.block_cloning)
        .map(|(vs, _)| vs.sum_l2_allocated.saturating_sub(vs.union_bytes))
        .sum();
    if cache_writes_allowed() {
        let c = crate::disk::CachesOnSystem {
            gb: (bytes as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0,
            measured: time.to_string(),
            refs_64k_slack_gb: (slack_64k >= (512 << 20)).then(|| gb2(slack_64k)),
            cow_saved_gb: (cow_saved >= (100 << 20)).then(|| gb2(cow_saved)),
        };
        if let Ok(bytes) = serde_json::to_vec(&c) {
            let _ = crate::disk_clean::write_atomic(&caches_file(), &bytes);
        }
    }
}

/// `memguard disk plan`: fresh numbers for exactly these items of the last checkup (all of
/// them when `ids` is empty). One entry per volume, never a sum across volumes. The plan is
/// recorded (`disk-plans.json`) so `clean` can check nothing changed since.
pub fn plan(cfg: &Config, ids: &[String], runner: Runner, progress: Progress) -> Result<Value, String> {
    let c = load_cache().ok_or("Run a disk checkup first (memguard disk checkup, or memguard_disk_checkup): a plan uses its item ids.")?;
    plan_from(cfg, &c, ids, runner, progress)
}

pub fn plan_from(cfg: &Config, c: &CachedCheckup, ids: &[String], runner: Runner, progress: Progress) -> Result<Value, String> {
    let (v, rec) = plan_record(cfg, c, ids, None, runner, progress)?;
    let _ = crate::disk_clean::DiskFiles::default().save_plan(&rec);
    Ok(v)
}

/// The plan and its record. `together`: the ids whose `frees_together` is computed (default:
/// all of them; `clean` passes only the items it deletes, since a bin move frees nothing).
pub fn plan_record(
    cfg: &Config,
    c: &CachedCheckup,
    ids: &[String],
    together: Option<&[String]>,
    runner: Runner,
    progress: Progress,
) -> Result<(Value, crate::disk_clean::PlanRecord), String> {
    use crate::disk_clean::{PlanRecord, PlannedItem, PlannedVolume};
    let s = cfg.disk();
    let mut picked: Vec<usize> = Vec::new();
    let mut refused = Vec::new();
    let all = ids.is_empty() || ids.iter().any(|i| i.eq_ignore_ascii_case("all"));
    if all {
        picked = (0..c.items.len()).collect();
    } else {
        for id in ids {
            match id.trim().strip_prefix('d').and_then(|n| n.parse::<usize>().ok()).filter(|n| *n >= 1 && *n <= c.items.len()) {
                Some(n) if !picked.contains(&(n - 1)) => picked.push(n - 1),
                Some(_) => {}
                None => refused.push(json!({ "id": id, "reason": "Not an item of the last disk checkup" })),
            }
        }
    }
    if picked.is_empty() {
        return Err("None of these ids are in the last disk checkup. Run it again and use its ids (d1, d2, ...).".into());
    }
    let items: Vec<CatItem> = picked.iter().map(|i| c.items[*i].clone()).collect();
    let id_of = |k: usize| format!("d{}", picked[k] + 1);
    let set: Vec<usize> = match together {
        None => (0..items.len()).collect(),
        Some(t) => (0..items.len()).filter(|k| t.iter().any(|x| x.eq_ignore_ascii_case(&id_of(*k)))).collect(),
    };
    let no_walk_mask: Vec<bool> = items
        .iter()
        .map(|i| catalog_info(&i.key).is_some_and(|cat| cat.no_walk))
        .collect();
    let job_items: Vec<Vec<String>> = items
        .iter()
        .zip(&no_walk_mask)
        .map(|(i, &nw)| if nw { Vec::new() } else { i.paths.clone() })
        .collect();
    let job = ScanJob {
        items: job_items,
        sets: if set.is_empty() { Vec::new() } else { vec![set] },
        threads: s.scan_threads,
        budget_sec: s.scan_budget_sec.max(300),
        stop_on_build: false,
    };
    let mut out = runner(&job, progress)?;
    for (i, &nw) in no_walk_mask.iter().enumerate() {
        if nw {
            if let Some(r) = out.items.get_mut(i) {
                r.unmeasurable = Some("component store is hard-linked; use DISM to inspect safely".into());
                if let Some(p) = items[i].paths.first() {
                    r.volume = disk_win::volume_of(p).map(|(_, g)| g);
                }
            }
        }
    }
    let busy = busy_projects();
    let now = crate::win::local_time_string();
    let plan_id = format!("p{}", crate::disk_clean::run_id());
    let mut rec = PlanRecord { id: plan_id.clone(), unix: crate::disk_clean::unix_now(), time: now.clone(), items: Vec::new(), volumes: Vec::new() };
    let in_use_of = |it: &CatItem| it.paths.iter().any(|p| busy.iter().any(|b| disk_space::is_within(&norm(p), b) || disk_space::is_within(b, &norm(p))));
    let mut confs_by_item: Vec<Confidence> = Vec::new();
    for (k, it) in items.iter().enumerate() {
        let r = &out.items[k];
        let vol = r.volume.as_ref().and_then(|g| out.volumes.iter().find(|v| v.guid.eq_ignore_ascii_case(g)));
        let j = judge(r, vol.is_some_and(|v| v.block_cloning), in_use_of(it), false);
        confs_by_item.push(j.conf);
        let logical_size = (!r.not_reached && r.unmeasurable.is_none() && !r.level1.cut_short).then(|| {
            r.level2.as_ref().filter(|l| !l.cut_short).map_or(r.level1.logical, |l| l.logical)
        });
        rec.items.push(PlannedItem {
            id: id_of(k),
            key: it.key.clone(),
            class: it.class.clone(),
            title: it.title.clone(),
            paths: it.paths.clone(),
            path_ids: it.paths.iter().map(|p| disk_win::identity(p).map(|(_, f, _)| f)).collect(),
            last_write: r.level2.as_ref().map_or(r.level1.last_write, |l| l.last_write.max(r.level1.last_write)),
            volume: r.volume.clone(),
            volume_root: vol.map(|v| v.root.clone()),
            frees_alone: j.frees,
            confidence: j.conf.name().into(),
            tool: catalog_info(&it.key).and_then(|c| c.tool),
            unmeasurable: r.unmeasurable.clone(),
            logical_size,
        });
    }
    let mut plans = Vec::new();
    for sr in &out.sets {
        let vol = out.volumes.iter().find(|v| v.guid.eq_ignore_ascii_case(&sr.volume));
        let mut confs = Vec::new();
        let mut per_item = Vec::new();
        for &k in &sr.items {
            confs.push(confs_by_item[k]);
            per_item.push(json!({ "id": id_of(k), "title": items[k].title, "frees_alone_gb": opt_gb(rec.items[k].frees_alone), "frees_confidence": confs_by_item[k].name(),
                                  "frees_basis": BASIS, "notes": notes_for(&out.items[k], in_use_of(&items[k])) }));
        }
        let tc = total_confidence(&confs);
        let free_now = vol.map_or(0, |v| v.avail);
        let mut notes = Vec::new();
        if vol.is_some_and(|v| v.fs.eq_ignore_ascii_case("ReFS")) {
            notes.push(REFS_DELAY_NOTE.to_string());
        }
        rec.volumes.push(PlannedVolume {
            volume: sr.volume.clone(),
            root: vol.map(|v| v.root.clone()).unwrap_or_default(),
            fs: vol.map(|v| v.fs.clone()).unwrap_or_default(),
            item_ids: sr.items.iter().map(|k| id_of(*k)).collect(),
            frees_together: tc.map(|_| sr.frees_together),
            confidence: tc.map_or("none", |c| c.name()).into(),
        });
        plans.push(json!({
            "volume": vol_name(&sr.volume, &out.volumes),
            "item_ids": sr.items.iter().map(|k| id_of(*k)).collect::<Vec<_>>(),
            "frees_together_gb": tc.map(|_| gb2(sr.frees_together)),
            "frees_confidence": tc.map_or("none", |c| c.name()),
            "frees_basis": BASIS,
            "per_item": per_item,
            "free_now_gb": gb2(free_now),
            "free_after_gb": tc.map(|_| gb2(free_now + sr.frees_together)),
            "free_after_basis": BASIS,
            "measured_at": now,
            "took_sec": out.took_sec,
            "notes": notes,
        }));
    }
    for (k, r) in out.items.iter().enumerate() {
        if let Some(why) = &r.unmeasurable {
            refused.push(json!({ "id": id_of(k), "reason": format!("Not counted: {}", why) }));
        }
    }
    let actions: Vec<Value> = rec
        .items
        .iter()
        .map(|it| {
            let a = crate::disk_clean::actions_json(&it.key, &it.class, it.tool.as_deref(), it.volume_root.is_some(), it.frees_alone.map(gb2));
            json!({ "id": it.id, "class": it.class, "actions": a, "no_action": crate::disk_clean::no_action_reason_for(&it.key, &it.class, it.tool.as_deref()) })
        })
        .collect();
    let cleanable: Vec<String> =
        rec.items.iter().filter(|it| !crate::disk_clean::offered_actions(&it.key, &it.class, it.tool.as_deref(), true).is_empty()).map(|it| it.id.clone()).collect();
    let v = json!({
        "plan_id": plan_id,
        "plans": plans,
        "items": actions,
        "refused": refused,
        "basis": BASIS,
        "command": if cleanable.is_empty() { Value::Null } else { json!(format!("memguard disk clean --fix {}", cleanable.join(" "))) },
        "removing": "To act on it: memguard disk clean --fix <id>[:now|bin|tool] (asks first). An agent may call memguard_disk_clean for regenerable items within 10 minutes of this plan. \
Ather MemGuard never removes lanes or worktrees: use git worktree remove for those.",
        "do_not_sum": "Plans on different drives are never added together. frees_together_gb is predicted, not measured.",
    });
    Ok((v, rec))
}

// ---------------------------------------------------------------- text

fn class_heading(class: &str) -> &'static str {
    match class {
        "regenerable" => "SAFE TO REMOVE (it is rebuilt when needed)",
        "disposable" => "NOT NEEDED (not rebuilt, and nothing needs it)",
        "rebuild_cost" => "SLOW TO REBUILD (rebuilt, but it takes long)",
        _ => "REVIEW (your call)",
    }
}

fn qualified(v: &Value, conf: &str) -> String {
    match v.as_f64() {
        None if conf == "unknown" => "not counted".into(),
        None => "not measured".into(),
        Some(g) => {
            let q = match conf {
                "lower_bound" => "at least ",
                "estimate" => "about ",
                "upper_bound" => "at most ",
                _ => "",
            };
            format!("{}{:.1} GB", q, g)
        }
    }
}

/// An item's frees: like `qualified`, but "up to X GB" where the lower bound is about nothing
/// because the clusters weren't counted in time (a Dev Drive item that was only sized).
fn qualified_item(it: &Value, conf: &str) -> String {
    let lo = it["frees_alone_gb"].as_f64();
    match (lo, it["frees_max_gb"].as_f64()) {
        (Some(l), Some(m)) if conf == "lower_bound" && l < 0.05 && m >= 0.05 => format!("up to {:.1} GB", m),
        _ => qualified(&it["frees_alone_gb"], conf),
    }
}

/// `memguard disk checkup` as text.
pub fn render_checkup(c: &Checkup) -> String {
    let mut s = String::new();
    for v in &c.volumes {
        s.push_str(&format!("{} ({})   {:.1} GB free\n", v["paths"][0].as_str().unwrap_or("").trim_end_matches('\\'), v["fs"].as_str().unwrap_or(""), v["free_gb"].as_f64().unwrap_or(0.0)));
        for n in v["notes"].as_array().into_iter().flatten() {
            s.push_str(&format!("  {}\n", n.as_str().unwrap_or("")));
        }
    }
    let scan = &c.scan;
    let age = scan["age_sec"].as_u64().unwrap_or(0);
    s.push_str(&format!(
        "Counted {} {}: {} files in {:.0} s{}{}.\n",
        if age > 0 { format!("{} ago", age_words(age)) } else { "just now".into() },
        if scan["from_cache"] == true { "(saved result)" } else { "" },
        scan["files"].as_u64().unwrap_or(0),
        scan["took_sec"].as_f64().unwrap_or(0.0),
        if scan["partial"] == true { ", stopped before the end (time limit)" } else { "" },
        if scan["stopped_for_build"] == true { ", paused for a build or cook" } else { "" },
    ));
    s.push_str("\"Frees\" is predicted from the drive's own counts, not measured after a delete.\n\n");
    s.push_str(&format!("{:<44} {:>10}   {}\n", "", "HOLDS", "FREES IF REMOVED"));
    let mut last_class = String::new();
    for it in &c.items {
        let class = it["class"].as_str().unwrap_or("");
        if class != last_class {
            s.push_str(&format!("{}\n", class_heading(class)));
            last_class = class.to_string();
        }
        let conf = it["frees_confidence"].as_str().unwrap_or("");
        let title: String = it["title"].as_str().unwrap_or("").chars().take(38).collect();
        let mut line = format!("  {:<4}{:<38} {:>8.1} GB   {} ({})", it["id"].as_str().unwrap_or(""), title, it["logical_gb"].as_f64().unwrap_or(0.0), qualified_item(it, conf), conf);
        if let Some(stays) = it["stays_shared_gb"].as_f64().filter(|g| *g >= 0.1) {
            line.push_str(&format!("   {:.1} GB stays: other trees use it", stays));
        }
        s.push_str(&line);
        s.push('\n');
    }
    for g in &c.sharing_groups {
        let ids: Vec<&str> = g["item_ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
        if ids.len() == 2 && g["frees_if_all_removed_gb"].as_f64().unwrap_or(0.0) >= 1.0 {
            s.push_str(&format!(
                "      {} and {} share {:.1} GB; {:.1} GB of it is used by nothing else.\n",
                ids[0],
                ids[1],
                g["shared_gb"].as_f64().unwrap_or(0.0),
                g["frees_if_all_removed_gb"].as_f64().unwrap_or(0.0)
            ));
        }
    }
    s.push('\n');
    for t in &c.totals {
        let ids: Vec<&str> = t["item_ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
        match t["frees_together_gb"].as_f64() {
            Some(_) => s.push_str(&format!(
                "Removing {} ({}) frees {} on {} (predicted, counted once).\n",
                t["set"].as_str().unwrap_or(""),
                ids.join(" "),
                qualified(&t["frees_together_gb"], t["frees_confidence"].as_str().unwrap_or("")),
                t["volume"].as_str().unwrap_or("")
            )),
            None => s.push_str(&format!("No total for {}: {}\n", t["set"].as_str().unwrap_or(""), t["reason"].as_str().unwrap_or(""))),
        }
    }
    if !c.items.is_empty() {
        s.push_str("Next: memguard disk plan <ids>   fresh numbers for exactly the items you pick.\n");
        s.push_str("Then: memguard disk clean --fix <id>[:now|bin|tool] ...   asks first; bin moves can be undone, deletes can't.\n");
        if let Some(id) = c.scan["checkup_id"].as_str() {
            s.push_str(&format!("Scripts: add --yes --checkup {}, so a newer checkup can't change what the ids mean.\n", id));
        }
    } else {
        s.push_str("Nothing to list here.\n");
    }
    s
}

/// `memguard disk plan` as text.
pub fn render_plan(p: &Value) -> String {
    let mut s = String::new();
    for pl in p["plans"].as_array().into_iter().flatten() {
        let ids: Vec<&str> = pl["item_ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
        let conf = pl["frees_confidence"].as_str().unwrap_or("");
        s.push_str(&format!("On {}, removing {} frees {} (predicted, {}).\n", pl["volume"].as_str().unwrap_or(""), ids.join(" "), qualified(&pl["frees_together_gb"], conf), conf));
        for it in pl["per_item"].as_array().into_iter().flatten() {
            s.push_str(&format!("  {:<4}{:<40} alone: {}\n", it["id"].as_str().unwrap_or(""), it["title"].as_str().unwrap_or(""), qualified(&it["frees_alone_gb"], it["frees_confidence"].as_str().unwrap_or(""))));
        }
        if let Some(after) = pl["free_after_gb"].as_f64() {
            s.push_str(&format!("  Free now {:.1} GB, about {:.1} GB after (predicted).\n", pl["free_now_gb"].as_f64().unwrap_or(0.0), after));
        }
        for n in pl["notes"].as_array().into_iter().flatten() {
            s.push_str(&format!("  {}\n", n.as_str().unwrap_or("")));
        }
    }
    for r in p["refused"].as_array().into_iter().flatten() {
        s.push_str(&format!("Left out {}: {}\n", r["id"].as_str().unwrap_or(""), r["reason"].as_str().unwrap_or("")));
    }
    s.push_str(p["removing"].as_str().unwrap_or(""));
    s.push('\n');
    s
}

fn age_words(sec: u64) -> String {
    match sec {
        0..=89 => format!("{} s", sec),
        90..=5399 => format!("{} min", (sec + 30) / 60),
        _ => format!("{} h", (sec + 1800) / 3600),
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_parses_and_classes_are_known() {
        let c = catalog();
        assert!(c.project.iter().any(|e| e.key == "ue:intermediate"));
        assert!(c.global.iter().any(|e| e.key == "temp:old" && e.older_than_days == Some(7)));
        for e in c.project.iter().chain(&c.global) {
            assert!(["regenerable", "disposable", "rebuild_cost", "review"].contains(&e.class.as_str()), "{}", e.key);
            assert!(!e.key.contains(':') || e.key.split(':').count() == 2);
        }
        // `Saved` itself is never an item: it holds autosaves and config.
        assert!(!c.project.iter().any(|e| e.path.eq_ignore_ascii_case("Saved")));
    }

    #[test]
    fn gitdir_and_command_lines() {
        assert_eq!(parse_gitdir_target("D:/lanes/sched-cleanup/.git\n"), Some(PathBuf::from(r"D:\lanes\sched-cleanup")));
        let dirs = uproject_dirs_in(r#""C:\S2Engine\Engine\Binaries\Win64\UnrealEditor.exe" "D:\s2\S2.uproject" -log"#);
        assert_eq!(dirs, vec![r"d:\s2".to_string()]);
        let dirs = uproject_dirs_in(r#"UnrealBuildTool.exe S2Editor Win64 Development -Project=D:\lanes\a\S2.uproject"#);
        assert_eq!(dirs, vec![r"d:\lanes\a".to_string()]);
    }

    /// Found in review: builds started from a command line (dotnet UnrealBuildTool.dll, UAT,
    /// shader workers) weren't seen as "in use".
    #[test]
    fn command_line_builds_are_unreal_work() {
        use UnrealUse::*;
        let p = |v: &[&str]| Projects(v.iter().map(|s| s.to_string()).collect());
        assert_eq!(
            unreal_use("dotnet", Some(r#""C:\Program Files\dotnet\dotnet.exe" "C:\UE_5.4\Engine\Binaries\DotNET\UnrealBuildTool\UnrealBuildTool.dll" S2Editor Win64 Development -Project="D:\s2\S2.uproject" -WaitMutex"#)),
            p(&[r"d:\s2"])
        );
        assert_eq!(
            unreal_use("dotnet.exe", Some(r#"dotnet "C:\UE\Engine\Binaries\DotNET\AutomationTool\AutomationTool.dll" BuildCookRun -project=D:\lanes\a\S2.uproject -cook"#)),
            p(&[r"d:\lanes\a"])
        );
        // A folder instead of a .uproject, and ScriptsForProject.
        assert_eq!(unreal_use("AutomationTool", Some(r#"AutomationTool.exe BuildCookRun -project="D:\s2" -cook"#)), p(&[r"d:\s2"]));
        assert_eq!(unreal_use("UnrealBuildTool", Some(r#"UnrealBuildTool.exe -Mode=QueryTargets -ScriptsForProject=D:\s2\S2.uproject"#)), p(&[r"d:\s2"]));
        assert_eq!(unreal_use("UnrealEditor-Cmd", Some(r#""C:\UE\UnrealEditor-Cmd.exe" "D:\s2\S2.uproject" -run=cook -targetplatform=Windows"#)), p(&[r"d:\s2"]));
        // Running, but no project can be found: busy, unknown project.
        assert_eq!(unreal_use("ShaderCompileWorker", Some(r#""C:\UE\ShaderCompileWorker.exe" "D:\s2\Intermediate\Shaders\WorkingDirectory\1234\" 4321 7 Worker.in Worker.out"#)), Unknown);
        assert_eq!(unreal_use("UnrealPak", Some(r#"UnrealPak.exe D:\out\pak.pak -create=list.txt"#)), Unknown);
        assert_eq!(unreal_use("dotnet", Some(r#"dotnet UnrealBuildTool.dll -Mode=GenerateClangDatabase"#)), Unknown);
        assert_eq!(unreal_use("UnrealEditor", Some(r#""C:\UE\UnrealEditor.exe""#)), Unknown);
        assert_eq!(unreal_use("UnrealBuildTool", None), Unreadable, "command line unreadable");
        assert_eq!(unreal_use("dotnet", None), Unreadable, "an unreadable dotnet could be a build");
        // Other dotnet work and other programs are not Unreal work.
        assert_eq!(unreal_use("dotnet", Some(r#"dotnet "C:\Users\me\.vscode\extensions\csharp\Microsoft.CodeAnalysis.LanguageServer.dll""#)), NotUnreal);
        assert_eq!(unreal_use("devenv", Some("devenv.exe D:\\s2\\S2.sln")), NotUnreal);
        assert!(unreal_candidate("ShaderCompileWorker.exe") && unreal_candidate("UnrealPak") && !unreal_candidate("node"));
    }

    /// Found in review: an unreadable dotnet (a SYSTEM service, say) made every cleanup say
    /// "Unreal is running". Only this person's own process in their session counts, and the
    /// words say only what is known.
    #[test]
    fn unreadable_process_counts_only_when_its_mine() {
        use crate::doctor_win::Ownership;
        let mine = Ownership { owned: true, same_session: true };
        let u = UnrealUse::Unreadable;
        assert_eq!(unknown_words("dotnet.exe", &u, mine), Some(BUILD_TOOL_UNREADABLE));
        assert_eq!(unknown_words("UnrealBuildTool", &u, mine), Some(BUILD_TOOL_UNREADABLE));
        assert_eq!(unknown_words("UnrealEditor", &u, mine), Some(EDITOR_UNREADABLE));
        // A SYSTEM service (session 0, another owner), another user in this session, this user elsewhere.
        assert_eq!(unknown_words("dotnet.exe", &u, Ownership { owned: false, same_session: false }), None);
        assert_eq!(unknown_words("dotnet.exe", &u, Ownership { owned: false, same_session: true }), None);
        assert_eq!(unknown_words("dotnet.exe", &u, Ownership { owned: true, same_session: false }), None);
        // Readable but naming no project: waits whoever owns it; not Unreal work never waits.
        assert_eq!(unknown_words("UnrealEditor", &UnrealUse::Unknown, Ownership::default()), Some(UNREAL_UNKNOWN));
        assert_eq!(unknown_words("dotnet", &UnrealUse::NotUnreal, mine), None);
        assert!(!BUILD_TOOL_UNREADABLE.contains("Unreal is running"));
    }

    /// A small tree on this PC's temp drive, scanned in-process: files, nested items and the
    /// set total follow the rules (everything there is exclusive to the test).
    #[test]
    fn scans_a_test_tree_in_process() {
        let root = std::env::temp_dir().join(format!("amg-disk-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"proj\Intermediate\Build")).unwrap();
        std::fs::create_dir_all(root.join(r"proj\Saved\Logs")).unwrap();
        std::fs::write(root.join(r"proj\Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();
        std::fs::write(root.join(r"proj\Intermediate\Build\a.bin"), vec![7u8; 300_000]).unwrap();
        std::fs::write(root.join(r"proj\Saved\Logs\Game.log"), vec![1u8; 100_000]).unwrap();
        let cfg = Config::builtin();
        let items = discover(&cfg, Some(&root.display().to_string()));
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["ue:intermediate", "ue:logs"]);
        let sets = checkup_sets(&items);
        let job = ScanJob { items: items.iter().map(|i| i.paths.clone()).collect(), sets: sets.iter().map(|s| s.1.clone()).collect(), threads: 2, budget_sec: 30, stop_on_build: false };
        let out = run_job(&job, &mut |_, _| {});
        assert!(!out.partial);
        assert_eq!(out.items[0].level2.as_ref().unwrap().files, 1);
        assert_eq!(out.items[0].level2.as_ref().unwrap().logical, 300_000);
        assert!(out.items[0].frees_alone >= 300_000, "{:?}", out.items[0]);
        let rep = build_checkup(&items, &out, &sets, &cfg.disk(), 0, false, &[], "now");
        assert_eq!(rep.items[0]["id"], "d1");
        assert_eq!(rep.items[0]["frees_basis"], "predicted");
        assert!(rep.items[0]["frees_confidence"] == "exact" || rep.items[0]["frees_confidence"] == "estimate", "{}", rep.items[0]);
        assert_eq!(rep.basis, "predicted");
        assert_eq!(rep.totals[0]["set"], "regenerable");
        assert_eq!(rep.totals[0]["frees_basis"], "predicted");
        // A plan over the same items, from a cache record.
        let cached = CachedCheckup { id: "c1".into(), scope: "x".into(), unix: 0, time: String::new(), items: items.clone(), root_ids: vec![], last_writes: vec![], report: rep };
        let run = |j: &ScanJob, p: Progress| Ok(run_job(j, p));
        let plan = plan_from(&cfg, &cached, &["d1".into(), "d9".into()], &run, &mut |_, _| {}).unwrap();
        assert_eq!(plan["plans"][0]["item_ids"], json!(["d1"]));
        assert_eq!(plan["refused"][0]["id"], "d9");
        assert_eq!(plan["basis"], "predicted");
        assert!(plan["plans"][0]["frees_together_gb"].is_number() || plan["plans"][0]["frees_together_gb"].is_null());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lane_sets_include_their_own_items() {
        let items = vec![
            CatItem { key: "lane".into(), class: "review".into(), title: "Lane a".into(), paths: vec![r"D:\lanes\a".into()], lane: None },
            CatItem { key: "ue:intermediate".into(), class: "regenerable".into(), title: "x".into(), paths: vec![r"D:\lanes\a\Intermediate".into()], lane: Some(r"D:\lanes\a".into()) },
        ];
        let sets = checkup_sets(&items);
        assert_eq!(sets[0], ("regenerable".to_string(), vec![1]));
        assert_eq!(sets[1], ("Lane a with everything in it".to_string(), vec![0, 1]));
    }
}

#[cfg(test)]
mod judge_tests {
    use super::*;
    use crate::disk_space::ItemExtents;
    const G: u64 = 1 << 30;
    fn sized(alloc: u64, cut: bool) -> ItemResult {
        let mut r = ItemResult::default();
        r.level1 = disk_win::WalkStats { files: 10, dirs: 2, allocated: alloc, logical: alloc, cut_short: cut, ..Default::default() };
        r
    }

    #[test]
    fn an_item_the_time_never_reached_is_not_counted_never_zero() {
        let mut r = ItemResult::default();
        r.not_reached = true;
        r.level1.cut_short = true;
        let j = judge(&r, false, false, false);
        assert_eq!(j.conf, Confidence::Unknown);
        assert_eq!(j.frees, None);
        assert!(notes_for(&r, false)[0].starts_with("Not counted: the time ran out before it was reached"));
    }

    #[test]
    fn sized_only_without_block_cloning_frees_about_its_size() {
        let j = judge(&sized(3 * G, false), false, false, false);
        assert_eq!(j.conf, Confidence::Estimate);
        assert_eq!(j.frees, Some(3 * G));
        let j = judge(&sized(3 * G, true), false, false, false);
        assert_eq!(j.conf, Confidence::LowerBound);
        assert_eq!(j.frees, Some(3 * G));
    }

    #[test]
    fn sized_only_on_a_dev_drive_is_a_lower_bound_with_its_size_as_the_ceiling() {
        let j = judge(&sized(24 * G, false), true, false, false);
        assert_eq!(j.conf, Confidence::LowerBound);
        assert_eq!(j.frees, Some(0));
        assert_eq!(j.frees_max, Some(24 * G));
        let it = json!({ "frees_alone_gb": 0.0, "frees_max_gb": 24.0 });
        assert_eq!(qualified_item(&it, "lower_bound"), "up to 24.0 GB");
        assert_eq!(qualified(&Value::Null, "unknown"), "not counted");
        // Even while it is being written (in flux): never "about 0 GB".
        assert!(!ages_to_estimate(&json!({ "frees_confidence": "lower_bound", "frees_alone_gb": 0.0, "frees_max_gb": 24.0 }), "frees_alone_gb"));
        assert!(ages_to_estimate(&json!({ "frees_confidence": "lower_bound", "frees_alone_gb": 5.0 }), "frees_alone_gb"));
        assert!(ages_to_estimate(&json!({ "frees_confidence": "exact", "frees_alone_gb": 0.0 }), "frees_alone_gb"));
        let j = judge(&sized(24 * G, false), true, true, false);
        assert_eq!(j.conf, Confidence::LowerBound);
        assert_eq!(j.frees_max, Some(24 * G));
    }

    #[test]
    fn min_sharing_clusters_converts_1_gib_for_any_cluster_size() {
        assert_eq!(min_sharing_clusters(4096), 262_144);
        assert_eq!(min_sharing_clusters(65_536), 16_384);
        assert_eq!(min_sharing_clusters(512), 2_097_152);
        assert_eq!(min_sharing_clusters(0), 1 << 30);

        let a = ItemExtents { exclusive_clusters: 0, shared: vec![disk_space::Ext { lcn: 0, len: 20_000, rc: 2 }] };
        let b = ItemExtents { exclusive_clusters: 0, shared: vec![disk_space::Ext { lcn: 0, len: 20_000, rc: 2 }] };
        let sw = disk_space::sweep(&[a, b]);
        assert_eq!(sw.sharing_groups(min_sharing_clusters(65_536)).len(), 1);
        assert!(sw.sharing_groups(1 << 30).is_empty());

        let c = ItemExtents { exclusive_clusters: 0, shared: vec![disk_space::Ext { lcn: 0, len: 10_000, rc: 2 }] };
        let d = ItemExtents { exclusive_clusters: 0, shared: vec![disk_space::Ext { lcn: 0, len: 10_000, rc: 2 }] };
        let sw_small = disk_space::sweep(&[c, d]);
        assert!(sw_small.sharing_groups(min_sharing_clusters(65_536)).is_empty());
    }

    #[test]
    fn scanned_pct_accounts_for_hard_links_and_live_hard_links() {
        let mut r = ItemResult::default();
        r.level1 = disk_win::WalkStats { files: 100, ..Default::default() };
        r.level2 = Some(disk_win::WalkStats { files: 40, hard_links: 60, cut_short: false, ..Default::default() });
        assert_eq!(judge(&r, false, false, false).scanned_pct, 100.0);

        r.level2 = Some(disk_win::WalkStats { files: 20, hard_links: 30, cut_short: true, ..Default::default() });
        assert_eq!(judge(&r, false, false, false).scanned_pct, 50.0);

        r.level2 = Some(disk_win::WalkStats { files: 0, hard_links: 100, cut_short: false, ..Default::default() });
        assert_eq!(judge(&r, false, false, false).scanned_pct, 100.0);

        // Live temp dir with hard links across two files in an item.
        let root = std::env::temp_dir().join(format!("amg-scan-hl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let inter = root.join(r"proj\Intermediate");
        std::fs::create_dir_all(&inter).unwrap();
        std::fs::write(root.join(r"proj\Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();
        std::fs::write(inter.join("a.bin"), vec![9u8; 65_536]).unwrap();
        std::fs::hard_link(inter.join("a.bin"), inter.join("b.bin")).unwrap();
        let cfg = Config::builtin();
        let items = discover(&cfg, Some(&root.display().to_string()));
        let job = ScanJob { items: items.iter().map(|i| i.paths.clone()).collect(), sets: vec![], threads: 2, budget_sec: 30, stop_on_build: false };
        let out = run_job(&job, &mut |_, _| {});
        let rep = build_checkup(&items, &out, &[], &cfg.disk(), 0, false, &[], "now");
        assert_eq!(rep.items[0]["scanned_pct"], 100.0);
        assert_eq!(out.items[0].level2.as_ref().unwrap().files, 1);
        assert_eq!(out.items[0].level2.as_ref().unwrap().hard_links, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sized_only_ntfs_reports_allocated_footprint_and_refs_reports_null() {
        let cfg = Config::builtin();
        let items = vec![CatItem { key: "ue:intermediate".into(), class: "regenerable".into(), title: "Inter".into(), paths: vec![r"D:\p\Intermediate".into()], lane: None }];
        let mut r_ntfs = sized(3 * G, false);
        r_ntfs.volume = Some("vol-ntfs".into());
        let out_ntfs = ScanOutput {
            items: vec![r_ntfs],
            volumes: vec![VolumeResult { guid: "vol-ntfs".into(), root: r"D:\".into(), fs: "NTFS".into(), block_cloning: false, total: 100 * G, avail: 50 * G, free: 50 * G }],
            ..Default::default()
        };
        let rep_ntfs = build_checkup(&items, &out_ntfs, &[], &cfg.disk(), 0, false, &[], "now");
        assert_eq!(rep_ntfs.items[0]["footprint_gb"], 3.0);
        assert_eq!(rep_ntfs.items[0]["frees_alone_gb"], 3.0);
        assert!(rep_ntfs.items[0]["stays_shared_gb"].is_null());
        assert!(rep_ntfs.items[0]["shared_outside_scan_gb"].is_null());

        let mut r_refs = sized(24 * G, false);
        r_refs.volume = Some("vol-refs".into());
        let out_refs = ScanOutput {
            items: vec![r_refs],
            volumes: vec![VolumeResult { guid: "vol-refs".into(), root: r"E:\".into(), fs: "ReFS".into(), block_cloning: true, total: 100 * G, avail: 50 * G, free: 50 * G }],
            ..Default::default()
        };
        let rep_refs = build_checkup(&items, &out_refs, &[], &cfg.disk(), 0, false, &[], "now");
        assert!(rep_refs.items[0]["footprint_gb"].is_null());
        assert_eq!(rep_refs.items[0]["frees_alone_gb"], 0.0);
        assert_eq!(rep_refs.items[0]["frees_max_gb"], 24.0);
        assert_eq!(rep_refs.items[0]["frees_confidence"], "lower_bound");
    }

    #[test]
    fn dir_failures_inconsistent_and_unknown_rc_downgrade_per_item() {
        // L2 dir_failures > 0 -> LowerBound + note.
        let mut r = ItemResult::default();
        r.level1 = disk_win::WalkStats { files: 10, ..Default::default() };
        r.level2 = Some(disk_win::WalkStats { files: 10, dir_failures: 1, ..Default::default() });
        let j = judge(&r, false, false, false);
        assert_eq!(j.conf, Confidence::LowerBound);
        assert!(notes_for(&r, false).iter().any(|n| n.contains("1 files or folders couldn't be read")));

        // Sized-only NTFS with l1.dir_failures > 0 -> LowerBound and frees_max = None.
        let mut r_sized = sized(2 * G, false);
        r_sized.level1.dir_failures = 1;
        let j_sized = judge(&r_sized, false, false, false);
        assert_eq!(j_sized.conf, Confidence::LowerBound);
        assert_eq!(j_sized.frees_max, None);

        // Inconsistent -> Estimate; unknown_rc on block_cloning -> UpperBound; clean sibling -> Exact.
        let mut r_inc = ItemResult::default();
        r_inc.level1 = disk_win::WalkStats { files: 1, allocated: 4096, ..Default::default() };
        r_inc.level2 = Some(r_inc.level1.clone());
        r_inc.inconsistent = true;
        assert_eq!(judge(&r_inc, true, false, false).conf, Confidence::Estimate);

        let mut r_unk = ItemResult::default();
        r_unk.level1 = disk_win::WalkStats { files: 1, allocated: 4096, ..Default::default() };
        r_unk.level2 = Some(r_unk.level1.clone());
        r_unk.unknown_rc = true;
        assert_eq!(judge(&r_unk, true, false, false).conf, Confidence::UpperBound);

        let mut r_clean = ItemResult::default();
        r_clean.level1 = disk_win::WalkStats { files: 1, allocated: 4096, ..Default::default() };
        r_clean.level2 = Some(r_clean.level1.clone());
        assert_eq!(judge(&r_clean, true, false, false).conf, Confidence::Exact);
    }

    #[test]
    fn judge_never_reports_100_pct_when_level1_cut_short() {
        let mut r = ItemResult::default();
        r.level1 = disk_win::WalkStats { files: 50, cut_short: true, ..Default::default() };
        r.level2 = Some(disk_win::WalkStats { files: 50, cut_short: true, ..Default::default() });
        let j = judge(&r, false, false, false);
        assert_eq!(j.scanned_pct, 0.0);
        assert_eq!(j.conf, Confidence::LowerBound);
    }

    #[test]
    fn coordinator_guard_decrements_and_notifies_on_panic() {
        let remaining = AtomicUsize::new(2);
        let done_cv = (Mutex::new(()), Condvar::new());
        let rem_ref = &remaining;
        let cv_ref = &done_cv;
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let h1 = s.spawn(move || {
                let _guard = CoordinatorGuard(rem_ref, cv_ref);
                panic!("simulated disk coordinator panic");
            });
            let h2 = s.spawn(move || {
                let _guard = CoordinatorGuard(rem_ref, cv_ref);
            });
            loop {
                if rem_ref.load(Ordering::Acquire) == 0 {
                    break;
                }
                let g = cv_ref.0.lock().unwrap_or_else(|p| p.into_inner());
                if rem_ref.load(Ordering::Acquire) == 0 {
                    break;
                }
                let _ = cv_ref.1.wait_timeout(g, Duration::from_secs(5)).unwrap_or_else(|p| p.into_inner());
            }
            assert!(t0.elapsed() < Duration::from_secs(2), "coordinator guard failed to wake waiter");
            assert!(h1.join().is_err());
            assert!(h2.join().is_ok());
        });
        assert_eq!(remaining.load(Ordering::Acquire), 0);
    }

    #[test]
    fn find_roots_discovers_git_and_standalone_engine_and_prunes_target() {
        let root = std::env::temp_dir().join(format!("amg-scan-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let git_repo = root.join("non_ue_git");
        let ue_src = root.join("ue_source");
        let rust_repo = root.join("rust_app");
        std::fs::create_dir_all(git_repo.join(".git")).unwrap();
        std::fs::create_dir_all(ue_src.join(r"Engine\Build")).unwrap();
        std::fs::write(ue_src.join(r"Engine\Build\Build.version"), b"{}").unwrap();
        std::fs::create_dir_all(ue_src.join(r"Engine\Intermediate")).unwrap();
        std::fs::create_dir_all(rust_repo.join(r"target\nested\.git")).unwrap();
        std::fs::write(rust_repo.join("Cargo.toml"), b"[package]\nname=\"x\"\n").unwrap();

        let found = find_roots(&root, 3);
        let paths: Vec<String> = found.iter().map(|r| norm(&r.path.display().to_string())).collect();
        assert!(paths.contains(&norm(&git_repo.display().to_string())), "{:?}", paths);
        assert!(paths.contains(&norm(&ue_src.join("Engine").display().to_string())), "{:?}", paths);
        assert!(paths.contains(&norm(&rust_repo.display().to_string())), "{:?}", paths);
        assert!(!paths.iter().any(|p| p.contains(r"\target\nested")), "target must be pruned: {:?}", paths);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relative_worktree_vs_submodule_and_lfs_parser() {
        let root = std::env::temp_dir().join(format!("amg-scan-wt-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let main_repo = root.join(r"ws\MainRepo");
        let wt_admin = main_repo.join(r".git\worktrees\LaneRel");
        let lane = root.join(r"elsewhere\LaneRel");
        let sub = main_repo.join(r"Plugins\MySub");
        let sub_admin = main_repo.join(r".git\modules\Plugins\MySub");
        std::fs::create_dir_all(&wt_admin).unwrap();
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&sub_admin).unwrap();

        std::fs::write(wt_admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt_admin.join("gitdir"), "../../../../../elsewhere/LaneRel/.git\n").unwrap();
        std::fs::write(lane.join(".git"), "gitdir: ../../ws/MainRepo/.git/worktrees/LaneRel\n").unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../../.git/modules/Plugins/MySub\n").unwrap();

        let resolved = resolve_gitdir_target(&wt_admin, "../../../../../elsewhere/LaneRel/.git\n").unwrap();
        assert_eq!(norm(&resolved.display().to_string()), norm(&lane.display().to_string()));
        assert_eq!(worktrees_of(&main_repo).len(), 1);

        let orig = resolve_worktree_links(&lane).unwrap();
        assert_eq!(norm(&orig.main_repo.display().to_string()), norm(&main_repo.display().to_string()));
        assert!(!orig.is_bare);
        assert!(is_worktree(&lane));
        // Submodule has no `commondir`: must NOT be treated as a worktree!
        assert!(resolve_worktree_links(&sub).is_none());
        assert!(!is_worktree(&sub));

        let lfs_out = "prune: 5 local objects, 2 retained, pruning 3 files (12.5 MB)\n* 0123abcd (4.0 MB)\n";
        let est = parse_lfs_prune_dry_run(lfs_out);
        assert_eq!(est.retained_objects, Some(2));
        assert_eq!(est.prunable_objects, Some(3));
        assert!(est.reclaimable_bytes.unwrap() >= (12 << 20));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn expanded_project_global_and_loose_temp_entries() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("amg-scan-expanded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("Proj");
        std::fs::create_dir_all(proj.join(r".vs\Proj\v17\ipch")).unwrap();
        std::fs::create_dir_all(proj.join(r"Binaries\Win64")).unwrap();
        std::fs::create_dir_all(proj.join("target")).unwrap();
        std::fs::write(proj.join("Cargo.toml"), b"[package]\nname=\"p\"\n").unwrap();
        let pdb = proj.join(r"Binaries\Win64\Old.pdb");
        std::fs::write(&pdb, vec![1u8; 4096]).unwrap();
        let old_time = std::time::SystemTime::now() - Duration::from_secs(10 * 86400);
        std::fs::OpenOptions::new().access_mode(0x100).open(&pdb).unwrap().set_modified(old_time).unwrap();
        std::fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(proj.join("target")).unwrap().set_modified(old_time).unwrap();

        let items = project_items(&proj, "Proj", None);
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert!(keys.contains(&"vs:ipch"), "{:?}", keys);
        assert!(keys.contains(&"ue:pdb"), "{:?}", keys);
        assert!(keys.contains(&"rust:target"), "{:?}", keys);
        let pdb_item = items.iter().find(|i| i.key == "ue:pdb").unwrap();
        assert_eq!(pdb_item.paths, vec![pdb.display().to_string()]);

        // Loose file in old_entries + wildcard expand_global_paths + protected root rejection.
        let tmp_dir = root.join("temp_dir");
        std::fs::create_dir_all(tmp_dir.join("Rider2025.1")).unwrap();
        let loose = tmp_dir.join("loose.tmp");
        std::fs::write(&loose, b"x").unwrap();
        std::fs::OpenOptions::new().access_mode(0x100).open(&loose).unwrap().set_modified(old_time).unwrap();
        let olds = old_entries(&tmp_dir, 7);
        assert_eq!(olds, vec![loose.display().to_string()]);

        assert!(expand_global(r"C:\").is_none(), "drive root must be rejected");
        std::env::set_var("AMG_TEST_SCAN_LOCAL", &tmp_dir);
        let wc = expand_global_paths(r"%AMG_TEST_SCAN_LOCAL%\Rider*");
        assert_eq!(wc, vec![tmp_dir.join("Rider2025.1")]);
        std::env::remove_var("AMG_TEST_SCAN_LOCAL");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_record_never_walks_no_walk_items_like_winsxs() {
        let root = std::env::temp_dir().join(format!("amg-scan-plan-nowalk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let inter = root.join(r"proj\Intermediate");
        std::fs::create_dir_all(&inter).unwrap();
        std::fs::write(root.join(r"proj\Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();
        std::fs::write(inter.join("a.bin"), vec![1u8; 4096]).unwrap();

        let cfg = Config::builtin();
        let items = vec![
            CatItem { key: "sys:winsxs".into(), class: "review".into(), title: "Windows component store (WinSxS)".into(), paths: vec![r"C:\Windows\WinSxS".into()], lane: None },
            CatItem { key: "ue:intermediate".into(), class: "regenerable".into(), title: "Intermediate".into(), paths: vec![inter.display().to_string()], lane: None },
        ];
        let cached = CachedCheckup {
            id: "c1".into(),
            scope: "*".into(),
            unix: unix_now(),
            time: String::new(),
            items,
            root_ids: vec![],
            last_writes: vec![],
            report: Checkup { scan: json!({}), volumes: vec![], items: vec![], sharing_groups: vec![], totals: vec![], basis: BASIS.into(), do_not_sum: DO_NOT_SUM.into() },
        };
        let run = |j: &ScanJob, p: Progress| {
            assert!(j.items[0].is_empty(), "plan_record must pass empty paths for no_walk item sys:winsxs: {:?}", j.items[0]);
            assert!(!j.items[1].is_empty());
            Ok(run_job(j, p))
        };
        let (v, rec) = plan_record(&cfg, &cached, &[], None, &run, &mut |_, _| {}).unwrap();
        assert!(rec.items[0].unmeasurable.as_deref().is_some_and(|u| u.contains("DISM")), "{:?}", rec.items[0].unmeasurable);
        assert!(v["refused"].as_array().is_some_and(|arr| arr.iter().any(|r| r["id"] == "d1")), "{}", v);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_prunes_kept_roots_and_lanes_early() {
        let root = std::env::temp_dir().join(format!("amg-scan-keep-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let kept_repo = root.join("kept_repo");
        let active_proj = root.join("active_proj");
        std::fs::create_dir_all(kept_repo.join(r".git\lfs\objects\aa")).unwrap();
        std::fs::create_dir_all(active_proj.join("Intermediate")).unwrap();
        std::fs::write(active_proj.join("Game.uproject"), br#"{ "FileVersion": 3 }"#).unwrap();

        let toml = format!(
            "[disk]\nproject_dirs = ['{}']\nkeep = ['{}']\nscan_depth = 3\n",
            root.display(),
            kept_repo.display()
        );
        let cfg = Config::parse(&toml).unwrap();
        let found = discover(&cfg, Some(&root.display().to_string()));
        assert!(!found.iter().any(|i| i.paths.iter().any(|p| p.to_ascii_lowercase().contains("kept_repo"))), "{:?}", found);
        assert!(found.iter().any(|i| i.key == "ue:intermediate"), "{:?}", found);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_lfs_emits_once_and_worktree_merged_distinguishes_unmerged_without_origin_head() {
        let root = std::env::temp_dir().join(format!("amg-scan-lfs-merged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let main_repo = root.join(r"repos\Main");
        let wt_unmerged = root.join(r"lanes\wt_unmerged");
        let wt_merged = root.join(r"lanes\wt_merged");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(root.join("lanes")).unwrap();

        let git = |repo: &Path, args: &[&str]| {
            let rs = repo.display().to_string();
            let mut full = vec!["-C", &rs];
            full.extend_from_slice(args);
            let (code, out) = run_git_scan(&full, Duration::from_secs(10)).expect("git command should run");
            assert_eq!(code, 0, "git {:?} in {} failed: {}", args, rs, out);
        };

        git(&main_repo, &["init", "-q", "-b", "main"]);
        git(&main_repo, &["config", "user.name", "T"]);
        git(&main_repo, &["config", "user.email", "t@e.com"]);
        std::fs::write(main_repo.join("a.txt"), "a\n").unwrap();
        git(&main_repo, &["add", "a.txt"]);
        git(&main_repo, &["commit", "-q", "-m", "init"]);
        std::fs::create_dir_all(main_repo.join(r".git\lfs\objects")).unwrap();

        let wt_merged_s = wt_merged.display().to_string();
        git(&main_repo, &["-c", "worktree.useRelativePaths=true", "worktree", "add", "-b", "feat/merged", &wt_merged_s, "-q"]);
        std::fs::write(wt_merged.join("m.txt"), "m\n").unwrap();
        git(&wt_merged, &["add", "m.txt"]);
        git(&wt_merged, &["commit", "-q", "-m", "merged work"]);
        git(&main_repo, &["merge", "--no-ff", "-q", "-m", "merge feat/merged", "feat/merged"]);

        let wt_unmerged_s = wt_unmerged.display().to_string();
        git(&main_repo, &["-c", "worktree.useRelativePaths=true", "worktree", "add", "-b", "feat/unmerged", &wt_unmerged_s, "-q"]);
        std::fs::write(wt_unmerged.join("b.txt"), "b\n").unwrap();
        git(&wt_unmerged, &["add", "b.txt"]);
        git(&wt_unmerged, &["commit", "-q", "-m", "unmerged work"]);

        let cfg = Config::builtin();
        let (items, extras) = discover_detailed(&cfg, Some(&root.display().to_string()));
        let lfs_indices: Vec<usize> = items.iter().enumerate().filter(|(_, i)| i.key == "git:lfs").map(|(idx, _)| idx).collect();
        assert_eq!(lfs_indices.len(), 1, "expected exactly 1 git:lfs item, got: {:?}", items);
        assert!(extras[lfs_indices[0]].lfs.is_some(), "single git:lfs item must carry ItemExtra.lfs");

        let sets = checkup_sets(&items);
        let job = ScanJob {
            items: items.iter().map(|i| i.paths.clone()).collect(),
            sets: sets.iter().map(|s| s.1.clone()).collect(),
            threads: 2,
            budget_sec: 30,
            stop_on_build: false,
        };
        let out = run_job(&job, &mut |_, _| {});
        let rep = build_checkup_with_extras(&items, &extras, &out, &sets, &cfg.disk(), 0, false, &[], "now");

        let lfs_rep: Vec<&Value> = rep.items.iter().filter(|i| i["key"] == "git:lfs").collect();
        assert_eq!(lfs_rep.len(), 1, "report must have 1 git:lfs item: {:?}", lfs_rep);
        assert!(lfs_rep[0]["git"].is_object(), "git:lfs report item must include git metadata: {}", lfs_rep[0]);

        let lane_unmerged = rep
            .items
            .iter()
            .find(|i| i["key"] == "lane" && i["worktree"]["branch"] == "feat/unmerged")
            .expect("feat/unmerged lane item");
        assert_eq!(lane_unmerged["worktree"]["merged"], json!(false), "{}", lane_unmerged);

        let lane_merged = rep
            .items
            .iter()
            .find(|i| i["key"] == "lane" && i["worktree"]["branch"] == "feat/merged")
            .expect("feat/merged lane item");
        assert_eq!(lane_merged["worktree"]["merged"], json!(true), "{}", lane_merged);

        // Reverse discovery starting at wt_unmerged alone must also emit 1 git:lfs and merged=false.
        let (rev_items, rev_extras) = discover_detailed(&cfg, Some(&wt_unmerged_s));
        let rev_lfs: Vec<usize> = rev_items.iter().enumerate().filter(|(_, i)| i.key == "git:lfs").map(|(idx, _)| idx).collect();
        assert_eq!(rev_lfs.len(), 1, "reverse discovery expected 1 git:lfs, got: {:?}", rev_items);
        assert!(rev_extras[rev_lfs[0]].lfs.is_some());
        let rev_lane = rev_items
            .iter()
            .enumerate()
            .find(|(_, i)| i.key == "lane")
            .map(|(idx, _)| &rev_extras[idx])
            .expect("reverse-discovered lane");
        assert_eq!(rev_lane.lane.as_ref().and_then(|l| l.merged), Some(false));

        // Master-only fallback (no origin/HEAD, no main) + unborn repo (no commits -> None).
        let master_repo = root.join(r"repos\MasterOnly");
        let wt_master = root.join(r"lanes\wt_master");
        std::fs::create_dir_all(&master_repo).unwrap();
        git(&master_repo, &["init", "-q", "-b", "master"]);
        git(&master_repo, &["config", "user.name", "T"]);
        git(&master_repo, &["config", "user.email", "t@e.com"]);
        std::fs::write(master_repo.join("a.txt"), "a\n").unwrap();
        git(&master_repo, &["add", "a.txt"]);
        git(&master_repo, &["commit", "-q", "-m", "init"]);
        let wt_master_s = wt_master.display().to_string();
        git(&master_repo, &["worktree", "add", "-b", "feat/ahead", &wt_master_s, "-q"]);
        std::fs::write(wt_master.join("c.txt"), "c\n").unwrap();
        git(&wt_master, &["add", "c.txt"]);
        git(&wt_master, &["commit", "-q", "-m", "ahead"]);
        let meta_master = gather_lane_meta(&wt_master, Some(&master_repo), Instant::now() + Duration::from_secs(5));
        assert_eq!(meta_master.merged, Some(false), "{:?}", meta_master);

        let unborn = root.join(r"repos\Unborn");
        std::fs::create_dir_all(&unborn).unwrap();
        git(&unborn, &["init", "-q", "-b", "main"]);
        let meta_unborn = gather_lane_meta(&unborn, Some(&unborn), Instant::now() + Duration::from_secs(5));
        assert_eq!(meta_unborn.merged, None, "{:?}", meta_unborn);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn t5_bare_repo_relative_worktree_dirty_and_unpushed_counts() {
        let root = std::env::temp_dir().join(format!("amg-t5-bare-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bare = root.join("bare.git");
        let wt = root.join("lane-feat");
        std::fs::create_dir_all(&bare).unwrap();
        let git = |args: &[&str]| crate::disk_clean::run_cmd(Path::new("git"), args, Duration::from_secs(20), &[]);
        if git(&["init", "--bare", "-q", &bare.display().to_string()]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        // Seed initial commit via temp clone, push to bare, add relative worktree
        let seed = root.join("seed");
        if git(&["clone", "-q", &bare.display().to_string(), &seed.display().to_string()]).is_ok() {
            let _ = git(&["-C", &seed.display().to_string(), "config", "user.email", "t@example.com"]);
            let _ = git(&["-C", &seed.display().to_string(), "config", "user.name", "Test"]);
            std::fs::write(seed.join("README.md"), b"hi").unwrap();
            let _ = git(&["-C", &seed.display().to_string(), "add", "README.md"]);
            if git(&["-C", &seed.display().to_string(), "commit", "-q", "-m", "init"]).is_ok() {
                let _ = git(&["-C", &seed.display().to_string(), "push", "-q", "origin", "HEAD"]);
                let _ = git(&["-C", &bare.display().to_string(), "worktree", "add", "--relative-paths", "../lane-feat", "-b", "feat"]);
                if wt.is_dir() {
                    let _ = git(&["-C", &wt.display().to_string(), "config", "user.email", "t@example.com"]);
                    let _ = git(&["-C", &wt.display().to_string(), "config", "user.name", "Test"]);
                    std::fs::write(wt.join("new.txt"), b"1").unwrap();
                    let _ = git(&["-C", &wt.display().to_string(), "add", "new.txt"]);
                    let _ = git(&["-C", &wt.display().to_string(), "commit", "-q", "-m", "unpushed"]);
                    std::fs::write(wt.join("dirty.txt"), b"untracked").unwrap();

                    let orig = resolve_worktree_links(&wt).expect("relative worktree resolves to bare repo");
                    assert!(orig.is_bare);
                    let meta = gather_lane_meta(&wt, Some(&orig.main_repo), Instant::now() + Duration::from_secs(10));
                    assert_eq!(meta.branch.as_deref(), Some("feat"));
                    assert_eq!(meta.dirty, Some(true));
                    assert!(meta.dirty_files.unwrap_or(0) >= 1);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

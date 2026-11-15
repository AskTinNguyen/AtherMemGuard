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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Config, DiskSettings};
use crate::disk_space::{self, gb2, norm, total_confidence, Confidence, ItemExtents, ScanFacts};
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
const NO_DESCEND: &[&str] = &["content", "intermediate", "saved", "binaries", "deriveddatacache", "plugins", "source", "node_modules", "build", "config", "$recycle.bin", "system volume information"];

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
    /// The tool whose own clean command can empty it (`npm`, `pip`, `dotnet`).
    #[serde(default)]
    tool: Option<String>,
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
}

pub fn catalog_info(key: &str) -> Option<CatalogInfo> {
    let c = catalog();
    let f = |e: &CatalogEntry, global: bool| CatalogInfo { class: e.class.clone(), path: e.path.clone(), plugins: e.plugins, global, older_than_days: e.older_than_days, tool: e.tool.clone() };
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

/// Does `dir` hold a `.uproject` file directly?
pub fn has_uproject(dir: &Path) -> bool {
    !uprojects(dir).is_empty()
}

/// The `.uproject` file names directly in `dir`.
fn uprojects(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.to_ascii_lowercase().ends_with(".uproject").then_some(n)
        })
        .collect()
}

/// Unreal project roots at or under `root`, at most `depth` levels down. Reparse points,
/// hidden folders and big Unreal folders are not searched; a project's own subfolders aren't.
pub fn find_projects(root: &Path, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut level = vec![root.to_path_buf()];
    for d in 0..=depth {
        let mut next = Vec::new();
        for dir in level {
            if d > 0 && is_reparse(&dir) {
                continue;
            }
            if !uprojects(&dir).is_empty() {
                out.push(dir);
                continue;
            }
            if d == depth {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                if n.starts_with('.') || n.starts_with('$') || NO_DESCEND.contains(&n.as_str()) {
                    continue;
                }
                let p = e.path();
                if is_dir(&p) {
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

/// A git worktree: `.git` is a file (`gitdir: ...`), not a folder.
pub fn is_worktree(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).map(|m| m.is_file()).unwrap_or(false)
}

/// The worktrees git knows for the repo at `root` (from `.git\worktrees\*\gitdir`).
pub fn worktrees_of(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root.join(r".git\worktrees")) else { return out };
    for e in rd.flatten() {
        if let Ok(text) = std::fs::read_to_string(e.path().join("gitdir")) {
            if let Some(p) = parse_gitdir_target(&text) {
                if is_dir(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
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

/// The catalogue's items inside one project root.
fn project_items(root: &Path, project: &str, lane: Option<&str>) -> Vec<CatItem> {
    let mut out = Vec::new();
    for e in &catalog().project {
        let paths: Vec<String> = if e.plugins {
            let mut v = Vec::new();
            plugin_dirs(&root.join("Plugins"), &e.path, 3, &mut v);
            v
        } else {
            let p = root.join(&e.path);
            if is_dir(&p) {
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

/// A `[[global]]` catalogue path with its variable filled in.
pub fn expand_global(path: &str) -> Option<PathBuf> {
    let (ddc, zen) = crate::disk::ue_cache_dirs();
    let s = if let Some(rest) = path.strip_prefix("%DDC%") {
        format!("{}{}", ddc?.display(), rest)
    } else if let Some(rest) = path.strip_prefix("%ZEN%") {
        format!("{}{}", zen?.display(), rest)
    } else if let Some(rest) = path.strip_prefix("%TEMP%") {
        format!("{}{}", std::env::temp_dir().display().to_string().trim_end_matches('\\'), rest)
    } else if let Some(rest) = path.strip_prefix("%LOCALAPPDATA%") {
        format!("{}{}", std::env::var("LOCALAPPDATA").ok()?, rest)
    } else {
        path.to_string()
    };
    Some(PathBuf::from(s))
}

fn global_items() -> Vec<CatItem> {
    let mut out = Vec::new();
    for e in &catalog().global {
        let Some(p) = expand_global(&e.path) else { continue };
        if !is_dir(&p) {
            continue;
        }
        let paths = match e.older_than_days {
            Some(days) => old_entries(&p, days),
            None => vec![p.display().to_string()],
        };
        if !paths.is_empty() {
            out.push(CatItem { key: e.key.clone(), class: e.class.clone(), title: e.title.clone(), paths, lane: None });
        }
    }
    out
}

/// Top-level folders of `dir` not written for `days` days. Loose files at the top of TEMP are
/// not listed: the scanner walks folders (a known gap, usually small).
fn old_entries(dir: &Path, days: u64) -> Vec<String> {
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(days * 86400);
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| is_dir(&e.path()))
        .filter(|e| e.metadata().and_then(|m| m.modified()).map(|t| t < cutoff).unwrap_or(false))
        .map(|e| e.path().display().to_string())
        .collect()
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
fn bin_items() -> Vec<CatItem> {
    let mut out = Vec::new();
    for v in disk_win::volumes().iter().filter(|v| v.drive_type == 3) {
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
    let s = cfg.disk();
    let mut items: Vec<CatItem> = Vec::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    match path {
        Some(p) => roots.push(PathBuf::from(p)),
        None => {
            roots.extend(s.project_dirs.iter().map(PathBuf::from));
            for v in disk_win::volumes().iter().filter(|v| v.dev_volume == Some(true)) {
                if let Some(r) = v.paths.first() {
                    roots.push(PathBuf::from(r));
                }
            }
        }
    }
    let mut projects: Vec<PathBuf> = Vec::new();
    for r in &roots {
        for p in find_projects(r, s.scan_depth) {
            if !projects.iter().any(|q| norm(&q.display().to_string()) == norm(&p.display().to_string())) {
                projects.push(p);
            }
        }
    }
    let listed = |p: &Path, projects: &[PathBuf]| projects.iter().any(|q| norm(&q.display().to_string()) == norm(&p.display().to_string()));
    let mut lanes_seen: Vec<String> = Vec::new();
    for root in &projects {
        let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if is_worktree(root) {
            // A lane asked for (or found) by itself: the lane, and its own items inside it.
            let lane = root.display().to_string();
            lanes_seen.push(norm(&lane));
            items.push(CatItem { key: "lane".into(), class: "review".into(), title: lane_title(root), paths: vec![lane.clone()], lane: None });
            items.extend(project_items(root, &name, Some(&lane)));
        } else {
            items.extend(project_items(root, &name, None));
            // Lanes of this project: git worktrees and same-named siblings, listed whole.
            let up = uprojects(root);
            let mut lanes = worktrees_of(root);
            let mut dirs: Vec<PathBuf> = root.parent().map(|p| vec![p.to_path_buf()]).unwrap_or_default();
            dirs.extend(s.lane_dirs.iter().map(PathBuf::from));
            for d in dirs {
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for e in rd.flatten() {
                        let p = e.path();
                        if is_dir(&p) && uprojects(&p).iter().any(|u| up.iter().any(|x| x.eq_ignore_ascii_case(u))) {
                            lanes.push(p);
                        }
                    }
                }
            }
            for l in lanes {
                let n = norm(&l.display().to_string());
                if n == norm(&root.display().to_string()) || lanes_seen.contains(&n) || listed(&l, &projects) {
                    continue;
                }
                lanes_seen.push(n);
                items.push(CatItem { key: "lane".into(), class: "review".into(), title: lane_title(&l), paths: vec![l.display().to_string()], lane: None });
            }
        }
    }
    if path.is_none() {
        items.extend(global_items());
        items.extend(old_cache_items());
        items.extend(bin_items());
    }
    let keep: Vec<String> = s.keep.iter().map(|k| norm(k)).collect();
    for it in &mut items {
        it.paths.retain(|p| !keep.iter().any(|k| disk_space::is_within(&norm(p), k)));
    }
    items.retain(|i| !i.paths.is_empty());
    items.sort_by_key(|i| class_rank(&i.class));
    items
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
    pub dropped_paths: Vec<String>,
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
pub struct ScanOutput {
    pub took_sec: f64,
    pub partial: bool,
    pub stopped_for_build: bool,
    pub started_ft: u64,
    pub items: Vec<ItemResult>,
    pub sets: Vec<SetResult>,
    pub groups: Vec<GroupResult>,
    pub volumes: Vec<VolumeResult>,
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
    let mut vol_scans: Vec<(String, disk_win::VolumeScan, disk_win::VolumeFacts)> = Vec::new();
    for g in by_vol.keys() {
        let paths = disk_win::volumes().into_iter().find(|v| v.guid.eq_ignore_ascii_case(g)).map(|v| v.paths).unwrap_or_default();
        let facts = disk_win::volume_facts(g, paths);
        let vs = disk_win::VolumeScan {
            cluster: facts.cluster,
            block_cloning: facts.fs_flags & disk_win::FS_BLOCK_REFCOUNTING != 0,
            level2: false,
            threads: job.threads.clamp(1, 64),
            deadline,
            stop: stop.clone(),
            started_ft,
            multi: Mutex::new(HashMap::new()),
            progress_files: files.clone(),
        };
        out.volumes.push(VolumeResult {
            guid: g.clone(),
            root: facts.paths.first().cloned().unwrap_or_else(|| g.clone()),
            fs: facts.fs.clone(),
            block_cloning: vs.block_cloning,
            total: facts.total,
            avail: facts.avail,
            free: facts.free,
        });
        vol_scans.push((g.clone(), vs, facts));
    }
    // Level 1 everywhere first: logical and allocated sizes are cheap and complete.
    for (g, vs, _) in &vol_scans {
        for &i in &by_vol[g] {
            let (st, _) = disk_win::walk_item(vs, i, &roots[i], &skips[i]);
            out.items[i].level1 = st;
            tick(&files, progress);
        }
    }
    // Level 2 per volume, in the job's order (class first, then larger items first), then sweep.
    for (g, mut vs, _) in vol_scans {
        vs.level2 = true;
        let mut order = by_vol[&g].clone();
        order.sort_by_key(|&i| i); // job order: the job is sorted by class
        let mut exts: Vec<ItemExtents> = vec![ItemExtents::default(); n];
        for &i in &order {
            if stop.load(Ordering::Relaxed) || Instant::now() > deadline {
                break;
            }
            let (st, ex) = disk_win::walk_item(&vs, i, &roots[i], &skips[i]);
            out.items[i].level2 = Some(st);
            exts[i] = ex;
            tick(&files, progress);
        }
        let multi = std::mem::take(&mut *vs.multi.lock().unwrap());
        disk_win::attribute_multilinks(multi, &mut exts, vs.block_cloning);
        let ids: Vec<usize> = by_vol[&g].clone();
        let local: Vec<ItemExtents> = ids.iter().map(|&i| std::mem::take(&mut exts[i])).collect();
        let sw = disk_space::sweep(&local);
        let c = vs.cluster;
        for (k, &i) in ids.iter().enumerate() {
            let sp = &sw.items[k];
            let r = &mut out.items[i];
            r.footprint = sp.footprint * c;
            r.frees_alone = sp.frees_alone * c;
            r.shared_outside = sp.shared_outside * c;
            r.shared_with = sp.shared_with.iter().map(|(j, v)| (ids[*j], v * c)).collect();
            r.anomalies = sw.anomalies > 0;
        }
        for (si, set) in job.sets.iter().enumerate() {
            let local_set: Vec<usize> = set.iter().filter_map(|i| ids.iter().position(|x| x == i)).collect();
            if !local_set.is_empty() {
                out.sets.push(SetResult { set: si, items: local_set.iter().map(|k| ids[*k]).collect(), volume: g.clone(), frees_together: sw.frees_together(&local_set) * c });
            }
        }
        for grp in sw.sharing_groups(1 << 30).into_iter().take(12) {
            out.groups.push(GroupResult { items: grp.items.iter().map(|k| ids[*k]).collect(), shared: grp.shared * c, frees_if_all_removed: grp.frees_if_all_removed * c });
        }
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
        let _ = std::fs::create_dir_all(crate::paths::data_dir());
        let tmp = cache_file().with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string(c).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, cache_file());
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

/// Project folders an Unreal editor, build or cook is working in (normalised), and whether
/// some Unreal process's command line couldn't be read (then nobody can say which project).
pub fn busy_state() -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut unknown = false;
    for p in crate::win::list_processes() {
        let n = p.name.to_ascii_lowercase();
        if !(n.starts_with("unrealeditor") || n == "unrealbuildtool" || n == "automationtool" || n.starts_with("unrealpak")) {
            continue;
        }
        match crate::win::command_line(p.pid) {
            Some(cmd) => out.extend(uproject_dirs_in(&cmd)),
            None => unknown = true,
        }
    }
    (out, unknown)
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
        unmeasurable: r.unmeasurable.is_some(),
        block_cloning,
        files_without_refcount: l2.map_or(0, |l| l.files_without_refcount),
        partial: l2.map_or(true, |l| l.cut_short) || l1.cut_short,
        failures: l2.map_or(l1.dir_failures, |l| l.dir_failures + l.open_failures + l.extent_failures),
        in_flux,
        anomalies: r.anomalies,
        stale,
    };
    let conf = disk_space::confidence(&facts);
    let scanned_pct = match l2 {
        Some(l) if l1.files > 0 => ((l.files as f64 / l1.files as f64 * 100.0).min(100.0) * 10.0).round() / 10.0,
        Some(_) => 100.0,
        None => 0.0,
    };
    let (frees, frees_max) = if conf == Confidence::Unknown {
        (None, None)
    } else {
        let proven_shared = r.footprint - r.frees_alone;
        let ceiling = if facts.partial { l1.allocated.saturating_sub(proven_shared).max(r.frees_alone) } else { r.frees_alone };
        (Some(r.frees_alone), Some(ceiling))
    };
    Judged { conf, frees, frees_max, scanned_pct, in_flux }
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
    if in_use {
        v.push("Unreal is working in this project".into());
    }
    if r.level2.is_none() && r.unmeasurable.is_none() {
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
        jitems.push(json!({
            "id": ids[k], "key": it.key, "class": it.class, "title": it.title,
            "volume": r.volume.as_deref().map(|g| vol_name(g, &out.volumes)),
            "paths": it.paths, "excludes": excludes, "files": files,
            "logical_gb": gb2(logical), "allocated_gb": gb2(alloc),
            "footprint_gb": if known { json!(gb2(r.footprint)) } else { Value::Null },
            "frees_alone_gb": opt_gb(j.frees), "frees_max_gb": opt_gb(j.frees_max),
            "frees_basis": BASIS, "frees_confidence": j.conf.name(), "scanned_pct": j.scanned_pct,
            "stays_shared_gb": if known { json!(gb2(r.footprint - r.frees_alone)) } else { Value::Null },
            "shared_with": r.shared_with.iter().filter(|(_, b)| *b >= 10 << 20).map(|(o, b)| json!({ "id": ids[*o], "gb": gb2(*b) })).collect::<Vec<_>>(),
            "shared_outside_scan_gb": if known { json!(gb2(r.shared_outside)) } else { Value::Null },
            "last_write": disk_win::filetime_local(l2.map_or(l1.last_write, |l| l.last_write.max(l1.last_write))),
            "in_use": in_use, "in_flux": j.in_flux, "notes": notes_for(r, in_use),
            "actions": crate::disk_clean::actions_json(&it.key, &it.class, catalog_info(&it.key).and_then(|c| c.tool).as_deref(), r.volume.is_some(), j.frees.map(gb2)),
            "no_action": crate::disk_clean::no_action_reason(&it.key, &it.class),
            "recommended": false,
        }));
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
            json!({ "volume_id": v.guid, "paths": [v.root], "fs": v.fs, "free_gb": gb2(v.avail), "basis": basis,
                    "used_gb": gb2(v.total.saturating_sub(v.free)), "unattributed_gb": Value::Null, "notes": notes })
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
                        if matches!(it["frees_confidence"].as_str(), Some("exact") | Some("lower_bound")) {
                            it["frees_confidence"] = json!("estimate");
                        }
                    }
                    for t in &mut rep.totals {
                        if matches!(t["frees_confidence"].as_str(), Some("exact") | Some("lower_bound")) {
                            t["frees_confidence"] = json!("estimate");
                        }
                    }
                }
                return Ok(rep);
            }
        }
    }
    let items = discover(cfg, path);
    let sets = checkup_sets(&items);
    let job = ScanJob {
        items: items.iter().map(|i| i.paths.clone()).collect(),
        sets: sets.iter().map(|s| s.1.clone()).collect(),
        threads: s.scan_threads,
        budget_sec: budget_sec.unwrap_or(s.scan_budget_sec).clamp(1, 3600),
        stop_on_build: true,
    };
    let out = runner(&job, progress)?;
    let time = crate::win::local_time_string();
    let rep = build_checkup(&items, &out, &sets, &s, 0, false, &busy_projects(), &time);
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
        last_writes: out.items.iter().map(|r| r.level1.last_write).collect(),
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
        .filter(|(i, r)| i.key != "lane" && i.lane.is_none() && r.volume.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(&sys_guid)) && (i.key.starts_with("ue:global") || i.key == "ue:zen" || i.key.starts_with("cache:") || i.key.starts_with("gpu:") || i.key == "sys:crashdumps" || i.key == "temp:old"))
        .map(|(_, r)| r.level1.allocated)
        .sum();
    if cache_writes_allowed() {
        let c = crate::disk::CachesOnSystem { gb: (bytes as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0, measured: time.to_string() };
        let _ = std::fs::write(caches_file(), serde_json::to_string(&c).unwrap_or_default());
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
    let job = ScanJob {
        items: items.iter().map(|i| i.paths.clone()).collect(),
        sets: if set.is_empty() { Vec::new() } else { vec![set] },
        threads: s.scan_threads,
        budget_sec: s.scan_budget_sec.max(300),
        stop_on_build: false,
    };
    let out = runner(&job, progress)?;
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
            json!({ "id": it.id, "class": it.class, "actions": a, "no_action": crate::disk_clean::no_action_reason(&it.key, &it.class) })
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
        let mut line = format!("  {:<4}{:<38} {:>8.1} GB   {} ({})", it["id"].as_str().unwrap_or(""), title, it["logical_gb"].as_f64().unwrap_or(0.0), qualified(&it["frees_alone_gb"], conf), conf);
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

    /// A small tree on this PC's temp drive, scanned in-process: files, nested items and the
    /// set total follow the rules (everything there is exclusive to the test).
    #[test]
    fn scans_a_test_tree_in_process() {
        let root = std::env::temp_dir().join(format!("amg-disk-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"proj\Intermediate\Build")).unwrap();
        std::fs::create_dir_all(root.join(r"proj\Saved\Logs")).unwrap();
        std::fs::write(root.join(r"proj\Game.uproject"), b"{}").unwrap();
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

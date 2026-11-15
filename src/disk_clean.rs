//! Disk cleanup with undo (proposal 4 E, Phase 2, with the product owner's decisions).
//!
//! Three actions, and only these:
//! - `now`: a permanent delete. The default for `regenerable` items, and the only action that
//!   frees space at once (after the confirmation lists what goes and the predicted frees).
//! - `bin`: a same-volume move into `<vol>:\.AtherMemGuard-bin\<run-id>\`. The default for
//!   `disposable` and `rebuild_cost` items. Undoable, and it frees nothing until the bin is
//!   emptied: the output says so. The bin empties after `hold_hours`; when a drive is
//!   `critical` Ather MemGuard asks to empty it and never does it on its own.
//! - `tool`: the tool's own clean command (npm, pip, dotnet nuget). Permanent, "can't be undone".
//!
//! Zen is never deleted file by file, and lanes and worktrees are never deleted at all.
//!
//! Every safety rule is a pure function over `ItemCheck` and `PathFacts` (unit-tested one by
//! one) and is checked again at apply time against facts read at that moment. After acting,
//! the prediction is reported at once, labelled predicted; a watcher then reads the volume's
//! free space for up to 30 minutes and reports how much it rose, labelled measured and never
//! claimed as caused by the cleanup
//! (`Measure`, also pure). Undo is `disk-undo.json`, the same shape as `doctor-undo.json`.
//!
//! No `unsafe` here: deletes and moves use `std::fs`, which never follows a junction when it
//! removes one (a junction is removed as a link).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::disk_space::{gb2, is_within, norm};
use crate::disk_win;

/// The bin folder at the root of each volume.
pub const BIN_DIR: &str = ".AtherMemGuard-bin";
/// A plan an agent may act on is at most this old.
pub const AGENT_PLAN_MAX_AGE: Duration = Duration::from_secs(600);
/// `git ls-files` gets this long; a timeout is a refusal.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// The measured watch reads free space for at most this long.
pub const WATCH_MAX_SEC: u64 = 30 * 60;
/// Readings that agree within this are "the same".
const AGREE: i64 = 16 << 20;
/// Free-space movement below this is noise.
const NOISE: i64 = 64 << 20;
/// Runs kept in the undo log, and plans kept.
const UNDO_KEEP_RUNS: usize = 30;
const PLANS_KEEP: usize = 10;
const WATCHES_KEEP: usize = 20;

const FT_PER_SEC: u64 = 10_000_000;

// ---------------------------------------------------------------- actions

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanAction {
    Now,
    Bin,
    Tool,
}

impl CleanAction {
    pub fn name(self) -> &'static str {
        match self {
            CleanAction::Now => "now",
            CleanAction::Bin => "bin",
            CleanAction::Tool => "tool",
        }
    }

    pub fn from_name(s: &str) -> Option<CleanAction> {
        match s.trim().to_ascii_lowercase().as_str() {
            "now" | "delete" => Some(CleanAction::Now),
            "bin" => Some(CleanAction::Bin),
            "tool" => Some(CleanAction::Tool),
            _ => None,
        }
    }
}

/// A tool's own clean command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCommand {
    /// The program name looked up on PATH ("npm", "pip", "dotnet").
    pub exe: &'static str,
    pub args: &'static [&'static str],
}

impl ToolCommand {
    pub fn display(&self) -> String {
        format!("{} {}", self.exe, self.args.join(" "))
    }
}

/// The clean command for a catalogue `tool` name.
pub fn tool_command(tool: &str) -> Option<ToolCommand> {
    match tool {
        "npm" => Some(ToolCommand { exe: "npm", args: &["cache", "clean", "--force"] }),
        "pip" => Some(ToolCommand { exe: "pip", args: &["cache", "purge"] }),
        "dotnet" => Some(ToolCommand { exe: "dotnet", args: &["nuget", "locals", "http-cache", "--clear"] }),
        _ => None,
    }
}

/// What an item offers, the default first. Zen, lanes, worktrees and `review` items offer
/// nothing; `bin` only where the bin can be made on that volume.
pub fn offered_actions(key: &str, class: &str, tool: Option<&str>, bin_ok: bool) -> Vec<CleanAction> {
    if key == "ue:zen" || key == "lane" || key == "git:lfs" || key == "memguard:bin" || class == "review" {
        return Vec::new();
    }
    let tool = tool.and_then(tool_command).map(|_| CleanAction::Tool);
    let bin = bin_ok.then_some(CleanAction::Bin);
    match class {
        "regenerable" => [Some(CleanAction::Now), bin, tool].into_iter().flatten().collect(),
        "disposable" | "rebuild_cost" => [bin, Some(CleanAction::Now), tool].into_iter().flatten().collect(),
        _ => Vec::new(),
    }
}

/// Why an item offers nothing, in plain words (for the checkup and refusals).
pub fn no_action_reason(key: &str, class: &str) -> Option<&'static str> {
    match (key, class) {
        ("ue:zen", _) => Some("Zen is cleaned by Zen's own garbage collection, never file by file."),
        ("lane", _) => Some("Ather MemGuard never removes lanes or worktrees. Use git worktree remove, or delete it yourself."),
        ("memguard:bin", _) => Some("Empty it with memguard disk bin --empty."),
        (_, "review") => Some("This one is your call; Ather MemGuard doesn't remove it."),
        _ => None,
    }
}

/// The `actions` list of a checkup item (proposal section 5).
pub fn actions_json(key: &str, class: &str, tool: Option<&str>, bin_ok: bool, frees_gb: Option<f64>) -> Vec<Value> {
    offered_actions(key, class, tool, bin_ok)
        .into_iter()
        .map(|a| match a {
            CleanAction::Now => json!({ "kind": "now", "label": "Delete it", "undo": if class == "regenerable" { "None; it is rebuilt when needed" } else { "None" },
                                        "frees_now_gb": frees_gb, "frees_basis": "predicted" }),
            CleanAction::Bin => json!({ "kind": "bin", "label": "Move to the bin", "undo": "Put it back (memguard disk undo)", "frees_now_gb": 0.0,
                                        "frees_when_bin_emptied_gb": frees_gb, "frees_basis": "predicted" }),
            CleanAction::Tool => {
                let cmd = tool.and_then(tool_command).map(|c| c.display()).unwrap_or_default();
                json!({ "kind": "tool", "label": format!("Clean with {}", tool.unwrap_or("the tool")), "undo": "Can't be undone", "command": cmd,
                        "frees_now_gb": frees_gb, "frees_basis": "predicted" })
            }
        })
        .collect()
}

/// `d3` or `d3:bin`.
pub fn parse_pick(s: &str) -> Result<(String, Option<CleanAction>), String> {
    let s = s.trim();
    let (id, action) = match s.split_once(':') {
        Some((id, a)) => (id, Some(CleanAction::from_name(a).ok_or_else(|| format!("\"{}\": the action must be now, bin or tool.", s))?)),
        None => (s, None),
    };
    let ok = id.len() > 1 && id.starts_with('d') && id[1..].chars().all(|c| c.is_ascii_digit());
    if !ok {
        return Err(format!("\"{}\" is not an item id from the disk checkup (d1, d2, ...).", s));
    }
    Ok((id.to_string(), action))
}

// ---------------------------------------------------------------- files in the data folder

/// Where the cleanup keeps its records: the data folder, or a test folder.
#[derive(Debug, Clone)]
pub struct DiskFiles {
    pub dir: PathBuf,
    writable: bool,
}

impl Default for DiskFiles {
    fn default() -> Self {
        DiskFiles { dir: crate::paths::data_dir(), writable: crate::disk_scan::cache_writes_allowed() }
    }
}

impl DiskFiles {
    /// Records in `dir` (tests: a temp folder they create and remove).
    pub fn in_dir(dir: impl Into<PathBuf>) -> DiskFiles {
        DiskFiles { dir: dir.into(), writable: true }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read<T: serde::de::DeserializeOwned + Default>(&self, name: &str) -> T {
        std::fs::read_to_string(self.path(name)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn write<T: Serialize>(&self, name: &str, v: &T) -> Result<(), String> {
        if !self.writable {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let p = self.path(name);
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
    }

    pub fn plans(&self) -> Vec<PlanRecord> {
        self.read("disk-plans.json")
    }

    pub fn save_plan(&self, p: &PlanRecord) -> Result<(), String> {
        let mut all = self.plans();
        all.push(p.clone());
        if all.len() > PLANS_KEEP {
            let extra = all.len() - PLANS_KEEP;
            all.drain(..extra);
        }
        self.write("disk-plans.json", &all)
    }

    pub fn undo_log(&self) -> UndoLog {
        self.read("disk-undo.json")
    }

    pub fn save_undo_log(&self, l: &UndoLog) -> Result<(), String> {
        self.write("disk-undo.json", l)
    }

    pub fn watches(&self) -> Vec<WatchRecord> {
        self.read("disk-watch.json")
    }

    pub fn save_watches(&self, w: &[WatchRecord]) -> Result<(), String> {
        let mut v = w.to_vec();
        if v.len() > WATCHES_KEEP {
            let extra = v.len() - WATCHES_KEEP;
            v.drain(..extra);
        }
        self.write("disk-watch.json", &v)
    }

    /// Change one watch record (read, edit, write back).
    pub fn update_watch(&self, id: &str, f: impl FnOnce(&mut WatchRecord)) -> Result<(), String> {
        let mut all = self.watches();
        if let Some(w) = all.iter_mut().find(|w| w.id == id) {
            f(w);
        }
        self.save_watches(&all)
    }

    pub fn relocate_record(&self) -> Option<crate::disk_relocate::RelocateRecord> {
        std::fs::read_to_string(self.path("disk-relocate.json")).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    pub fn save_relocate_record(&self, r: &crate::disk_relocate::RelocateRecord) -> Result<(), String> {
        self.write("disk-relocate.json", r)
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// "20261114153012-417": local time, then a counter so two runs in one second differ.
pub fn run_id() -> String {
    let digits: String = crate::win::local_time_string().chars().filter(|c| c.is_ascii_digit()).collect();
    format!("{}-{}", digits, crate::win::now_filetime() / 10_000 % 1000)
}

// ---------------------------------------------------------------- plans

/// One item as a plan measured it: what the apply-time checks compare against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedItem {
    pub id: String,
    pub key: String,
    pub class: String,
    pub title: String,
    pub paths: Vec<String>,
    /// File id of each path when the plan counted it (`None`: could not be read).
    pub path_ids: Vec<Option<u128>>,
    /// Newest write under the item when the plan counted it (FILETIME).
    pub last_write: u64,
    pub volume: Option<String>,
    pub volume_root: Option<String>,
    pub frees_alone: Option<u64>,
    pub confidence: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub unmeasurable: Option<String>,
}

/// The `frees_together` of one volume's items in a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedVolume {
    pub volume: String,
    pub root: String,
    pub fs: String,
    pub item_ids: Vec<String>,
    pub frees_together: Option<u64>,
    pub confidence: String,
}

/// A plan, as `memguard disk plan` / `memguard_disk_plan` measured it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRecord {
    pub id: String,
    pub unix: u64,
    pub time: String,
    pub items: Vec<PlannedItem>,
    pub volumes: Vec<PlannedVolume>,
}

impl PlanRecord {
    pub fn item(&self, id: &str) -> Option<&PlannedItem> {
        self.items.iter().find(|i| i.id.eq_ignore_ascii_case(id))
    }

    pub fn age_sec(&self, now_unix: u64) -> u64 {
        now_unix.saturating_sub(self.unix)
    }
}

/// The plan an agent may act on: exactly `plan_id`, from the plans this MCP server made itself
/// (kept in its memory, never read back from a file an agent could edit). Refused when older
/// than 10 minutes or when an id isn't in it.
pub fn plan_for_agent<'a>(plans: &'a [PlanRecord], plan_id: &str, ids: &[String], now_unix: u64) -> Result<&'a PlanRecord, String> {
    let p = plans
        .iter()
        .find(|p| p.id == plan_id)
        .ok_or_else(|| format!("There is no plan {} from this session. Call memguard_disk_plan with exactly the ids you want to clean, then pass its plan_id.", plan_id))?;
    if p.age_sec(now_unix) > AGENT_PLAN_MAX_AGE.as_secs() {
        return Err(format!(
            "The plan {} was made {} minutes ago; an agent may only act on a plan under 10 minutes old. Call memguard_disk_plan again.",
            p.id,
            p.age_sec(now_unix) / 60
        ));
    }
    if let Some(missing) = ids.iter().find(|i| p.item(i).is_none()) {
        return Err(format!("{} is not in plan {}. Plan exactly the items you want to clean.", missing, p.id));
    }
    Ok(p)
}

/// What a clean says when what the person or agent saw is not what the ids name now.
pub const CHECKUP_CHANGED: &str = "The checkup changed since you confirmed; nothing was deleted. Check again.";

/// A short, stable name for one checkup item as it was seen: its key, its paths and the file
/// ids of its roots (FNV-1a, 64 bits, hex). The checkup window passes it with each pick, so a
/// clean can tell that `d3` still means the folder the person confirmed.
pub fn item_fingerprint(key: &str, paths: &[String], root_ids: &[u128]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for x in b {
            h ^= *x as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    eat(key.as_bytes());
    for p in paths {
        eat(norm(p).as_bytes());
    }
    let mut ids = root_ids.to_vec();
    ids.sort_unstable();
    for id in ids {
        eat(&id.to_le_bytes());
    }
    format!("{:016x}", h)
}

/// The fingerprint of item `id` (`d3`) of a saved checkup.
pub fn checkup_item_fingerprint(c: &crate::disk_scan::CachedCheckup, id: &str) -> Option<String> {
    let k = c.index_of(id)?;
    let it = &c.items[k];
    Some(item_fingerprint(&it.key, &it.paths, c.root_ids.get(k).map(Vec::as_slice).unwrap_or(&[])))
}

/// `d3=0123abcd...` from `--expect`.
pub fn parse_expect(s: &str) -> Result<(String, String), String> {
    let (id, fp) = s.trim().split_once('=').ok_or_else(|| format!("\"{}\": --expect takes <id>=<fingerprint>, like d3=0f1e2d3c4b5a6978.", s))?;
    parse_pick(id)?;
    if fp.len() != 16 || !fp.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("\"{}\": the fingerprint is 16 hex digits.", s));
    }
    Ok((id.trim().to_ascii_lowercase(), fp.to_ascii_lowercase()))
}

/// Does the saved checkup `c` still name what was seen? `checkup_id` is the checkup the person
/// (or the window) confirmed on, `expects` the fingerprints of the picked items as seen.
/// Every picked id needs a matching fingerprint when any is given.
pub fn check_identity(c: &crate::disk_scan::CachedCheckup, checkup_id: &str, expects: &[(String, String)], ids: &[String]) -> Result<(), String> {
    if !c.ident().eq_ignore_ascii_case(checkup_id.trim()) {
        return Err(CHECKUP_CHANGED.into());
    }
    for (id, fp) in expects {
        if !ids.iter().any(|i| i.eq_ignore_ascii_case(id)) {
            return Err(format!("--expect names {}, which isn't one of the items to clean.", id));
        }
        if checkup_item_fingerprint(c, id).as_deref() != Some(fp.as_str()) {
            return Err(CHECKUP_CHANGED.into());
        }
    }
    if !expects.is_empty() {
        if let Some(missing) = ids.iter().find(|i| !expects.iter().any(|(e, _)| e.eq_ignore_ascii_case(i))) {
            return Err(format!("{} has no --expect; pass one for every item, or none.", missing));
        }
    }
    Ok(())
}

/// The folders a fresh plan found must be the ones the checkup counted (their file ids): a
/// folder deleted and made again in between is not what the person saw.
pub fn check_live_ids(c: &crate::disk_scan::CachedCheckup, plan: &PlanRecord) -> Result<(), String> {
    for it in &plan.items {
        let Some(k) = c.index_of(&it.id) else { return Err(CHECKUP_CHANGED.into()) };
        let seen = c.root_ids.get(k).cloned().unwrap_or_default();
        if seen.is_empty() {
            continue;
        }
        if c.items[k].paths != it.paths || it.path_ids.iter().any(|id| !id.is_some_and(|id| seen.contains(&id))) {
            return Err(CHECKUP_CHANGED.into());
        }
    }
    Ok(())
}

/// What the cleanup allows for an item key, from the embedded catalogue (never from a plan or a
/// cache file): (class, tool). `cache:old` is the relocation's old caches (disposable); lanes
/// and the bin are `review`. `None`: not a key Ather MemGuard cleans.
pub fn catalog_class(key: &str) -> Option<(String, Option<String>)> {
    match key {
        "cache:old" => Some(("disposable".into(), None)),
        "lane" | "memguard:bin" => Some(("review".into(), None)),
        _ => crate::disk_scan::catalog_info(key).map(|i| (i.class, i.tool)),
    }
}

/// Keys an agent may clean: the catalogue's `regenerable` entries, nothing else.
pub fn agent_may_clean(key: &str) -> bool {
    key != "cache:old" && crate::disk_scan::catalog_info(key).is_some_and(|i| i.class == "regenerable")
}

// ---------------------------------------------------------------- safety rules (pure)

/// Folders never touched, wherever they appear under a project root.
const NEVER_COMPONENTS: &[&str] = &["content", "config", "source", "binaries", ".git"];
/// `Saved\...` folders that hold the person's work and settings.
const NEVER_SAVED: &[&str] = &["autosaves", "savegames", "config", "collections", "sourcecontrol"];

/// Is a path relative to a project root on the never-list? (`Content`, `Config`, `Source`,
/// `Plugins\*\Content`, `.git`, `Saved\Autosaves|SaveGames|Config|Collections|SourceControl`,
/// `Binaries`, the root itself, `Saved` and `Plugins` as a whole.)
pub fn never_touch(rel: &str) -> Option<String> {
    let r = norm(rel);
    let parts: Vec<&str> = r.split('\\').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Some("the project folder itself".into());
    }
    if let Some(p) = parts.iter().find(|p| NEVER_COMPONENTS.contains(p)) {
        return Some(format!("a {} folder", p));
    }
    if parts.len() == 1 && (parts[0] == "saved" || parts[0] == "plugins") {
        return Some(format!("the whole {} folder", parts[0]));
    }
    if parts[0] == "saved" && parts.len() >= 2 && NEVER_SAVED.contains(&parts[1]) {
        return Some(format!("Saved\\{}", parts[1]));
    }
    None
}

/// Does `rel` match the catalogue path of a project item? Plugin items match
/// `Plugins\...\<leaf>` (the plugin folder can be nested).
pub fn rel_matches_catalog(rel: &str, cat_path: &str, plugins: bool) -> bool {
    let r = norm(rel);
    let c = norm(cat_path);
    if !plugins {
        return r == c;
    }
    let parts: Vec<&str> = r.split('\\').collect();
    parts.len() >= 3 && parts[0] == "plugins" && r.ends_with(&format!("\\{}", c))
}

/// The answer of `git ls-files` for an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitCheck {
    /// Not inside a git checkout.
    NotRepo,
    /// Inside one, and git tracks nothing in the item.
    Clean,
    /// Git tracks this many files in it.
    Tracked(usize),
    /// Git didn't answer in time (5 s).
    TimedOut,
    /// Git couldn't be run, or failed.
    Failed(String),
}

/// What was found about one path of an item, at the moment of checking.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PathFacts {
    pub path: String,
    pub exists: bool,
    /// The item root and every folder between it and the project root (or the user's profile
    /// for shared caches), with whether it is a reparse point. The item root is first.
    pub chain: Vec<(String, bool)>,
    /// The nearest folder at or above the item holding a `.uproject`, or an `Engine` folder
    /// with `Build\Build.version`.
    pub recognised_root: Option<String>,
    /// The path relative to `recognised_root`.
    pub rel: Option<String>,
    /// The path itself is a checkout: it holds a `.uproject` or a `.git`.
    pub is_checkout: bool,
    pub in_profile: bool,
    /// The cloud sync root it is inside, if any.
    pub sync_root: Option<String>,
    /// Cloud placeholder attributes on the root.
    pub cloud_attrs: bool,
    pub git: Option<GitCheck>,
    /// A `.git` somewhere inside the item (a nested repository), and whether that walk
    /// couldn't finish.
    pub holds_repo: bool,
    pub repo_walk_incomplete: bool,
    pub file_id: Option<u128>,
}

/// What the rules need about the item as a whole.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemCheck {
    pub key: String,
    pub class: String,
    /// A shared cache (`[[global]]`): allowed only at its catalogue path, inside the profile.
    pub global: bool,
    /// Project items: the catalogue path (`Intermediate`, `Saved\Logs`) and whether it is per plugin.
    pub catalog_rel: Option<String>,
    pub plugins: bool,
    /// Global items: the paths the catalogue allows (normalised). For TEMP entries, the folder
    /// whose direct children are allowed.
    pub allowed_paths: Vec<String>,
    pub allowed_parent: Option<String>,
    /// File ids from the plan, one per path.
    pub planned_ids: Vec<Option<u128>>,
    /// Newest write when the plan counted it, and now (FILETIME). `now_last_write` is `None`
    /// when the walk couldn't finish.
    pub planned_last_write: u64,
    pub now_last_write: Option<u64>,
    pub now_ft: u64,
    pub min_age_hours: f64,
    /// Project folders Unreal is working in (normalised), and whether some Unreal process's
    /// command line couldn't be read.
    pub busy: Vec<String>,
    pub busy_unknown: bool,
}

/// The first rule an item breaks, in plain words, or `None` when it may be cleaned. The whole
/// item is refused when any one of its paths breaks a rule.
pub fn refusal(item: &ItemCheck, paths: &[PathFacts]) -> Option<String> {
    if let Some(why) = no_action_reason(&item.key, &item.class) {
        return Some(why.into());
    }
    if paths.is_empty() {
        return Some("It has no folders left to clean.".into());
    }
    for (k, p) in paths.iter().enumerate() {
        if let Some(why) = path_refusal(item, p, item.planned_ids.get(k).copied().flatten()) {
            return Some(why);
        }
    }
    // Recent writes refuse the whole item: a half-deleted Intermediate is worse than a full one.
    let Some(now_lw) = item.now_last_write else {
        return Some("Ather MemGuard couldn't check every file in it for recent writes, so it leaves it alone.".into());
    };
    let age_ft = (item.min_age_hours.max(0.0) * 3600.0 * FT_PER_SEC as f64) as u64;
    if now_lw > item.now_ft.saturating_sub(age_ft) {
        return Some(format!("Something in it was written in the last {} hours, so it may be in use. Nothing in it was touched.", fmt_hours(item.min_age_hours)));
    }
    if item.planned_last_write > 0 && now_lw > item.planned_last_write {
        return Some("Something in it was written after the plan was made. Make a new plan first.".into());
    }
    if item.busy_unknown {
        return Some("Unreal is running and Ather MemGuard can't tell which project it works in, so it waits.".into());
    }
    for p in paths {
        let root = p.recognised_root.as_deref().map(norm);
        let here = root.as_deref().unwrap_or(&p.path);
        let np = norm(&p.path);
        if item.busy.iter().any(|b| is_within(here, b) || is_within(b, here) || is_within(&np, b)) {
            return Some("An Unreal editor, build or cook is working in this project. Try again when it's done.".into());
        }
    }
    None
}

fn path_refusal(item: &ItemCheck, p: &PathFacts, planned_id: Option<u128>) -> Option<String> {
    if !p.exists {
        return Some(format!("{} isn't there any more.", p.path));
    }
    if p.is_checkout {
        return Some("It is a project checkout (a lane or worktree). Ather MemGuard never removes those.".into());
    }
    // Allow-list only, under a recognised root (projects) or at the catalogue path (caches).
    if item.global {
        if !p.in_profile {
            return Some("It is outside your user folder and outside any Unreal project, so Ather MemGuard doesn't touch it.".into());
        }
        let np = norm(&p.path);
        let listed = item.allowed_paths.iter().any(|a| *a == np)
            || item.allowed_parent.as_deref().is_some_and(|par| Path::new(&np).parent().map(|x| norm(&x.display().to_string())) == Some(par.to_string()));
        if !listed {
            return Some("It isn't at a place Ather MemGuard knows is safe to clean.".into());
        }
    } else {
        let (Some(_), Some(rel)) = (&p.recognised_root, &p.rel) else {
            return Some("It isn't inside a folder with a .uproject (or an engine with Engine\\Build\\Build.version).".into());
        };
        if let Some(what) = never_touch(rel) {
            return Some(format!("It is {}, which Ather MemGuard never touches.", what));
        }
        let Some(cat) = &item.catalog_rel else {
            return Some("It isn't on the list of folders Ather MemGuard may clean.".into());
        };
        if !rel_matches_catalog(rel, cat, item.plugins) {
            return Some("It isn't on the list of folders Ather MemGuard may clean.".into());
        }
    }
    if let Some((link, _)) = p.chain.iter().find(|(_, r)| *r) {
        return Some(format!("{} is a link (junction or symbolic link) to somewhere else, so Ather MemGuard doesn't follow it.", link));
    }
    if p.cloud_attrs || p.sync_root.is_some() {
        return Some("It is inside a cloud-synced folder; deleting there deletes the cloud copy too.".into());
    }
    match &p.git {
        Some(GitCheck::Tracked(n)) => return Some(format!("Git tracks {} file{} in it, so it is part of the project, not a cache.", n, if *n == 1 { "" } else { "s" })),
        Some(GitCheck::TimedOut) => return Some("Git didn't answer within 5 seconds, so Ather MemGuard can't be sure git doesn't track it.".into()),
        Some(GitCheck::Failed(why)) => return Some(format!("Ather MemGuard couldn't ask git whether it tracks files in it ({}).", why)),
        _ => {}
    }
    if p.holds_repo {
        return Some("It holds a git repository (a .git folder) inside it, so Ather MemGuard leaves it alone.".into());
    }
    if p.repo_walk_incomplete {
        return Some("Ather MemGuard couldn't check every folder in it for a git repository, so it leaves it alone.".into());
    }
    if let Some(planned) = planned_id {
        if p.file_id != Some(planned) {
            return Some("The folder was replaced since the plan was made (its file id changed). Make a new plan first.".into());
        }
    } else if !item.planned_ids.is_empty() {
        return Some("The plan couldn't identify this folder, so Ather MemGuard can't be sure it is the same one.".into());
    }
    None
}

fn fmt_hours(h: f64) -> String {
    if (h - h.round()).abs() < 1e-9 {
        format!("{}", h.round() as i64)
    } else {
        format!("{:.1}", h)
    }
}

// ---------------------------------------------------------------- reading the facts

fn attrs(p: &Path) -> Option<u32> {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| m.file_attributes())
}

/// `.uproject` folder, or an `Engine` folder with `Build\Build.version`.
pub fn is_recognised_root(dir: &Path) -> bool {
    let has_uproject = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".uproject")))
        .unwrap_or(false);
    let engine = dir.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("Engine")) && dir.join(r"Build\Build.version").is_file();
    has_uproject || engine
}

/// The user's cloud sync roots: OneDrive's variables and every root registered with Windows
/// (`SyncRootManager`, read-only).
pub fn sync_roots() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for var in ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                out.push(norm(&v));
            }
        }
    }
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    if let Ok(mgr) = hklm.open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager") {
        for name in mgr.enum_keys().flatten() {
            if let Ok(roots) = mgr.open_subkey(format!(r"{}\UserSyncRoots", name)) {
                for (_, v) in roots.enum_values().flatten() {
                    let s = v.to_string();
                    if !s.trim().is_empty() {
                        out.push(norm(s.trim()));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Ask git whether it tracks anything under `path`. Not in a checkout: `NotRepo`.
///
/// Every repository above the item is asked, nearest first, so a nested repository (a plugin
/// with its own `.git` inside the project's checkout) can't hide files the outer one tracks.
/// The pathspec ignores case (`:(literal,icase)`): Windows paths do, and the catalogue's
/// spelling (`Intermediate`) need not be the one git recorded (`intermediate/...`). A git that
/// refuses the repository (`safe.directory`, dubious ownership) or fails is a refusal, never
/// "not tracked".
pub fn git_check(path: &Path) -> GitCheck {
    git_check_env(path, &[])
}

/// `git_check` with extra environment variables for git (tests).
pub fn git_check_env(path: &Path, env: &[(&str, &str)]) -> GitCheck {
    let repos: Vec<&Path> = path.ancestors().skip(1).filter(|a| std::fs::symlink_metadata(a.join(".git")).is_ok()).collect();
    if repos.is_empty() {
        return GitCheck::NotRepo;
    }
    let mut tracked = 0;
    for repo in repos {
        match git_ls(repo, path, env) {
            GitCheck::Tracked(n) => tracked += n,
            GitCheck::Clean | GitCheck::NotRepo => {}
            other => return other,
        }
    }
    if tracked > 0 {
        GitCheck::Tracked(tracked)
    } else {
        GitCheck::Clean
    }
}

/// Environment variables that would point git at another repository or index.
const GIT_ENV_CLEARED: &[&str] = &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM", "GIT_NAMESPACE", "GIT_OBJECT_DIRECTORY"];

fn git_ls(repo: &Path, path: &Path, env: &[(&str, &str)]) -> GitCheck {
    let rel = path.strip_prefix(repo).map(|r| r.display().to_string()).unwrap_or_default();
    let repo_s = repo.display().to_string();
    let spec = format!(":(literal,icase){}", rel.replace('\\', "/"));
    // No fsmonitor: a repository's config must not get to run a program here.
    let args = ["-c", "core.fsmonitor=false", "-C", repo_s.as_str(), "ls-files", "-z", "--", spec.as_str()];
    let mut envs: Vec<(&str, Option<&str>)> = GIT_ENV_CLEARED.iter().map(|k| (*k, None)).collect();
    envs.extend(env.iter().map(|(k, v)| (*k, Some(*v))));
    match run_cmd(Path::new("git"), &args, GIT_TIMEOUT, &envs) {
        Ok((code, out, err)) => git_answer(code, &out, &err),
        Err(RunError::TimedOut) => GitCheck::TimedOut,
        Err(RunError::NotFound) => GitCheck::Failed("git isn't installed or isn't on PATH".into()),
        Err(RunError::Other(e)) => GitCheck::Failed(e),
    }
}

/// What `git ls-files -z` said, from its exit code, stdout and stderr (pure).
pub fn git_answer(code: i32, out: &[u8], err: &[u8]) -> GitCheck {
    if code == 0 {
        let n = out.split(|b| *b == 0).filter(|s| !s.is_empty()).count();
        return if n == 0 { GitCheck::Clean } else { GitCheck::Tracked(n) };
    }
    let e = String::from_utf8_lossy(err).to_ascii_lowercase();
    if e.contains("dubious ownership") || e.contains("safe.directory") {
        return GitCheck::Failed("git won't read this repository because another user owns it (safe.directory)".into());
    }
    let first = String::from_utf8_lossy(err).lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(160).collect::<String>());
    GitCheck::Failed(match first {
        Some(l) => format!("git stopped with exit code {}: {}", code, l),
        None => format!("git stopped with exit code {}", code),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    NotFound,
    TimedOut,
    Other(String),
}

/// Run a console program with no window; (exit code, stdout), killed after `timeout`.
pub fn run_with_timeout(exe: &Path, args: &[&str], timeout: Duration) -> Result<(i32, Vec<u8>), RunError> {
    run_cmd(exe, args, timeout, &[]).map(|(c, o, _)| (c, o))
}

/// `run_with_timeout` that also returns stderr, with environment changes (`None` removes one).
pub fn run_cmd(exe: &Path, args: &[&str], timeout: Duration, env: &[(&str, Option<&str>)]) -> Result<(i32, Vec<u8>, Vec<u8>), RunError> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut child = cmd.spawn().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { RunError::NotFound } else { RunError::Other(e.to_string()) })?;
    let read_all = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = r.read_to_end(&mut s);
            s
        })
    };
    let stdout = child.stdout.take().ok_or(RunError::Other("no stdout".into()))?;
    let stderr = child.stderr.take().ok_or(RunError::Other("no stderr".into()))?;
    let out_reader = read_all(Box::new(stdout));
    let err_reader = read_all(Box::new(stderr));
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            Err(e) => return Err(RunError::Other(e.to_string())),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    Ok((code, out, err))
}

/// Does `root` hold a git repository (a `.git` file or folder) anywhere inside it? Links are
/// not followed. `None` when the walk couldn't read everything before `deadline`.
pub fn holds_repo(root: &Path, deadline: Instant) -> Option<bool> {
    use std::os::windows::fs::MetadataExt;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if Instant::now() > deadline {
            return None;
        }
        let rd = std::fs::read_dir(&d).ok()?;
        for e in rd {
            let e = e.ok()?;
            if e.file_name().to_string_lossy().eq_ignore_ascii_case(".git") {
                return Some(true);
            }
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            let a = m.file_attributes();
            if a & disk_win::A_DIR != 0 && a & disk_win::A_REPARSE == 0 {
                stack.push(e.path());
            }
        }
    }
    Some(false)
}

/// Read every fact the rules need about one path, now.
pub fn path_facts(path: &str, global: bool, profile: Option<&str>, syncs: &[String], check_git: bool) -> PathFacts {
    let p = Path::new(path);
    let np = norm(path);
    let mut f = PathFacts { path: path.to_string(), ..Default::default() };
    let Some(a) = attrs(p) else { return f };
    f.exists = true;
    f.cloud_attrs = a & (disk_win::A_RECALL_OPEN | disk_win::A_RECALL_DATA | disk_win::A_OFFLINE) != 0;
    f.sync_root = syncs.iter().find(|s| is_within(&np, s)).cloned();
    f.in_profile = profile.map(norm).is_some_and(|pr| is_within(&np, &pr) && np != pr);
    f.is_checkout = p.join(".git").exists() || crate::disk_scan::has_uproject(p);
    f.recognised_root = p.ancestors().skip(1).find(|a| is_recognised_root(a)).map(|r| r.display().to_string());
    if let Some(root) = &f.recognised_root {
        f.rel = p.strip_prefix(root).ok().map(|r| r.display().to_string());
    }
    // The chain: the item root up to the project root (inclusive), or up to the profile.
    let stop = if global { profile.map(norm) } else { f.recognised_root.as_deref().map(norm) };
    for anc in p.ancestors() {
        let na = norm(&anc.display().to_string());
        if global && stop.as_deref() == Some(na.as_str()) {
            break;
        }
        let reparse = attrs(anc).is_some_and(|x| x & disk_win::A_REPARSE != 0);
        f.chain.push((anc.display().to_string(), reparse));
        if !global && stop.as_deref() == Some(na.as_str()) {
            break;
        }
        if stop.is_none() || anc.parent().is_none() {
            break;
        }
    }
    if check_git {
        f.git = Some(git_check(p));
        match holds_repo(p, Instant::now() + Duration::from_secs(60)) {
            Some(h) => f.holds_repo = h,
            None => f.repo_walk_incomplete = true,
        }
    }
    f.file_id = disk_win::identity(path).map(|(_, id, _)| id);
    f
}

/// The newest write under `paths`, read now without following links. `None` when the walk
/// couldn't read everything in `budget`.
pub fn newest_write(paths: &[String], budget: Duration) -> Option<u64> {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};
    let vs = disk_win::VolumeScan {
        cluster: 4096,
        block_cloning: false,
        level2: false,
        threads: 4,
        deadline: Instant::now() + budget,
        stop: Arc::new(AtomicBool::new(false)),
        started_ft: u64::MAX,
        multi: Mutex::new(std::collections::HashMap::new()),
        progress_files: Arc::new(AtomicU64::new(0)),
    };
    let (st, _) = disk_win::walk_item(&vs, 0, paths, &[]);
    if st.cut_short || st.dir_failures > 0 {
        return None;
    }
    // The roots' own write times count too (a folder renamed in, or emptied).
    let own = paths.iter().filter_map(|p| std::fs::symlink_metadata(p).ok()).filter_map(|m| m.modified().ok()).map(filetime_of).max().unwrap_or(0);
    Some(st.last_write.max(own))
}

fn filetime_of(t: std::time::SystemTime) -> u64 {
    // FILETIME counts from 1601; UNIX from 1970: 11644473600 s apart.
    let unix = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() / 100) as u64;
    unix + 11_644_473_600 * FT_PER_SEC
}

// ---------------------------------------------------------------- deleting, moving, tools

/// What a permanent delete removed and what is left.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeleteReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Junctions and symbolic links removed as links (their targets untouched).
    pub links: u64,
    /// Up to 20 paths that couldn't be removed, with the reason.
    pub failed: Vec<(String, String)>,
    pub failed_count: u64,
    /// The root is still there (something in it couldn't be removed).
    pub remains: bool,
}

impl DeleteReport {
    fn fail(&mut self, p: &Path, e: &std::io::Error) {
        self.failed_count += 1;
        if self.failed.len() < 20 {
            self.failed.push((p.display().to_string(), e.to_string()));
        }
    }

    /// "Removed 6,301 files (16.2 GB)" and, after a partial delete, what remains.
    pub fn words(&self) -> String {
        let mut s = format!("Removed {} ({})", plural(self.files, "file"), size_words(self.bytes));
        if self.links > 0 {
            s.push_str(&format!(" and {} (their targets were left alone)", plural(self.links, "link")));
        }
        if self.failed_count > 0 {
            s.push_str(&format!(
                ". {} could not be removed and are still there{}",
                thousands(self.failed_count),
                self.failed.first().map(|(p, e)| format!(", for example {} ({})", p, e)).unwrap_or_default()
            ));
        }
        s
    }
}

/// "1 file", "6,301 files".
pub fn plural(n: u64, word: &str) -> String {
    format!("{} {}{}", thousands(n), word, if n == 1 { "" } else { "s" })
}

/// "16.4 GB", or "30 MB" below 0.1 GB (so small items don't read as nothing).
pub fn size_words(bytes: u64) -> String {
    if bytes > 0 && bytes < (1 << 20) {
        "less than 1 MB".into()
    } else if bytes > 0 && bytes < (1u64 << 30) / 10 {
        format!("{} MB", (bytes as f64 / (1u64 << 20) as f64).round() as u64)
    } else {
        format!("{:.1} GB", gb2(bytes))
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Delete `root` and everything in it, never following a reparse point: a junction or
/// symbolic link inside is removed as a link and its target is left alone. Read-only files
/// are made writable first. Reports exactly what was removed and what remains.
pub fn delete_tree(root: &Path) -> DeleteReport {
    use std::os::windows::fs::MetadataExt;
    let mut r = DeleteReport::default();
    // The callers' checks refuse a root that is a link, but the root can change between that
    // check and now (and the bin's expiry has no such check): never walk into a link's target.
    if let Ok(m) = std::fs::symlink_metadata(root) {
        let a = m.file_attributes();
        if a & disk_win::A_REPARSE != 0 {
            let res = if a & disk_win::A_DIR != 0 { std::fs::remove_dir(root) } else { std::fs::remove_file(root) };
            match res {
                Ok(()) => r.links += 1,
                Err(err) => r.fail(root, &err),
            }
            r.remains = std::fs::symlink_metadata(root).is_ok();
            return r;
        }
    }
    delete_dir(root, &mut r);
    r.remains = std::fs::symlink_metadata(root).is_ok();
    r
}

fn clear_readonly(p: &Path, m: &std::fs::Metadata) {
    let mut perm = m.permissions();
    if perm.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(p, perm);
    }
}

fn delete_dir(dir: &Path, r: &mut DeleteReport) {
    use std::os::windows::fs::MetadataExt;
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for e in rd {
                let e = match e {
                    Ok(e) => e,
                    Err(err) => {
                        r.fail(dir, &err);
                        continue;
                    }
                };
                let p = e.path();
                let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
                let a = m.file_attributes();
                if a & disk_win::A_REPARSE != 0 {
                    // A link: remove the link itself, never what it points to.
                    let res = if a & disk_win::A_DIR != 0 { std::fs::remove_dir(&p) } else { std::fs::remove_file(&p) };
                    match res {
                        Ok(()) => r.links += 1,
                        Err(err) => r.fail(&p, &err),
                    }
                } else if a & disk_win::A_DIR != 0 {
                    delete_dir(&p, r);
                } else {
                    clear_readonly(&p, &m);
                    match std::fs::remove_file(&p) {
                        Ok(()) => {
                            r.files += 1;
                            r.bytes += m.len();
                        }
                        Err(err) => r.fail(&p, &err),
                    }
                }
            }
        }
        Err(err) => {
            r.fail(dir, &err);
            return;
        }
    }
    if let Ok(m) = std::fs::symlink_metadata(dir) {
        clear_readonly(dir, &m);
    }
    match std::fs::remove_dir(dir) {
        Ok(()) => r.dirs += 1,
        Err(err) => {
            // A folder that still holds something that failed is already counted there.
            if std::fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false) {
                r.fail(dir, &err);
            }
        }
    }
}

/// `<vol>:\.AtherMemGuard-bin`
pub fn bin_root(volume_root: &str) -> PathBuf {
    PathBuf::from(format!("{}\\{}", volume_root.trim_end_matches('\\'), BIN_DIR))
}

/// Can the bin be made on this volume? Creates the bin folder if it is missing (it is ours).
pub fn bin_writable(volume_root: &str) -> bool {
    if cfg!(test) {
        return true;
    }
    let b = bin_root(volume_root);
    b.is_dir() || std::fs::create_dir(&b).is_ok()
}

/// Move `src` into `<bin>\<run>\<n>\<name>` on the same volume. Never copies: a move across
/// volumes fails instead.
pub fn move_to_bin(src: &Path, bin: &Path, run: &str, n: usize) -> Result<PathBuf, String> {
    let dir = bin.join(run).join(n.to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make the bin folder {}: {}", dir.display(), e))?;
    let name = src.file_name().ok_or("the folder has no name")?;
    let dst = dir.join(name);
    std::fs::rename(src, &dst).map_err(|e| {
        let _ = std::fs::remove_dir(&dir);
        format!("couldn't move it into the bin ({}); nothing was moved", e)
    })?;
    Ok(dst)
}

/// Look a program up on `path_var` with `pathext` (Windows style), like the shell would.
pub fn resolve_on_path(name: &str, path_var: &str, pathext: &str) -> Option<PathBuf> {
    let exts: Vec<String> = pathext.split(';').map(|e| e.trim().to_ascii_lowercase()).filter(|e| e.starts_with('.')).collect();
    for dir in path_var.split(';').map(str::trim).filter(|d| !d.is_empty()) {
        let dir = Path::new(dir.trim_matches('"'));
        for ext in &exts {
            let cand = dir.join(format!("{}{}", name, ext));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Run a tool's own clean command (2 minute limit). `path_var` overrides PATH for the lookup
/// (tests put a fake tool first).
pub fn run_tool(cmd: &ToolCommand, path_var: Option<&str>) -> Result<String, String> {
    let pv = path_var.map(str::to_string).unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    let pe = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let exe = resolve_on_path(cmd.exe, &pv, &pe).ok_or_else(|| format!("{} isn't installed or isn't on PATH", cmd.exe))?;
    match run_with_timeout(&exe, cmd.args, Duration::from_secs(120)) {
        Ok((0, _)) => Ok(format!("Ran {}", cmd.display())),
        Ok((code, _)) => Err(format!("{} stopped with exit code {}", cmd.display(), code)),
        Err(RunError::TimedOut) => Err(format!("{} didn't finish within 2 minutes", cmd.display())),
        Err(RunError::NotFound) => Err(format!("{} isn't installed", cmd.exe)),
        Err(RunError::Other(e)) => Err(e),
    }
}

// ---------------------------------------------------------------- undo log

/// One folder moved into the bin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinMove {
    pub from: String,
    pub to: String,
}

/// What undoing one action takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoStep {
    /// Move these folders back out of the bin.
    FromBin { volume_root: String, moves: Vec<BinMove>, frees_alone: Option<u64> },
    /// A permanent delete or a tool's clean (or a bin entry that was emptied): can't be undone.
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UndoEntry {
    /// `<run id>-<n>`.
    pub id: String,
    /// The checkup item id (`d3`).
    pub finding_id: String,
    pub title: String,
    pub action: CleanAction,
    pub step: UndoStep,
    #[serde(default)]
    pub undone: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UndoRun {
    pub id: String,
    pub time: String,
    #[serde(default)]
    pub unix: u64,
    pub entries: Vec<UndoEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UndoLog {
    pub runs: Vec<UndoRun>,
}

impl UndoLog {
    pub fn push_run(&mut self, run: UndoRun) {
        if run.entries.is_empty() {
            return;
        }
        self.runs.push(run);
        if self.runs.len() > UNDO_KEEP_RUNS {
            let extra = self.runs.len() - UNDO_KEEP_RUNS;
            self.runs.drain(..extra);
        }
    }

    /// (run, entry) indexes to undo: one by id, or every open bin move of the latest run that
    /// has any.
    pub fn select(&self, id: Option<&str>) -> Vec<(usize, usize)> {
        if let Some(id) = id {
            for (r, run) in self.runs.iter().enumerate() {
                if let Some(e) = run.entries.iter().position(|e| e.id == id || run.id == id) {
                    if run.id == id {
                        return run.entries.iter().enumerate().filter(|(_, e)| !e.undone && e.step != UndoStep::Nothing).map(|(e, _)| (r, e)).collect();
                    }
                    return vec![(r, e)];
                }
            }
            return Vec::new();
        }
        for (r, run) in self.runs.iter().enumerate().rev() {
            let open: Vec<(usize, usize)> = run.entries.iter().enumerate().filter(|(_, e)| !e.undone && e.step != UndoStep::Nothing).map(|(e, _)| (r, e)).collect();
            if !open.is_empty() {
                return open;
            }
        }
        Vec::new()
    }

    pub fn can_undo(&self) -> bool {
        !self.select(None).is_empty()
    }

    /// The id of the newest run that can still be undone.
    pub fn latest_undoable_run(&self) -> Option<&str> {
        self.select(None).first().map(|(r, _)| self.runs[*r].id.as_str())
    }
}

/// The result of acting on one item, or undoing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub title: String,
    pub action: CleanAction,
    /// "done", "refused", "failed", "partial", "would_do", "undone".
    pub result: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_id: Option<String>,
    /// Predicted to free now (bytes); 0 for a bin move.
    #[serde(default)]
    pub predicted_now: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<DeleteReport>,
}

/// Move entries back out of the bin. A folder that is back at its old place (rebuilt) is not
/// overwritten: the moved copy stays in the bin and the outcome says where.
pub fn undo(files: &DiskFiles, id: Option<&str>) -> Vec<Outcome> {
    let mut log = files.undo_log();
    let picked = log.select(id);
    if picked.is_empty() {
        let msg = match id {
            Some(i) => format!("Nothing to undo with id \"{}\".", i),
            None => "Nothing to undo: only moves into the bin can be undone, and none is waiting.".into(),
        };
        return vec![Outcome { id: id.unwrap_or("").into(), title: String::new(), action: CleanAction::Bin, result: "refused".into(), message: msg, undo_id: None, predicted_now: 0, deleted: None }];
    }
    let mut out = Vec::new();
    for (r, e) in picked {
        let entry = log.runs[r].entries[e].clone();
        let mut o = Outcome { id: entry.finding_id.clone(), title: entry.title.clone(), action: entry.action, result: "failed".into(), message: String::new(), undo_id: Some(entry.id.clone()), predicted_now: 0, deleted: None };
        if entry.undone {
            o.result = "refused".into();
            o.message = "Already undone".into();
            out.push(o);
            continue;
        }
        let UndoStep::FromBin { moves, .. } = &entry.step else {
            o.result = "refused".into();
            o.message = "A permanent delete can't be undone.".into();
            out.push(o);
            continue;
        };
        let mut problems = Vec::new();
        let mut back = 0;
        for m in moves {
            let (from, to) = (Path::new(&m.from), Path::new(&m.to));
            if !to.exists() {
                problems.push(format!("{} is no longer in the bin (it was emptied)", m.from));
            } else if from.exists() {
                problems.push(format!("something is at {} again (rebuilt?), so the moved copy stays in the bin at {}", m.from, m.to));
            } else {
                if let Some(parent) = from.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::rename(to, from) {
                    Ok(()) => {
                        back += 1;
                        if let Some(n) = to.parent() {
                            let _ = std::fs::remove_dir(n);
                            if let Some(run) = n.parent() {
                                let _ = std::fs::remove_dir(run); // only when empty
                            }
                        }
                    }
                    Err(err) => problems.push(format!("couldn't move {} back: {}", m.to, err)),
                }
            }
        }
        if problems.is_empty() {
            log.runs[r].entries[e].undone = true;
            o.result = "undone".into();
            o.message = "Put back".into();
            crate::log::write(&format!("disk: undid bin move of {}", entry.title));
        } else {
            if back > 0 && back == moves.len() {
                log.runs[r].entries[e].undone = true;
            }
            o.message = problems.join("; ");
        }
        out.push(o);
    }
    if let Err(e) = files.save_undo_log(&log) {
        crate::log::write(&format!("disk: could not write the undo log: {}", e));
    }
    out
}

// ---------------------------------------------------------------- the bin

/// One run's folder in a bin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinRun {
    pub volume_root: String,
    pub run_id: String,
    pub path: String,
    pub moved_unix: u64,
    pub titles: Vec<String>,
    /// Predicted to free when emptied (the plan's numbers for what was moved), bytes.
    pub frees_alone: u64,
    /// Bytes of files in it (sizes; they may share space with other files).
    pub holds: u64,
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            if m.file_type().is_symlink() {
                continue;
            }
            if m.is_dir() {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & disk_win::A_REPARSE == 0 {
                    stack.push(e.path());
                }
            } else {
                total += m.len();
            }
        }
    }
    total
}

/// Every bin run on every fixed volume, joined with what the undo log says about it.
pub fn bin_runs(files: &DiskFiles, volume_roots: &[String]) -> Vec<BinRun> {
    let log = files.undo_log();
    let mut out = Vec::new();
    for root in volume_roots {
        let bin = bin_root(root);
        let Ok(rd) = std::fs::read_dir(&bin) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let run_id = e.file_name().to_string_lossy().to_string();
            let run = log.runs.iter().find(|r| r.id == run_id);
            let mut titles = Vec::new();
            let mut frees = 0;
            if let Some(run) = run {
                for en in &run.entries {
                    if let UndoStep::FromBin { frees_alone, volume_root, .. } = &en.step {
                        if norm(volume_root) == norm(root) && !en.undone {
                            titles.push(en.title.clone());
                            frees += frees_alone.unwrap_or(0);
                        }
                    }
                }
            }
            let moved_unix = run.map(|r| r.unix).filter(|u| *u > 0).unwrap_or_else(|| {
                e.metadata().and_then(|m| m.created()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
            });
            out.push(BinRun { volume_root: root.clone(), run_id, path: p.display().to_string(), moved_unix, titles, frees_alone: frees, holds: dir_size(&p) });
        }
    }
    out.sort_by_key(|r| r.moved_unix);
    out
}

/// Runs older than `hold_hours` (the bin empties these on its own).
pub fn expired(runs: &[BinRun], hold_hours: f64, now_unix: u64) -> Vec<BinRun> {
    let hold = (hold_hours.max(0.0) * 3600.0) as u64;
    runs.iter().filter(|r| r.moved_unix + hold <= now_unix).cloned().collect()
}

/// Empty these bin runs for good. Their undo entries become "can't be undone".
pub fn empty_runs(files: &DiskFiles, runs: &[BinRun]) -> Vec<(BinRun, DeleteReport)> {
    let mut log = files.undo_log();
    let mut out = Vec::new();
    for r in runs {
        let p = Path::new(&r.path);
        // Only ever a run folder directly inside a bin.
        let ok = p.parent().is_some_and(|b| b.file_name().is_some_and(|n| n.to_string_lossy() == BIN_DIR));
        if !ok {
            continue;
        }
        let rep = delete_tree(p);
        if !rep.remains {
            if let Some(run) = log.runs.iter_mut().find(|x| x.id == r.run_id) {
                for e in run.entries.iter_mut() {
                    if matches!(&e.step, UndoStep::FromBin { volume_root, .. } if norm(volume_root) == norm(&r.volume_root)) {
                        e.step = UndoStep::Nothing;
                    }
                }
            }
        }
        crate::log::write(&format!("disk: emptied the bin run {} on {}: {}", r.run_id, r.volume_root, rep.words()));
        out.push((r.clone(), rep));
    }
    let _ = files.save_undo_log(&log);
    out
}

/// The roots of the fixed drives with a letter (`C:\`, `D:\`): where bins can be.
pub fn fixed_roots() -> Vec<String> {
    disk_win::volumes().into_iter().filter(|v| v.drive_type == 3).flat_map(|v| v.paths.into_iter().filter(|p| p.len() <= 3)).collect()
}

/// Empty bin runs and leave a measured watch per drive for what they were predicted to free
/// (the sum of the moved items' own numbers: an "at least").
pub fn empty_bins(files: &DiskFiles, runs: &[BinRun], presample_secs: u32, start_watcher: bool) -> (Vec<(BinRun, DeleteReport)>, Vec<WatchRecord>) {
    let mut roots: Vec<String> = runs.iter().map(|r| r.volume_root.clone()).collect();
    roots.sort();
    roots.dedup();
    let before: Vec<(String, Option<(u64, u64)>)> = roots.iter().map(|r| (r.clone(), presample(r, presample_secs.max(1)))).collect();
    let done = empty_runs(files, runs);
    let run = run_id();
    let mut watches = Vec::new();
    for (root, read) in before {
        let (free_before, motion) = read.unwrap_or((0, 0));
        let mine: Vec<&(BinRun, DeleteReport)> = done.iter().filter(|(r, _)| r.volume_root.eq_ignore_ascii_case(&root)).collect();
        let predicted: u64 = mine.iter().map(|(r, _)| r.frees_alone).sum();
        if predicted == 0 {
            continue;
        }
        let guid = disk_win::volume_of(&root).map(|(_, g)| g).unwrap_or_default();
        let fs = disk_win::volumes().into_iter().find(|v| v.guid.eq_ignore_ascii_case(&guid)).map(|v| v.fs).unwrap_or_default();
        let rec = WatchRecord {
            id: format!("{}-bin-{}", run, root.chars().next().unwrap_or('v')),
            run_id: run.clone(),
            volume: guid,
            volume_root: root.clone(),
            fs,
            started_unix: unix_now(),
            time: crate::win::local_time_string(),
            free_before,
            pre_motion: motion,
            predicted,
            predicted_confidence: "lower_bound".into(),
            titles: vec![format!("Ather MemGuard's bin on {}", root.trim_end_matches('\\'))],
            watcher_pid: 0,
            measured: unmeasurable_watch(&read),
            toasted: false,
        };
        let mut all = files.watches();
        all.push(rec.clone());
        let _ = files.save_watches(&all);
        if start_watcher {
            if let Some(pid) = spawn_watcher(&rec.id) {
                let _ = files.update_watch(&rec.id, |w| w.watcher_pid = w.watcher_pid.max(pid));
            }
        }
        watches.push(rec);
    }
    (done, watches)
}

/// The latest cleanups for `memguard disk` and `memguard_disk`: predicted, and measured once
/// the watcher is done.
pub fn recent_cleanups(files: &DiskFiles, n: usize) -> Vec<Value> {
    let all = files.watches();
    all.iter()
        .rev()
        .take(n)
        .map(|w| {
            json!({
                "run_id": w.run_id, "time": w.time, "volume": w.volume_root.trim_end_matches('\\'), "items": w.titles,
                "predicted_freed_gb": gb2(w.predicted), "predicted_confidence": w.predicted_confidence,
                "measured_free_space_rise_gb": w.measured.as_ref().filter(|m| !m.unreadable).map(|m| (m.measured_freed as f64 / (1u64 << 30) as f64 * 100.0).round() / 100.0),
                "measured_watched_sec": w.measured.as_ref().filter(|m| !m.unreadable).map(|m| m.watched_sec),
                "measured_meaning": MEASURED_MEANING,
                "measured": w.measured.as_ref().is_some_and(|m| !m.unreadable), "settled": w.measured.as_ref().map(|m| m.settled),
                "other_activity": w.measured.as_ref().map(|m| m.other_activity),
                "appeared_after_sec": w.measured.as_ref().and_then(|m| m.appeared_after_sec),
                "words": w.measured.as_ref().map(|m| measured_words(w.volume_root.trim_end_matches('\\'), w.predicted, m, w.fs.eq_ignore_ascii_case("ReFS")).1),
                "state": match &w.measured {
                    Some(m) if m.unreadable => "couldnt_measure",
                    Some(_) => "measured",
                    None if unix_now().saturating_sub(w.started_unix) > WATCH_MAX_SEC + 120 => "not_watched",
                    None => "watching",
                },
            })
        })
        .collect()
}

/// What the tray's disk poll does with the cleanup records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Housekeeping {
    /// Measured results not shown yet: (title, body).
    pub measured: Vec<(String, String)>,
    /// What the bin holds per drive: (`D:\`, bytes of files).
    pub bins: Vec<(String, u64)>,
    /// A child was started to empty bin runs past `hold_hours`.
    pub expiring: bool,
}

static LAST_EXPIRE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The tray's part, run on its 60-second disk poll (a worker thread): hand over measured
/// results once, restart a watcher whose process is gone, and empty bin runs older than
/// `hold_hours` in a hidden child (at most once an hour). Never empties anything else.
pub fn housekeeping(files: &DiskFiles, hold_hours: f64, live_pids: &[u32]) -> Housekeeping {
    let mut hk = Housekeeping::default();
    let mut all = files.watches();
    let mut changed = false;
    let now = unix_now();
    for w in all.iter_mut() {
        match &w.measured {
            Some(m) if !w.toasted => {
                let vol = w.volume_root.trim_end_matches('\\').to_string();
                hk.measured.push(measured_words(&vol, w.predicted, m, w.fs.eq_ignore_ascii_case("ReFS")));
                w.toasted = true;
                changed = true;
            }
            None if now.saturating_sub(w.started_unix) < WATCH_MAX_SEC && now.saturating_sub(w.started_unix) > 20 && !live_pids.contains(&w.watcher_pid) => {
                if let Some(pid) = spawn_watcher(&w.id) {
                    w.watcher_pid = pid;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    if changed {
        let _ = files.save_watches(&all);
    }
    let runs = bin_runs(files, &fixed_roots());
    for r in &runs {
        match hk.bins.iter_mut().find(|(root, _)| root.eq_ignore_ascii_case(&r.volume_root)) {
            Some(b) => b.1 += r.holds,
            None => hk.bins.push((r.volume_root.clone(), r.holds)),
        }
    }
    let last = LAST_EXPIRE.load(std::sync::atomic::Ordering::Relaxed);
    if !expired(&runs, hold_hours, now).is_empty() && now.saturating_sub(last) > 3600 && !cfg!(test) {
        LAST_EXPIRE.store(now, std::sync::atomic::Ordering::Relaxed);
        hk.expiring = spawn_cli_hidden(&["disk", "bin", "--expired", "--yes", "--json"]);
    }
    hk
}

/// Start `memguard.exe <args>` hidden and detached (the tray never deletes files itself).
pub fn spawn_cli_hidden(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL: u32 = 0x4000;
    let Some(exe) = crate::disk_scan::cli_exe() else { return false };
    disk_win::stop_std_handle_inheritance();
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | BELOW_NORMAL)
        .spawn()
        .is_ok()
}

/// Run `memguard.exe <args>` hidden and wait for its JSON on stdout (the checkup window's
/// cleanup and "Empty the bin" run the CLI, so the tray process never walks or deletes).
pub fn run_cli_json(args: &[&str], timeout: Duration) -> Result<Value, String> {
    let exe = crate::disk_scan::cli_exe().ok_or("memguard.exe was not found")?;
    match run_with_timeout(&exe, args, timeout) {
        Ok((_, out)) => serde_json::from_slice(&out).map_err(|_| format!("memguard {} gave no result", args.first().copied().unwrap_or(""))),
        Err(RunError::TimedOut) => Err("it took too long and was stopped".into()),
        Err(RunError::NotFound) => Err("memguard.exe was not found".into()),
        Err(RunError::Other(e)) => Err(e),
    }
}

// ---------------------------------------------------------------- the measured result (pure)

/// What came back after acting, as read from the volume's free space.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// Free space now minus free space just before acting (bytes; can be negative). This is an
    /// observation of the drive, not proof of what this cleanup freed: other activity on the
    /// drive (or a late ReFS return of earlier deletes) can add to it or hide it.
    #[serde(rename = "free_space_rise", alias = "measured_freed")]
    pub measured_freed: i64,
    /// The free space could not be read (before acting, or at any point after): nothing was
    /// measured, and no number is reported.
    #[serde(default)]
    pub unreadable: bool,
    /// The free space stopped moving after reaching the prediction.
    pub settled: bool,
    /// Free space was moving for other reasons too (before acting, or the wrong way after).
    pub other_activity: bool,
    /// Seconds until 90% of the prediction had shown up; `None` if it never did.
    pub appeared_after_sec: Option<u64>,
    pub watched_sec: u64,
}

/// Reads free space after acting, until the prediction has shown up and settled, or 30 minutes.
#[derive(Debug, Clone)]
pub struct Measure {
    before: u64,
    predicted: u64,
    pre_motion: u64,
    last: Vec<(u64, i64)>,
    high: i64,
    low: i64,
    drop_after_rise: bool,
    appeared: Option<u64>,
    sec: u64,
}

impl Measure {
    /// `before`: free space just before acting. `pre_motion`: how far free space moved in the
    /// seconds before acting (max - min), from other programs.
    pub fn new(before: u64, predicted: u64, pre_motion: u64) -> Measure {
        Measure { before, predicted, pre_motion, last: Vec::new(), high: 0, low: 0, drop_after_rise: false, appeared: None, sec: 0 }
    }

    /// Add a reading `sec` seconds after acting. True when the watch can stop.
    pub fn push(&mut self, sec: u64, avail: u64) -> bool {
        let d = avail as i64 - self.before as i64;
        self.sec = sec;
        if d < self.high - NOISE {
            self.drop_after_rise = true;
        }
        self.high = self.high.max(d);
        self.low = self.low.min(d);
        if self.appeared.is_none() && self.predicted > 0 && d >= (self.predicted as i64) * 9 / 10 {
            self.appeared = Some(sec);
        }
        self.last.push((sec, d));
        if self.last.len() > 3 {
            self.last.remove(0);
        }
        if sec >= WATCH_MAX_SEC {
            return true;
        }
        let quiet = self.agree();
        (self.appeared.is_some() && quiet) || (self.predicted == 0 && quiet && sec >= 30)
    }

    /// A moment with no reading (the free space couldn't be read). True when the watch can stop.
    pub fn skip(&mut self, sec: u64) -> bool {
        self.sec = sec;
        sec >= WATCH_MAX_SEC
    }

    fn agree(&self) -> bool {
        self.last.len() == 3 && {
            let lo = self.last.iter().map(|x| x.1).min().unwrap_or(0);
            let hi = self.last.iter().map(|x| x.1).max().unwrap_or(0);
            hi - lo <= AGREE
        }
    }

    pub fn result(&self) -> Measured {
        let d = self.last.last().map_or(0, |x| x.1);
        let p = self.predicted as i64;
        let overshoot = d > p + (p / 10).max(256 << 20);
        Measured {
            measured_freed: d,
            unreadable: self.last.is_empty(),
            settled: self.agree() && (self.appeared.is_some() || self.predicted == 0),
            other_activity: self.pre_motion as i64 > NOISE || self.low < -NOISE || self.drop_after_rise || overshoot,
            appeared_after_sec: self.appeared,
            watched_sec: self.sec,
        }
    }

    /// Quiet, stable, and well under the prediction: worth an `accounting_mismatch` log line.
    pub fn mismatch(&self) -> bool {
        let m = self.result();
        self.agree() && !m.other_activity && self.predicted > (256 << 20) && (m.measured_freed as f64) < self.predicted as f64 * 0.8
    }
}

/// What `measured_free_space_rise_gb` means, for agents.
pub const MEASURED_MEANING: &str = "How much the drive's free space rose in the minutes Ather MemGuard watched after the cleanup. It is an observation, \
not proof of what the cleanup freed: other activity on the drive can add to or hide it. Quote it as \"free space rose by X GB (measured)\", never as \"freed\".";

/// "the minute", "the 4 minutes": how long the watch ran, for the measured words.
fn minutes_words(sec: u64) -> String {
    let mins = sec.div_ceil(60).max(1);
    if mins == 1 {
        "the minute".into()
    } else {
        format!("the {} minutes", mins)
    }
}

/// The toast and log words for a measured result: (title, body). They state only what was
/// observed (free space rose on the drive), never that this cleanup caused it: other activity
/// on the drive, or ReFS returning earlier deletes late and in bulk, can add to it or hide it.
pub fn measured_words(volume: &str, predicted: u64, m: &Measured, refs: bool) -> (String, String) {
    let pred = gb2(predicted);
    if m.unreadable {
        return (
            format!("Couldn't measure the free space on {}", volume),
            format!("Ather MemGuard couldn't read the free space on {} around the cleanup, so nothing was measured. Predicted {:.1} GB.", volume, pred),
        );
    }
    let got = gb2(m.measured_freed.max(0) as u64);
    let when = minutes_words(m.watched_sec);
    let hedge = "(measured; other activity on the drive can add to or hide this)";
    let busy = format!(" Other programs were writing to {} at the same time, so the number isn't exact.", volume);
    // Far more than predicted: something else freed space too (on ReFS, possibly an earlier
    // delete arriving in bulk). Say so instead of claiming the number for this cleanup.
    if m.measured_freed > 0 && m.measured_freed as u64 > predicted.saturating_mul(2) + (1u64 << 30) {
        return (
            format!("Free space jumped on {}", volume),
            format!(
                "Free space on {} rose by {:.1} GB in {} after the cleanup (measured), far more than this cleanup's predicted {:.1} GB, so something else freed space at the same time. How much of the {:.1} GB came back can't be told apart.",
                volume, got, when, pred, pred
            ),
        );
    }
    let rose = if m.measured_freed > 0 { format!("rose by {:.1} GB", got) } else { "didn't rise".to_string() };
    let mut body = format!("Free space on {} {} in {} after the cleanup {}. Predicted {:.1} GB.", volume, rose, when, hedge, pred);
    if m.other_activity {
        body.push_str(&busy);
    }
    if m.appeared_after_sec.is_some() {
        (format!("Free space rose on {}", volume), body)
    } else {
        if refs {
            body.push_str(" This drive can return deleted space much later; memguard disk shows the free space now.");
        }
        (format!("Space hasn't come back yet on {}", volume), body)
    }
}

/// A watch the cleaner leaves for the watcher: one per volume acted on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchRecord {
    pub id: String,
    pub run_id: String,
    pub volume: String,
    pub volume_root: String,
    pub fs: String,
    pub started_unix: u64,
    pub time: String,
    pub free_before: u64,
    pub pre_motion: u64,
    pub predicted: u64,
    pub predicted_confidence: String,
    pub titles: Vec<String>,
    /// The process watching it (0: nobody yet).
    #[serde(default)]
    pub watcher_pid: u32,
    #[serde(default)]
    pub measured: Option<Measured>,
    /// The tray has shown its toast.
    #[serde(default)]
    pub toasted: bool,
}

/// Watch one record until it is done (blocking, up to 30 minutes from when it started), and
/// write the result back. `say` gets progress lines.
pub fn watch(files: &DiskFiles, id: &str, say: &mut dyn FnMut(&str)) -> Option<Measured> {
    let rec = files.watches().into_iter().find(|w| w.id == id)?;
    if let Some(m) = rec.measured {
        return Some(m);
    }
    let me = std::process::id();
    // Another live watcher on it already: leave it to that one.
    if rec.watcher_pid != 0 && rec.watcher_pid != me && crate::win::list_processes().iter().any(|p| p.pid == rec.watcher_pid && p.name.to_ascii_lowercase().starts_with("memguard")) {
        return None;
    }
    let _ = files.update_watch(id, |w| w.watcher_pid = me);
    let mut m = Measure::new(rec.free_before, rec.predicted, rec.pre_motion);
    loop {
        let sec = unix_now().saturating_sub(rec.started_unix);
        // A failed read is no reading (never "no change"); with none at all it is "couldn't measure".
        let done = match disk_win::free_space(&rec.volume_root) {
            Some(x) => m.push(sec.min(WATCH_MAX_SEC), x.0),
            None => m.skip(sec.min(WATCH_MAX_SEC)),
        };
        let r = m.result();
        if r.appeared_after_sec == Some(sec) {
            say(&format!("{}: free space has risen by {:.1} GB after {} s (measured)", rec.volume_root, gb2(r.measured_freed.max(0) as u64), sec));
        }
        if done {
            break;
        }
        std::thread::sleep(Duration::from_secs(if sec < 120 { 1 } else { 5 }));
    }
    let res = m.result();
    let vol = rec.volume_root.trim_end_matches('\\').to_string();
    let (t, b) = measured_words(&vol, rec.predicted, &res, rec.fs.eq_ignore_ascii_case("ReFS"));
    crate::log::write(&format!(
        "disk: measured after cleanup {}: {} {} (measured {:.2} GB, predicted {:.2} GB, settled {}, other_activity {}, appeared_after_sec {:?})",
        rec.run_id,
        t,
        b,
        res.measured_freed as f64 / (1u64 << 30) as f64,
        gb2(rec.predicted),
        res.settled,
        res.other_activity,
        res.appeared_after_sec
    ));
    if m.mismatch() {
        let ver = disk_win::volumes().into_iter().find(|v| v.guid.eq_ignore_ascii_case(&rec.volume)).and_then(|v| v.fs_version);
        crate::log::write(&format!(
            "disk: accounting_mismatch on {} ({} {:?}): measured {} bytes, predicted {} bytes",
            vol, rec.fs, ver, res.measured_freed, rec.predicted
        ));
    }
    let _ = files.update_watch(id, |w| w.measured = Some(res.clone()));
    say(&format!("{} {}", t, b));
    Some(res)
}

/// Start the watcher for `id` in its own hidden, detached process (`memguard disk watch-child`).
/// Returns its process id.
pub fn spawn_watcher(id: &str) -> Option<u32> {
    if cfg!(test) {
        return None;
    }
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL: u32 = 0x4000;
    let exe = crate::disk_scan::cli_exe()?;
    disk_win::stop_std_handle_inheritance();
    std::process::Command::new(exe)
        .args(["disk", "watch-child", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | BELOW_NORMAL)
        .spawn()
        .ok()
        .map(|c| c.id())
}

/// Free space on `root` read `n` times a second apart: (last reading, max - min). `None` when
/// a reading failed: the cleanup then reports "couldn't measure", never a number from a 0.
pub fn presample(root: &str, n: u32) -> Option<(u64, u64)> {
    let mut lo = u64::MAX;
    let mut hi = 0;
    let mut last = 0;
    for k in 0..n.max(1) {
        if k > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        last = disk_win::free_space(root)?.0;
        lo = lo.min(last);
        hi = hi.max(last);
    }
    Some((last, hi.saturating_sub(lo)))
}

/// The measured result of a watch whose starting free space couldn't be read.
fn unmeasurable_watch(read: &Option<(u64, u64)>) -> Option<Measured> {
    read.is_none().then(|| Measured { unreadable: true, ..Default::default() })
}

// ---------------------------------------------------------------- acting on a plan

/// How `clean` runs.
#[derive(Debug, Clone, Default)]
pub struct CleanOpts {
    /// An agent asked (MCP): regenerable items and `now` only, and the plan must be fresh.
    pub agent: bool,
    /// Check everything, act on nothing.
    pub dry_run: bool,
    /// Seconds of free-space readings before acting (to see other programs' writes).
    pub presample_secs: u32,
    /// Start the detached measured watcher (off in tests and for `--wait`, which watches itself).
    pub start_watcher: bool,
    /// PATH for finding a tool (tests).
    pub tool_path: Option<String>,
    /// Facts about Unreal processes; `None` reads them now.
    pub busy: Option<(Vec<String>, bool)>,
    /// Check git (off only in tests that don't need it).
    pub skip_git: bool,
}

/// The prediction for one volume after acting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumePrediction {
    pub volume: String,
    pub volume_root: String,
    pub fs: String,
    pub item_ids: Vec<String>,
    /// Bytes predicted to come back from the permanent deletes and tool cleans.
    pub predicted_freed: u64,
    /// exact, lower_bound, estimate, unknown.
    pub confidence: String,
    pub free_before: u64,
    #[serde(default)]
    pub watch_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CleanResult {
    pub run_id: String,
    pub plan_id: String,
    pub dry_run: bool,
    pub outcomes: Vec<Outcome>,
    pub volumes: Vec<VolumePrediction>,
}

impl CleanResult {
    /// The result as JSON for agents and `--json`, with every number labelled.
    pub fn to_json(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "plan_id": self.plan_id,
            "dry_run": self.dry_run,
            "outcomes": self.outcomes.iter().map(|o| json!({
                "id": o.id, "title": o.title, "action": o.action.name(), "result": o.result, "message": o.message,
                "undo_id": o.undo_id, "predicted_freed_gb": gb2(o.predicted_now), "basis": "predicted",
                "removed": o.deleted.as_ref().map(|d| json!({ "files": d.files, "gb": gb2(d.bytes), "links": d.links, "failed": d.failed_count, "remains": d.remains })),
            })).collect::<Vec<_>>(),
            "volumes": self.volumes.iter().map(|v| json!({
                "volume": v.volume_root.trim_end_matches('\\'), "item_ids": v.item_ids,
                "predicted_freed_gb": gb2(v.predicted_freed), "frees_confidence": v.confidence, "basis": "predicted",
                "free_before_gb": gb2(v.free_before), "measured": if v.watch_id.is_some() { json!("later") } else { Value::Null },
            })).collect::<Vec<_>>(),
            "measured_note": "The space is predicted, not yet measured. Ather MemGuard watches the drive for up to 30 minutes and reports how much its free space rose \
(a notification, the log, and memguard_disk's recent_cleanups). On ReFS, deleted space can take a long time to show up.",
            "bin_note": if self.outcomes.iter().any(|o| o.action == CleanAction::Bin && o.result == "done") {
                json!("Moving to the bin frees nothing until the bin is emptied (after 24 hours, or memguard disk bin --empty). Undo with memguard disk undo.")
            } else { Value::Null },
        })
    }
}

/// The rule facts for one planned item (catalogue, relocation record, settings, now).
pub fn item_check(p: &PlannedItem, s: &crate::config::DiskSettings, files: &DiskFiles, busy: &(Vec<String>, bool)) -> ItemCheck {
    let info = crate::disk_scan::catalog_info(&p.key);
    let mut c = ItemCheck {
        key: p.key.clone(),
        class: p.class.clone(),
        planned_ids: p.path_ids.clone(),
        planned_last_write: p.last_write,
        now_ft: crate::win::now_filetime(),
        min_age_hours: s.min_age_hours,
        busy: busy.0.clone(),
        busy_unknown: busy.1,
        ..Default::default()
    };
    match &info {
        Some(i) if i.global => {
            c.global = true;
            if let Some(base) = crate::disk_scan::expand_global(&i.path) {
                if i.older_than_days.is_some() {
                    c.allowed_parent = Some(norm(&base.display().to_string()));
                } else {
                    c.allowed_paths.push(norm(&base.display().to_string()));
                }
            }
        }
        Some(i) => {
            c.catalog_rel = Some(i.path.clone());
            c.plugins = i.plugins;
        }
        None if p.key == "cache:old" => {
            c.global = true;
            if let Some(r) = files.relocate_record().filter(|r| !r.undone) {
                c.allowed_paths = r.entries.iter().map(|e| norm(&e.old_location)).collect();
            }
        }
        None => {}
    }
    c
}

/// Act on picked items of a plan. Every rule is checked again now; an item that breaks one is
/// refused whole. Bin moves go to the undo log; permanent deletes and tool cleans can't be
/// undone. The prediction comes back at once; the measured result comes from the watcher.
pub fn clean(cfg: &crate::config::Config, files: &DiskFiles, plan: &PlanRecord, picks: &[(String, Option<CleanAction>)], opts: &CleanOpts) -> CleanResult {
    let s = cfg.disk();
    let run = run_id();
    let busy = opts.busy.clone().unwrap_or_else(crate::disk_scan::busy_state);
    let profile = std::env::var("USERPROFILE").ok();
    let syncs = sync_roots();
    let mut outcomes: Vec<Outcome> = Vec::new();
    // (outcome index, item, action) that passed every check.
    let mut ready: Vec<(usize, PlannedItem, CleanAction)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for (id, want) in picks {
        let refuse = |title: &str, action: CleanAction, msg: String| Outcome { id: id.clone(), title: title.into(), action, result: "refused".into(), message: msg, undo_id: None, predicted_now: 0, deleted: None };
        if seen.iter().any(|x| x.eq_ignore_ascii_case(id)) {
            continue;
        }
        seen.push(id.clone());
        let Some(planned) = plan.item(id) else {
            outcomes.push(refuse("", want.unwrap_or(CleanAction::Now), format!("{} is not in plan {}. Make a plan with it first.", id, plan.id)));
            continue;
        };
        // The class and the tool come from the built-in catalogue, by key: a plan or cache file
        // that says otherwise was changed by someone.
        let Some((class, tool)) = catalog_class(&planned.key) else {
            outcomes.push(refuse(&planned.title, want.unwrap_or(CleanAction::Now), "It isn't on the list of folders Ather MemGuard may clean.".into()));
            continue;
        };
        if planned.class != class {
            outcomes.push(refuse(&planned.title, want.unwrap_or(CleanAction::Now), "The plan doesn't match Ather MemGuard's list of what may be cleaned (its class was changed). Make a new plan.".into()));
            continue;
        }
        let mut item = planned.clone();
        item.tool = tool;
        let item = &item;
        if opts.agent && !agent_may_clean(&item.key) {
            outcomes.push(refuse(&item.title, want.unwrap_or(CleanAction::Now), "Agents may only delete regenerable build output and caches (action now). Ask the person to do the rest.".into()));
            continue;
        }
        let offered = offered_actions(&item.key, &item.class, item.tool.as_deref(), item.volume_root.is_some());
        let action = match want.or_else(|| offered.first().copied()) {
            Some(a) => a,
            None => {
                outcomes.push(refuse(&item.title, CleanAction::Now, no_action_reason(&item.key, &item.class).unwrap_or("Nothing can be done with it.").into()));
                continue;
            }
        };
        if !offered.contains(&action) {
            outcomes.push(refuse(&item.title, action, format!("\"{}\" isn't offered for this item.", action.name())));
            continue;
        }
        if action == CleanAction::Bin && !opts.dry_run && !item.volume_root.as_deref().is_some_and(bin_writable) {
            outcomes.push(refuse(&item.title, action, "The bin can't be made on that drive (its top folder isn't writable), so it can only be deleted.".into()));
            continue;
        }
        if opts.agent && (item.class != "regenerable" || action != CleanAction::Now) {
            outcomes.push(refuse(&item.title, action, "Agents may only delete regenerable build output and caches (action now). Ask the person to do the rest.".into()));
            continue;
        }
        if let Some(why) = &item.unmeasurable {
            outcomes.push(refuse(&item.title, action, format!("It couldn't be measured: {}.", why)));
            continue;
        }
        let mut check = item_check(item, &s, files, &busy);
        let facts: Vec<PathFacts> = item.paths.iter().map(|p| path_facts(p, check.global, profile.as_deref(), &syncs, !opts.skip_git)).collect();
        check.now_last_write = newest_write(&item.paths, Duration::from_secs(120));
        if let Some(why) = refusal(&check, &facts) {
            outcomes.push(refuse(&item.title, action, why));
            continue;
        }
        let o = Outcome { id: item.id.clone(), title: item.title.clone(), action, result: "would_do".into(), message: String::new(), undo_id: None, predicted_now: 0, deleted: None };
        outcomes.push(o);
        ready.push((outcomes.len() - 1, item.clone(), action));
    }

    if opts.dry_run {
        for (k, item, action) in &ready {
            outcomes[*k].message = match action {
                CleanAction::Now => format!("Would delete it for good; predicted to free {}.", gb_words(item.frees_alone, &item.confidence)),
                CleanAction::Bin => "Would move it to the bin (frees nothing until the bin is emptied).".into(),
                CleanAction::Tool => format!("Would run {} (can't be undone).", item.tool.as_deref().and_then(tool_command).map(|c| c.display()).unwrap_or_default()),
            };
        }
        return CleanResult { run_id: run, plan_id: plan.id.clone(), dry_run: true, outcomes, volumes: Vec::new() };
    }

    // Free space just before acting, on every volume that should get space back.
    let mut before: Vec<(String, Option<(u64, u64)>)> = Vec::new();
    for (_, item, action) in &ready {
        if *action == CleanAction::Bin {
            continue;
        }
        if let Some(root) = &item.volume_root {
            if !before.iter().any(|(r, _)| r.eq_ignore_ascii_case(root)) {
                before.push((root.clone(), presample(root, opts.presample_secs.max(1))));
            }
        }
    }

    let mut entries: Vec<UndoEntry> = Vec::new();
    let mut tools_run: Vec<String> = Vec::new();
    let mut n = 0;
    for (k, item, action) in &ready {
        n += 1;
        let entry_id = format!("{}-{}", run, n);
        let o = &mut outcomes[*k];
        match action {
            CleanAction::Now => {
                let mut total = DeleteReport::default();
                for p in &item.paths {
                    let r = delete_tree(Path::new(p));
                    total.files += r.files;
                    total.dirs += r.dirs;
                    total.bytes += r.bytes;
                    total.links += r.links;
                    total.failed_count += r.failed_count;
                    total.failed.extend(r.failed.into_iter().take(20usize.saturating_sub(total.failed.len())));
                    total.remains |= r.remains;
                }
                o.result = if total.remains { if total.files > 0 || total.dirs > 0 { "partial" } else { "failed" } } else { "done" }.into();
                o.message = if total.remains { total.words() } else { format!("Deleted for good. {}.", total.words()) };
                // A partial delete frees only part of the prediction: scale it to what went (an
                // estimate), so the watch never waits for space that was never deleted.
                o.predicted_now = if o.result == "failed" {
                    0
                } else if total.remains {
                    let left: u64 = item.paths.iter().map(|p| dir_size(Path::new(p))).sum();
                    scaled_prediction(item.frees_alone, total.bytes, left)
                } else {
                    item.frees_alone.unwrap_or(0)
                };
                o.deleted = Some(total);
                entries.push(UndoEntry { id: entry_id, finding_id: item.id.clone(), title: item.title.clone(), action: *action, step: UndoStep::Nothing, undone: false });
            }
            CleanAction::Bin => {
                let root = item.volume_root.clone().unwrap_or_default();
                let bin = bin_root(&root);
                let mut moves = Vec::new();
                let mut err = None;
                for (j, p) in item.paths.iter().enumerate() {
                    match move_to_bin(Path::new(p), &bin, &run, n * 1000 + j) {
                        Ok(dst) => moves.push(BinMove { from: p.clone(), to: dst.display().to_string() }),
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
                if !moves.is_empty() {
                    o.undo_id = Some(entry_id.clone());
                    entries.push(UndoEntry {
                        id: entry_id,
                        finding_id: item.id.clone(),
                        title: item.title.clone(),
                        action: *action,
                        step: UndoStep::FromBin { volume_root: root.clone(), moves: moves.clone(), frees_alone: item.frees_alone },
                        undone: false,
                    });
                }
                match err {
                    None => {
                        o.result = "done".into();
                        o.message = format!("Moved to the bin on {}. It frees nothing until the bin is emptied; undo with memguard disk undo.", root.trim_end_matches('\\'));
                    }
                    Some(e) if moves.is_empty() => {
                        o.result = "failed".into();
                        o.message = e;
                    }
                    Some(e) => {
                        o.result = "partial".into();
                        o.message = format!("{} of {} folders moved to the bin; then {}.", moves.len(), item.paths.len(), e);
                    }
                }
            }
            CleanAction::Tool => {
                let cmd = item.tool.as_deref().and_then(tool_command);
                match cmd {
                    Some(c) if tools_run.contains(&c.exe.to_string()) => {
                        o.result = "done".into();
                        o.message = format!("Cleaned by {} (already run).", c.display());
                    }
                    Some(c) => {
                        tools_run.push(c.exe.to_string());
                        match run_tool(&c, opts.tool_path.as_deref()) {
                            Ok(msg) => {
                                o.result = "done".into();
                                o.message = format!("{}. This can't be undone.", msg);
                                o.predicted_now = item.frees_alone.unwrap_or(0);
                            }
                            Err(e) => {
                                o.result = "failed".into();
                                o.message = e;
                            }
                        }
                    }
                    None => {
                        o.result = "refused".into();
                        o.message = "No clean command is known for this tool.".into();
                    }
                }
                entries.push(UndoEntry { id: entry_id, finding_id: item.id.clone(), title: item.title.clone(), action: *action, step: UndoStep::Nothing, undone: false });
            }
        }
        crate::log::write(&format!("disk: {} {} ({}): {} {}", action.name(), item.title, item.paths.join("; "), o.result, o.message));
    }

    let mut log = files.undo_log();
    log.push_run(UndoRun { id: run.clone(), time: crate::win::local_time_string(), unix: unix_now(), entries });
    if let Err(e) = files.save_undo_log(&log) {
        crate::log::write(&format!("disk: could not write the undo log: {}", e));
    }

    // The prediction per volume: the plan's set total when exactly that set went, else the
    // sum of the items' own numbers, which can only be lower (an "at least").
    let mut volumes: Vec<VolumePrediction> = Vec::new();
    for (root, read) in &before {
        let (free_before, motion) = &read.unwrap_or((0, 0));
        let done: Vec<&(usize, PlannedItem, CleanAction)> = ready
            .iter()
            .filter(|(k, it, a)| {
                *a != CleanAction::Bin && it.volume_root.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(root)) && matches!(outcomes[*k].result.as_str(), "done" | "partial")
            })
            .collect();
        if done.is_empty() {
            continue;
        }
        let ids: Vec<String> = done.iter().map(|(_, it, _)| it.id.clone()).collect();
        let partial = done.iter().any(|(k, _, _)| outcomes[*k].result != "done");
        let pv = plan.volumes.iter().find(|v| v.root.eq_ignore_ascii_case(root)).filter(|_| !partial);
        let same_set = pv.is_some_and(|v| {
            let mut a: Vec<String> = v.item_ids.iter().map(|x| x.to_ascii_lowercase()).collect();
            let mut b: Vec<String> = ids.iter().map(|x| x.to_ascii_lowercase()).collect();
            a.sort();
            b.sort();
            a == b
        });
        let (bytes, mut conf) = match pv.filter(|_| same_set).and_then(|v| v.frees_together.map(|t| (t, v.confidence.clone()))) {
            Some(x) => x,
            None => {
                // Each item's number as acted on: its own prediction, scaled down after a partial delete.
                let sum: u64 = done.iter().map(|(k, _, _)| outcomes[*k].predicted_now).sum();
                let weakest = if done.iter().any(|(_, it, _)| it.frees_alone.is_none() || it.confidence == "unknown" || it.confidence == "upper_bound") {
                    "unknown"
                } else if done.iter().any(|(_, it, _)| it.confidence == "estimate") {
                    "estimate"
                } else {
                    "lower_bound"
                };
                (sum, weakest.to_string())
            }
        };
        if partial && conf != "unknown" {
            conf = "estimate".into();
        }
        let fs = plan.volumes.iter().find(|v| v.root.eq_ignore_ascii_case(root)).map(|v| v.fs.clone()).unwrap_or_else(|| {
            done.first().and_then(|(_, it, _)| it.volume.clone()).and_then(|g| disk_win::volumes().into_iter().find(|v| v.guid.eq_ignore_ascii_case(&g)).map(|v| v.fs)).unwrap_or_default()
        });
        let guid = done.first().and_then(|(_, it, _)| it.volume.clone()).unwrap_or_default();
        let mut vp = VolumePrediction { volume: guid.clone(), volume_root: root.clone(), fs: fs.clone(), item_ids: ids, predicted_freed: bytes, confidence: conf.clone(), free_before: *free_before, watch_id: None };
        if bytes > 0 {
            let wid = format!("{}-{}", run, root.chars().next().unwrap_or('v'));
            let rec = WatchRecord {
                id: wid.clone(),
                run_id: run.clone(),
                volume: guid,
                volume_root: root.clone(),
                fs,
                started_unix: unix_now(),
                time: crate::win::local_time_string(),
                free_before: *free_before,
                pre_motion: *motion,
                predicted: bytes,
                predicted_confidence: conf,
                titles: done.iter().map(|(_, it, _)| it.title.clone()).collect(),
                watcher_pid: 0,
                measured: unmeasurable_watch(read),
                toasted: false,
            };
            let mut all = files.watches();
            all.push(rec);
            let _ = files.save_watches(&all);
            if opts.start_watcher {
                if let Some(pid) = spawn_watcher(&wid) {
                    let _ = files.update_watch(&wid, |w| w.watcher_pid = w.watcher_pid.max(pid));
                }
            }
            vp.watch_id = Some(wid);
        }
        crate::log::write(&format!(
            "disk: cleanup {} on {}: predicted {} ({}); free before {:.2} GB",
            run,
            root,
            gb_words(Some(vp.predicted_freed), &vp.confidence),
            vp.confidence,
            gb2(vp.free_before)
        ));
        volumes.push(vp);
    }
    CleanResult { run_id: run, plan_id: plan.id.clone(), dry_run: false, outcomes, volumes }
}

/// The prediction for an item after only part of it went: its own number scaled by the share of
/// its bytes that was removed (`removed` of `removed + left`). An estimate, never the whole.
pub fn scaled_prediction(frees_alone: Option<u64>, removed: u64, left: u64) -> u64 {
    let Some(f) = frees_alone else { return 0 };
    let all = removed as u128 + left as u128;
    if all == 0 {
        return 0;
    }
    ((f as u128 * removed as u128) / all).min(f as u128) as u64
}

/// "16.4 GB", "at least 16.4 GB", "about 16.4 GB", or "an unknown amount".
pub fn gb_words(bytes: Option<u64>, confidence: &str) -> String {
    match bytes {
        None => "an unknown amount".into(),
        Some(b) => {
            let q = match confidence {
                "lower_bound" => "at least ",
                "estimate" => "about ",
                "upper_bound" => "at most ",
                _ => "",
            };
            format!("{}{}", q, size_words(b))
        }
    }
}

/// `clean` as text for the CLI.
pub fn render_clean(r: &CleanResult) -> String {
    let mut s = String::new();
    for o in &r.outcomes {
        let what = if o.title.is_empty() { o.id.clone() } else { format!("{} {}", o.id, o.title) };
        let tag = match o.result.as_str() {
            "done" => "",
            "would_do" => "would do: ",
            "refused" => "NOT DONE: ",
            "partial" => "PARTLY DONE: ",
            _ => "FAILED: ",
        };
        s.push_str(&format!("  {:<44} {}{}\n", what, tag, o.message));
    }
    for v in &r.volumes {
        s.push_str(&format!(
            "\nPredicted to free {} on {} ({}; not measured yet).\n",
            gb_words(Some(v.predicted_freed), &v.confidence),
            v.volume_root.trim_end_matches('\\'),
            v.confidence
        ));
    }
    if r.volumes.iter().any(|v| v.watch_id.is_some()) {
        s.push_str("Ather MemGuard watches the free space for up to 30 minutes and reports how much it rose (measured; other activity on the drive can add to or hide this) in a notification and the log.\n");
    }
    if r.outcomes.iter().any(|o| o.action == CleanAction::Bin && o.result == "done") {
        s.push_str("Moving to the bin frees nothing until the bin is emptied (after 24 hours, or memguard disk bin --empty). Undo with memguard disk undo.\n");
    }
    s
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3600 * FT_PER_SEC;

    fn proj_facts(rel: &str) -> PathFacts {
        let path = format!(r"D:\proj\{}", rel);
        PathFacts {
            path: path.clone(),
            exists: true,
            chain: vec![(path, false), (r"D:\proj".into(), false)],
            recognised_root: Some(r"D:\proj".into()),
            rel: Some(rel.into()),
            git: Some(GitCheck::Clean),
            file_id: Some(7),
            ..Default::default()
        }
    }

    fn proj_item(key: &str, class: &str, rel: &str) -> ItemCheck {
        let now = 1_000_000 * H;
        ItemCheck {
            key: key.into(),
            class: class.into(),
            catalog_rel: Some(rel.into()),
            planned_ids: vec![Some(7)],
            planned_last_write: now - 10 * H,
            now_last_write: Some(now - 10 * H),
            now_ft: now,
            min_age_hours: 2.0,
            ..Default::default()
        }
    }

    #[test]
    fn a_clean_project_item_passes() {
        assert_eq!(refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[proj_facts("Intermediate")]), None);
    }

    #[test]
    fn rule_allow_list_and_recognised_root() {
        // Not under a .uproject folder or an engine.
        let mut f = proj_facts("Intermediate");
        f.recognised_root = None;
        f.rel = None;
        assert!(refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[f]).unwrap().contains(".uproject"));
        // Under a project, but not at the catalogue path.
        let r = refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[proj_facts(r"Saved\Temp")]).unwrap();
        assert!(r.contains("list of folders"), "{}", r);
        // Plugin items: any plugin folder's Intermediate, never the plugin's Content.
        assert!(rel_matches_catalog(r"Plugins\Foo\Intermediate", "Intermediate", true));
        assert!(rel_matches_catalog(r"Plugins\Group\Foo\Intermediate", "Intermediate", true));
        assert!(!rel_matches_catalog(r"Plugins\Foo\Content", "Intermediate", true));
        assert!(!rel_matches_catalog("Intermediate", "Intermediate", true));
        assert!(rel_matches_catalog(r"saved\logs", r"Saved\Logs", false));
        // Engine roots are recognised (folder named Engine with Build\Build.version).
        let tmp = std::env::temp_dir().join(format!("amg-clean-engine-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(r"Engine\Build")).unwrap();
        std::fs::write(tmp.join(r"Engine\Build\Build.version"), b"{}").unwrap();
        assert!(is_recognised_root(&tmp.join("Engine")));
        assert!(!is_recognised_root(&tmp));
        std::fs::write(tmp.join("Game.uproject"), b"{}").unwrap();
        assert!(is_recognised_root(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rule_never_list() {
        for rel in ["Content", r"Content\Maps", "Config", "Source", r"Plugins\Foo\Content", ".git", r".git\lfs\objects", r"Saved\Autosaves", r"Saved\SaveGames",
            r"Saved\Config", r"Saved\Collections", r"Saved\SourceControl", "Binaries", r"Plugins\Foo\Binaries", "", "Saved", "Plugins"]
        {
            assert!(never_touch(rel).is_some(), "{} must be refused", rel);
            let mut it = proj_item("ue:intermediate", "regenerable", rel);
            it.catalog_rel = Some(rel.into());
            assert!(refusal(&it, &[proj_facts(rel)]).is_some(), "{}", rel);
        }
        for rel in ["Intermediate", "DerivedDataCache", r"Saved\Logs", r"Saved\Crashes", r"Saved\ShaderDebugInfo", r"Saved\Cooked", r"Plugins\Foo\Intermediate"] {
            assert_eq!(never_touch(rel), None, "{}", rel);
        }
    }

    #[test]
    fn rule_outside_profile_and_projects() {
        let mut it = proj_item("cache:npm", "regenerable", "");
        it.global = true;
        it.catalog_rel = None;
        it.allowed_paths = vec![norm(r"C:\Users\me\AppData\Local\npm-cache\_cacache")];
        let mut f = PathFacts { path: r"C:\Users\me\AppData\Local\npm-cache\_cacache".into(), exists: true, in_profile: true, file_id: Some(7), git: Some(GitCheck::NotRepo), ..Default::default() };
        assert_eq!(refusal(&it, &[f.clone()]), None);
        f.in_profile = false;
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("outside your user folder"));
        // In the profile but not where the catalogue says.
        f.in_profile = true;
        f.path = r"C:\Users\me\Documents".into();
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("knows is safe"));
        // TEMP entries: direct children of the TEMP folder only.
        it.allowed_paths.clear();
        it.allowed_parent = Some(norm(r"C:\Users\me\AppData\Local\Temp"));
        f.path = r"C:\Users\me\AppData\Local\Temp\old-build".into();
        assert_eq!(refusal(&it, &[f.clone()]), None);
        f.path = r"C:\Users\me\AppData\Local\Temp\a\b".into();
        assert!(refusal(&it, &[f]).is_some());
    }

    #[test]
    fn rule_reparse_points_in_the_chain() {
        let it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        let mut f = proj_facts("Intermediate");
        f.chain[0].1 = true; // the item root is a junction
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("is a link"));
        let mut g = proj_facts(r"Plugins\Foo\Intermediate");
        g.chain = vec![(r"D:\proj\Plugins\Foo\Intermediate".into(), false), (r"D:\proj\Plugins\Foo".into(), true), (r"D:\proj\Plugins".into(), false), (r"D:\proj".into(), false)];
        let mut pit = proj_item("ue:plugin_intermediate", "regenerable", "Intermediate");
        pit.plugins = true;
        assert!(refusal(&pit, &[g.clone()]).unwrap().contains(r"D:\proj\Plugins\Foo is a link"), "a folder between it and the project root");
        g.chain[1].1 = false;
        assert_eq!(refusal(&pit, &[g]), None);
    }

    #[test]
    fn rule_cloud_sync_roots() {
        let it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        let mut f = proj_facts("Intermediate");
        f.sync_root = Some(r"d:\onedrive".into());
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("cloud-synced"));
        f.sync_root = None;
        f.cloud_attrs = true;
        assert!(refusal(&it, &[f]).unwrap().contains("cloud-synced"));
    }

    #[test]
    fn rule_recent_writes_refuse_the_whole_item() {
        let mut it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        it.now_last_write = Some(it.now_ft - H); // 1 hour ago, min age 2 hours
        it.planned_last_write = it.now_ft - H;
        let r = refusal(&it, &[proj_facts("Intermediate")]).unwrap();
        assert!(r.contains("last 2 hours") && r.contains("Nothing in it was touched"), "{}", r);
        // Couldn't read everything: refused too.
        it.now_last_write = None;
        assert!(refusal(&it, &[proj_facts("Intermediate")]).unwrap().contains("couldn't check every file"));
        // Written after the plan (but long enough ago).
        let mut it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        it.now_last_write = Some(it.now_ft - 3 * H);
        it.planned_last_write = it.now_ft - 5 * H;
        assert!(refusal(&it, &[proj_facts("Intermediate")]).unwrap().contains("after the plan"));
    }

    #[test]
    fn rule_unreal_working_in_the_project() {
        let mut it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        it.busy = vec![norm(r"D:\proj")];
        assert!(refusal(&it, &[proj_facts("Intermediate")]).unwrap().contains("editor, build or cook"));
        it.busy = vec![norm(r"D:\other")];
        assert_eq!(refusal(&it, &[proj_facts("Intermediate")]), None);
        it.busy_unknown = true;
        assert!(refusal(&it, &[proj_facts("Intermediate")]).unwrap().contains("can't tell"));
    }

    #[test]
    fn rule_file_id_must_match_the_plan() {
        let it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        let mut f = proj_facts("Intermediate");
        f.file_id = Some(8);
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("file id changed"));
        f.file_id = None;
        assert!(refusal(&it, &[f]).is_some());
    }

    #[test]
    fn rule_git_tracked_and_timeout() {
        let it = proj_item("ue:intermediate", "regenerable", "Intermediate");
        let mut f = proj_facts("Intermediate");
        f.git = Some(GitCheck::Tracked(3));
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("Git tracks 3 files"));
        f.git = Some(GitCheck::TimedOut);
        assert!(refusal(&it, &[f.clone()]).unwrap().contains("5 seconds"));
        f.git = Some(GitCheck::Failed("x".into()));
        assert!(refusal(&it, &[f.clone()]).is_some());
        f.git = Some(GitCheck::NotRepo);
        assert_eq!(refusal(&it, &[f]), None);
    }

    #[test]
    fn git_check_on_a_real_repo() {
        let root = std::env::temp_dir().join(format!("amg-clean-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(r"Saved\Logs")).unwrap();
        std::fs::create_dir_all(root.join("Intermediate")).unwrap();
        std::fs::write(root.join(r"Saved\Logs\tracked.log"), b"x").unwrap();
        std::fs::write(root.join(r"Intermediate\a.bin"), b"x").unwrap();
        let r = |args: &[&str]| run_with_timeout(Path::new("git"), args, Duration::from_secs(20));
        let rs = root.display().to_string();
        if r(&["-C", &rs, "init", "-q"]).is_err() {
            return; // no git on this machine: the pure rule test covers it
        }
        r(&["-C", &rs, "add", "Saved/Logs/tracked.log"]).unwrap();
        assert_eq!(git_check(&root.join(r"Saved\Logs")), GitCheck::Tracked(1));
        assert_eq!(git_check(&root.join("Intermediate")), GitCheck::Clean);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(git_check(&std::env::temp_dir().join("amg-surely-not-a-repo-x\\y")), GitCheck::NotRepo);
    }

    /// Found in review: git tracked `intermediate/Build/tracked.txt`, the item was cleaned as
    /// `...\Game1\Intermediate`, and the case-sensitive pathspec said "not tracked".
    #[test]
    fn git_check_ignores_case_like_windows_does() {
        let root = std::env::temp_dir().join(format!("amg-clean-gitcase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let game = root.join("Game1");
        std::fs::create_dir_all(game.join(r"intermediate\Build")).unwrap();
        std::fs::write(game.join(r"intermediate\Build\tracked.txt"), b"x").unwrap();
        let r = |args: &[&str]| run_with_timeout(Path::new("git"), args, Duration::from_secs(20));
        let gs = game.display().to_string();
        if r(&["-C", &gs, "init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        r(&["-C", &gs, "add", "intermediate/Build/tracked.txt"]).unwrap();
        assert_eq!(git_check(&game.join("Intermediate")), GitCheck::Tracked(1), "the catalogue's spelling must still find it");
        assert_eq!(git_check(&game.join("INTERMEDIATE")), GitCheck::Tracked(1));
        // Pathspec characters in a folder name are taken literally.
        std::fs::create_dir_all(game.join("Saved").join("[ab]")).unwrap();
        assert_eq!(git_check(&game.join("Saved").join("[ab]")), GitCheck::Clean);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A nested repository between the item and the project root: the outer checkout still
    /// tracks a file in the item, and the inner (empty) one must not hide it.
    #[test]
    fn git_check_asks_every_repository_above_the_item() {
        let root = std::env::temp_dir().join(format!("amg-clean-gitnest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let game = root.join("Game1");
        let foo = game.join(r"Plugins\Foo");
        std::fs::create_dir_all(foo.join("Intermediate")).unwrap();
        std::fs::write(foo.join(r"Intermediate\kept.txt"), b"x").unwrap();
        let r = |args: &[&str]| run_with_timeout(Path::new("git"), args, Duration::from_secs(20));
        let gs = game.display().to_string();
        if r(&["-C", &gs, "init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        r(&["-C", &gs, "add", "Plugins/Foo/Intermediate/kept.txt"]).unwrap();
        r(&["-C", &foo.display().to_string(), "init", "-q"]).unwrap();
        assert_eq!(git_check(&foo.join("Intermediate")), GitCheck::Tracked(1));
        // A repository inside the item: the walk finds it, and the rule refuses it.
        let lane = game.join("Intermediate");
        std::fs::create_dir_all(lane.join(r"Deep\Sub\.git")).unwrap();
        assert_eq!(holds_repo(&lane, Instant::now() + Duration::from_secs(10)), Some(true));
        assert_eq!(holds_repo(&foo.join("Intermediate"), Instant::now() + Duration::from_secs(10)), Some(false));
        assert_eq!(holds_repo(&lane, Instant::now() - Duration::from_secs(1)), None, "out of time: unknown, not false");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// git refusing the repository (another user owns it) is a refusal, never "not tracked".
    #[test]
    fn git_safe_directory_failure_refuses() {
        let root = std::env::temp_dir().join(format!("amg-clean-gitsafe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Intermediate")).unwrap();
        let r = |args: &[&str]| run_with_timeout(Path::new("git"), args, Duration::from_secs(20));
        if r(&["-C", &root.display().to_string(), "init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let g = git_check_env(&root.join("Intermediate"), &[("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")]);
        assert!(matches!(&g, GitCheck::Failed(w) if w.contains("safe.directory")), "{:?}", g);
        let mut f = proj_facts("Intermediate");
        f.git = Some(g);
        assert!(refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[f]).unwrap().contains("couldn't ask git"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn git_answers_are_read_strictly() {
        assert_eq!(git_answer(0, b"", b""), GitCheck::Clean);
        assert_eq!(git_answer(0, b"a\0b\0", b""), GitCheck::Tracked(2));
        let g = git_answer(128, b"", b"fatal: detected dubious ownership in repository at 'D:/x'\nTo add an exception...");
        assert!(matches!(&g, GitCheck::Failed(w) if w.contains("another user")), "{:?}", g);
        assert!(matches!(git_answer(128, b"", b"fatal: not a git repository\n"), GitCheck::Failed(w) if w.contains("not a git repository")));
        let mut f = proj_facts("Intermediate");
        f.holds_repo = true;
        assert!(refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[f.clone()]).unwrap().contains("git repository"));
        f.holds_repo = false;
        f.repo_walk_incomplete = true;
        assert!(refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[f]).is_some());
    }

    #[test]
    fn lanes_worktrees_zen_and_review_items_are_never_acted_on() {
        for (key, class) in [("lane", "review"), ("ue:zen", "rebuild_cost"), ("git:lfs", "review"), ("memguard:bin", "review")] {
            assert!(offered_actions(key, class, None, true).is_empty(), "{}", key);
            let mut it = proj_item(key, class, "x");
            it.catalog_rel = Some("x".into());
            assert!(refusal(&it, &[proj_facts("x")]).is_some(), "{}", key);
        }
        // A checkout found where a cache was expected.
        let mut f = proj_facts("Intermediate");
        f.is_checkout = true;
        assert!(refusal(&proj_item("ue:intermediate", "regenerable", "Intermediate"), &[f]).unwrap().contains("never removes"));
    }

    #[test]
    fn default_actions_follow_the_class() {
        assert_eq!(offered_actions("ue:intermediate", "regenerable", None, true), vec![CleanAction::Now, CleanAction::Bin]);
        assert_eq!(offered_actions("cache:npm", "regenerable", Some("npm"), true), vec![CleanAction::Now, CleanAction::Bin, CleanAction::Tool]);
        assert_eq!(offered_actions("ue:logs", "disposable", None, true), vec![CleanAction::Bin, CleanAction::Now]);
        assert_eq!(offered_actions("ue:global_ddc", "rebuild_cost", None, true), vec![CleanAction::Bin, CleanAction::Now]);
        assert_eq!(offered_actions("ue:logs", "disposable", None, false), vec![CleanAction::Now], "no bin where it can't be made");
        let a = actions_json("ue:logs", "disposable", None, true, Some(1.5));
        assert_eq!(a[0]["kind"], "bin");
        assert_eq!(a[0]["frees_now_gb"], 0.0);
        assert_eq!(a[0]["frees_when_bin_emptied_gb"], 1.5);
        let t = actions_json("cache:pip", "regenerable", Some("pip"), true, Some(2.0));
        assert_eq!(t[2]["undo"], "Can't be undone");
        assert_eq!(t[2]["command"], "pip cache purge");
    }

    #[test]
    fn picks_parse() {
        assert_eq!(parse_pick("d3").unwrap(), ("d3".into(), None));
        assert_eq!(parse_pick("d12:bin").unwrap(), ("d12".into(), Some(CleanAction::Bin)));
        assert_eq!(parse_pick("d1:NOW").unwrap().1, Some(CleanAction::Now));
        assert!(parse_pick("d1:zap").is_err());
        assert!(parse_pick(r"D:\s2").is_err());
        assert!(parse_pick("x1").is_err());
    }

    #[test]
    fn tool_commands_are_built_exactly() {
        assert_eq!(tool_command("npm").unwrap().display(), "npm cache clean --force");
        assert_eq!(tool_command("pip").unwrap().display(), "pip cache purge");
        assert_eq!(tool_command("dotnet").unwrap().display(), "dotnet nuget locals http-cache --clear");
        assert!(tool_command("cargo").is_none());
    }

    /// A fake npm on a PATH this test makes: the clean runs it with exactly the right
    /// arguments and nothing else is executed.
    #[test]
    fn tool_runs_through_a_fake_on_path() {
        let dir = std::env::temp_dir().join(format!("amg-fake-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("args.txt");
        std::fs::write(dir.join("npm.cmd"), format!("@echo off\r\necho %*> \"{}\"\r\nexit /b 0\r\n", out.display())).unwrap();
        std::fs::write(dir.join("pip.cmd"), "@echo off\r\nexit /b 3\r\n").unwrap();
        let pv = dir.display().to_string();
        assert_eq!(resolve_on_path("npm", &pv, ".COM;.EXE;.BAT;.CMD"), Some(dir.join("npm.cmd")));
        assert_eq!(resolve_on_path("dotnet", &pv, ".COM;.EXE;.BAT;.CMD"), None);
        let r = run_tool(&tool_command("npm").unwrap(), Some(&pv)).unwrap();
        assert_eq!(r, "Ran npm cache clean --force");
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "cache clean --force");
        assert!(run_tool(&tool_command("pip").unwrap(), Some(&pv)).unwrap_err().contains("exit code 3"));
        assert!(run_tool(&tool_command("dotnet").unwrap(), Some(&pv)).unwrap_err().contains("isn't installed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_links_not_their_targets_and_reports_what_remains() {
        let root = std::env::temp_dir().join(format!("amg-clean-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let item = root.join("Intermediate");
        let outside = root.join("Content");
        std::fs::create_dir_all(item.join(r"a\b")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.uasset"), b"precious").unwrap();
        std::fs::write(item.join(r"a\b\x.obj"), vec![1u8; 5000]).unwrap();
        std::fs::write(item.join("ro.txt"), b"ro").unwrap();
        let mut p = std::fs::metadata(item.join("ro.txt")).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(item.join("ro.txt"), p).unwrap();
        // A junction inside the item pointing at Content (mklink /J needs no admin).
        let j = item.join("link");
        let ok = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&j).arg(&outside).output().map(|o| o.status.success()).unwrap_or(false);
        let rep = delete_tree(&item);
        assert!(!rep.remains, "{:?}", rep);
        assert_eq!(rep.files, 2);
        assert_eq!(rep.bytes, 5002);
        if ok {
            assert_eq!(rep.links, 1);
        }
        assert_eq!(std::fs::read(outside.join("keep.uasset")).unwrap(), b"precious", "the junction's target is untouched");
        // A file held open without share-delete: the delete reports it and what remains.
        std::fs::create_dir_all(item.join("c")).unwrap();
        std::fs::write(item.join(r"c\held.bin"), b"held").unwrap();
        std::fs::write(item.join("gone.bin"), b"g").unwrap();
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).share_mode(1).open(item.join(r"c\held.bin")).unwrap()
        };
        let rep = delete_tree(&item);
        assert!(rep.remains);
        assert_eq!(rep.files, 1);
        assert_eq!(rep.failed_count, 1);
        assert!(rep.words().starts_with("Removed 1 file (less than 1 MB)") && rep.words().contains("1 could not be removed"), "{}", rep.words());
        drop(held);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_of_a_root_that_is_a_link_removes_only_the_link() {
        let root = std::env::temp_dir().join(format!("amg-clean-rootlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let target = root.join("Content");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.uasset"), b"precious").unwrap();
        let link = root.join("Intermediate");
        let ok = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&link).arg(&target).output().map(|o| o.status.success()).unwrap_or(false);
        if ok {
            let rep = delete_tree(&link);
            assert_eq!((rep.links, rep.files, rep.dirs), (1, 0, 0), "{:?}", rep);
            assert!(!rep.remains);
            assert!(!link.exists());
            assert_eq!(std::fs::read(target.join("keep.uasset")).unwrap(), b"precious", "the link's target is untouched");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bin_move_and_undo_round_trip() {
        let root = std::env::temp_dir().join(format!("amg-clean-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let logs = root.join(r"proj\Saved\Logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("a.log"), b"log").unwrap();
        let bin = root.join(BIN_DIR);
        let dst = move_to_bin(&logs, &bin, "run1", 1).unwrap();
        assert!(!logs.exists() && dst.join("a.log").is_file());
        let files = DiskFiles::in_dir(root.join("data"));
        let mut log = UndoLog::default();
        log.push_run(UndoRun {
            id: "run1".into(),
            time: "t".into(),
            unix: 1,
            entries: vec![UndoEntry {
                id: "run1-1".into(),
                finding_id: "d2".into(),
                title: "Logs".into(),
                action: CleanAction::Bin,
                step: UndoStep::FromBin { volume_root: root.display().to_string(), moves: vec![BinMove { from: logs.display().to_string(), to: dst.display().to_string() }], frees_alone: Some(3) },
                undone: false,
            }],
        });
        files.save_undo_log(&log).unwrap();
        assert!(files.undo_log().can_undo());
        let runs = bin_runs(&files, &[root.display().to_string()]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].titles, vec!["Logs".to_string()]);
        assert_eq!(runs[0].holds, 3);
        assert_eq!(expired(&runs, 24.0, 1 + 23 * 3600).len(), 0);
        assert_eq!(expired(&runs, 24.0, 1 + 24 * 3600).len(), 1);
        let outs = undo(&files, None);
        assert_eq!(outs[0].result, "undone", "{:?}", outs);
        assert!(logs.join("a.log").is_file());
        assert!(!files.undo_log().can_undo());
        // Undo when something is back at the old place: the moved copy stays.
        let dst = move_to_bin(&logs, &bin, "run2", 1).unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        let mut log = files.undo_log();
        log.push_run(UndoRun { id: "run2".into(), time: "t".into(), unix: 2, entries: vec![UndoEntry { id: "run2-1".into(), finding_id: "d2".into(), title: "Logs".into(), action: CleanAction::Bin,
            step: UndoStep::FromBin { volume_root: root.display().to_string(), moves: vec![BinMove { from: logs.display().to_string(), to: dst.display().to_string() }], frees_alone: None }, undone: false }] });
        files.save_undo_log(&log).unwrap();
        let outs = undo(&files, Some("run2-1"));
        assert_eq!(outs[0].result, "failed");
        assert!(outs[0].message.contains("stays in the bin"), "{}", outs[0].message);
        assert!(dst.join("a.log").is_file());
        // Emptying the bin: the entry can no longer be undone.
        let runs = bin_runs(&files, &[root.display().to_string()]);
        let emptied = empty_runs(&files, &runs);
        assert!(emptied.iter().all(|(_, r)| !r.remains));
        assert!(!dst.exists());
        assert!(!files.undo_log().can_undo());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_log_has_the_doctor_shape() {
        let mut log = UndoLog::default();
        log.push_run(UndoRun { id: "r".into(), time: "t".into(), unix: 0, entries: vec![UndoEntry { id: "r-1".into(), finding_id: "d1".into(), title: "x".into(), action: CleanAction::Now, step: UndoStep::Nothing, undone: false }] });
        let v: Value = serde_json::to_value(&log).unwrap();
        let e = &v["runs"][0]["entries"][0];
        for k in ["id", "finding_id", "title", "action", "step", "undone"] {
            assert!(e.get(k).is_some(), "{}", k);
        }
        assert_eq!(e["step"]["kind"], "nothing");
        assert!(!log.can_undo(), "a permanent delete can't be undone");
    }

    #[test]
    fn agent_plans_must_be_fresh_and_hold_the_ids() {
        let item = |id: &str| PlannedItem { id: id.into(), key: "ue:intermediate".into(), class: "regenerable".into(), title: "x".into(), paths: vec![], path_ids: vec![], last_write: 0, volume: None, volume_root: None, frees_alone: Some(1), confidence: "exact".into(), tool: None, unmeasurable: None };
        let plans = vec![PlanRecord { id: "p1".into(), unix: 1000, time: String::new(), items: vec![item("d1"), item("d2")], volumes: vec![] }];
        assert_eq!(plan_for_agent(&plans, "p1", &["d1".into()], 1000 + 599).unwrap().id, "p1");
        assert!(plan_for_agent(&plans, "p1", &["d1".into()], 1000 + 601).unwrap_err().contains("under 10 minutes"));
        assert!(plan_for_agent(&plans, "p1", &["d3".into()], 1000).is_err());
        assert!(plan_for_agent(&plans, "p9", &["d1".into()], 1000).unwrap_err().contains("no plan p9"));
        assert!(plan_for_agent(&plans, "p1", &["d1".into(), "d3".into()], 1000).unwrap_err().contains("d3 is not in plan"));
    }

    pub(crate) const REAL_UPROJECT: &[u8] = br#"{ "FileVersion": 3, "EngineAssociation": "5.4", "Category": "", "Modules": [] }"#;

    /// Set every file and folder under `root` to `secs` ago.
    pub(crate) fn age_tree(root: &Path, secs: u64) {
        use std::os::windows::fs::OpenOptionsExt;
        let when = std::time::SystemTime::now() - Duration::from_secs(secs);
        fn walk(p: &Path, when: std::time::SystemTime) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path(), when);
                }
            }
            if let Ok(f) = std::fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(p) {
                let _ = f.set_modified(when);
            }
        }
        walk(root, when);
    }

    /// A plan for one item of a tree a test made, measured now (file ids, newest write).
    pub(crate) fn plan_one(key: &str, title: &str, paths: &[PathBuf], frees: u64) -> PlanRecord {
        let ps: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        let (root, guid) = disk_win::volume_of(&ps[0]).unwrap();
        let (class, tool) = catalog_class(key).unwrap();
        let item = PlannedItem {
            id: "d1".into(),
            key: key.into(),
            class,
            title: title.into(),
            path_ids: ps.iter().map(|p| disk_win::identity(p).map(|x| x.1)).collect(),
            last_write: newest_write(&ps, Duration::from_secs(30)).unwrap(),
            paths: ps,
            volume: Some(guid.clone()),
            volume_root: Some(root.clone()),
            frees_alone: Some(frees),
            confidence: "exact".into(),
            tool,
            unmeasurable: None,
        };
        PlanRecord { id: "ptest".into(), unix: unix_now(), time: String::new(), items: vec![item], volumes: vec![PlannedVolume { volume: guid, root, fs: String::new(), item_ids: vec!["d1".into()], frees_together: Some(frees), confidence: "exact".into() }] }
    }

    pub(crate) fn test_opts() -> CleanOpts {
        CleanOpts { presample_secs: 1, start_watcher: false, busy: Some((vec![], false)), skip_git: true, ..Default::default() }
    }

    /// Found in review: a locked 2 MB file stayed, 1 KB was removed, and the watch still
    /// waited for the whole 2,007,040 bytes (and would have blamed the drive for them).
    #[test]
    fn a_partial_delete_predicts_only_what_went() {
        let root = std::env::temp_dir().join(format!("amg-clean-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let game = root.join("Game");
        let item = game.join("Intermediate");
        std::fs::create_dir_all(&item).unwrap();
        std::fs::write(game.join("Game.uproject"), REAL_UPROJECT).unwrap();
        std::fs::write(item.join("held.bin"), vec![7u8; 2_000_000]).unwrap();
        std::fs::write(item.join("small.bin"), vec![1u8; 1024]).unwrap();
        age_tree(&root, 3 * 3600);
        let plan = plan_one("ue:intermediate", "Build files", std::slice::from_ref(&item), 2_007_040);
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).share_mode(1).open(item.join("held.bin")).unwrap()
        };
        let files = DiskFiles::in_dir(root.join("data"));
        let r = clean(&crate::config::Config::builtin(), &files, &plan, &[("d1".into(), Some(CleanAction::Now))], &test_opts());
        drop(held);
        let o = &r.outcomes[0];
        assert_eq!(o.result, "partial", "{:?}", o);
        let want = scaled_prediction(Some(2_007_040), 1024, 2_000_000);
        assert!(want > 0 && want < 4096, "{}", want);
        assert_eq!(o.predicted_now, want);
        assert_eq!(r.volumes.len(), 1);
        assert_eq!(r.volumes[0].predicted_freed, want, "not the plan's 2,007,040");
        assert_eq!(r.volumes[0].confidence, "estimate");
        let w = files.watches();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].predicted, want);
        assert_eq!(w[0].predicted_confidence, "estimate");
        assert!(item.join("held.bin").is_file());
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(scaled_prediction(None, 5, 5), 0);
        assert_eq!(scaled_prediction(Some(100), 0, 0), 0);
        assert_eq!(scaled_prediction(Some(100), 50, 50), 50);
    }

    pub(crate) fn snapshot(id: &str, items: &[(&str, &str, u128)]) -> crate::disk_scan::CachedCheckup {
        crate::disk_scan::CachedCheckup {
            id: id.into(),
            scope: "*".into(),
            unix: 1,
            time: String::new(),
            items: items.iter().map(|(k, p, _)| crate::disk_scan::CatItem { key: k.to_string(), class: "regenerable".into(), title: k.to_string(), paths: vec![p.to_string()], lane: None }).collect(),
            root_ids: items.iter().map(|(_, _, f)| vec![*f]).collect(),
            last_writes: vec![],
            report: crate::disk_scan::Checkup { scan: json!({}), volumes: vec![], items: vec![], sharing_groups: vec![], totals: vec![], basis: "predicted".into(), do_not_sum: String::new() },
        }
    }

    /// Found in review: the window confirmed d1 on its snapshot, another checkup ran, and the
    /// clean deleted whatever d1 named then, without listing it.
    #[test]
    fn a_checkup_replaced_between_confirm_and_clean_refuses() {
        let a = snapshot("cA", &[("ue:intermediate", r"D:\s2\Intermediate", 11), ("ue:ddc", r"D:\s2\DerivedDataCache", 12)]);
        let b = snapshot("cB", &[("ue:crashes", r"D:\other\Saved\Crashes", 21), ("ue:intermediate", r"D:\s2\Intermediate", 11)]);
        let fp = checkup_item_fingerprint(&a, "d1").unwrap();
        let expects = vec![("d1".to_string(), fp.clone())];
        let ids = vec!["d1".to_string()];
        assert_eq!(check_identity(&a, "cA", &expects, &ids), Ok(()));
        assert_eq!(check_identity(&b, "cA", &expects, &ids).unwrap_err(), CHECKUP_CHANGED, "another checkup replaced it");
        assert_eq!(check_identity(&b, "cA", &[], &ids).unwrap_err(), CHECKUP_CHANGED);
        // Same checkup id, but d1 is something else (or its folder id changed): refused too.
        let mut a2 = a.clone();
        a2.items.swap(0, 1);
        a2.root_ids.swap(0, 1);
        assert_eq!(check_identity(&a2, "cA", &expects, &ids).unwrap_err(), CHECKUP_CHANGED);
        let mut a3 = a.clone();
        a3.root_ids[0] = vec![99];
        assert_eq!(check_identity(&a3, "cA", &expects, &ids).unwrap_err(), CHECKUP_CHANGED);
        // Every picked id needs its fingerprint once any is given.
        assert!(check_identity(&a, "cA", &expects, &["d1".into(), "d2".into()]).unwrap_err().contains("d2 has no --expect"));
        assert_eq!(parse_expect(&format!("d1={}", fp)).unwrap(), ("d1".into(), fp));
        assert!(parse_expect("d1").is_err() && parse_expect("d1=xyz").is_err() && parse_expect("D:=0123456789abcdef").is_err());
        // The fresh plan must find the folders the checkup counted.
        let item = |id: &str, p: &str, fid: u128| PlannedItem { id: id.into(), key: "ue:intermediate".into(), class: "regenerable".into(), title: "x".into(), paths: vec![p.into()], path_ids: vec![Some(fid)], last_write: 0, volume: None, volume_root: None, frees_alone: Some(1), confidence: "exact".into(), tool: None, unmeasurable: None };
        let plan = |it: PlannedItem| PlanRecord { id: "p".into(), unix: 0, time: String::new(), items: vec![it], volumes: vec![] };
        assert_eq!(check_live_ids(&a, &plan(item("d1", r"D:\s2\Intermediate", 11))), Ok(()));
        assert_eq!(check_live_ids(&a, &plan(item("d1", r"D:\s2\Intermediate", 77))).unwrap_err(), CHECKUP_CHANGED, "made again since the checkup");
        assert_eq!(check_live_ids(&a, &plan(item("d1", r"D:\s2\Content", 11))).unwrap_err(), CHECKUP_CHANGED);
        // Fingerprints ignore the case and slashes of paths, never the key or the ids.
        assert_eq!(item_fingerprint("k", &[r"D:\S2\x".into()], &[1]), item_fingerprint("k", &["d:/s2/x".into()], &[1]));
        assert_ne!(item_fingerprint("k", &[r"D:\s2\x".into()], &[1]), item_fingerprint("k", &[r"D:\s2\x".into()], &[2]));
        assert_ne!(item_fingerprint("k", &[r"D:\s2\x".into()], &[1]), item_fingerprint("j", &[r"D:\s2\x".into()], &[1]));
    }

    #[test]
    fn class_and_agent_limits_come_from_the_catalogue() {
        assert_eq!(catalog_class("ue:logs").unwrap().0, "disposable");
        assert_eq!(catalog_class("cache:npm").unwrap(), ("regenerable".to_string(), Some("npm".to_string())));
        assert_eq!(catalog_class("cache:old").unwrap().0, "disposable");
        assert_eq!(catalog_class("lane").unwrap().0, "review");
        assert_eq!(catalog_class("made:up"), None);
        for k in ["ue:intermediate", "ue:plugin_intermediate", "ue:ddc", "ue:crashes", "ue:shader_debug", "cache:npm", "cache:pip", "gpu:d3dscache"] {
            assert!(agent_may_clean(k), "{}", k);
        }
        for k in ["ue:logs", "ue:cooked", "ue:staged", "ue:global_ddc", "ue:zen", "temp:old", "cache:old", "lane", "memguard:bin", "git:lfs", "made:up"] {
            assert!(!agent_may_clean(k), "{}", k);
        }
    }

    const G: u64 = 1 << 30;

    #[test]
    fn measured_on_a_quiet_ntfs_drive() {
        let before = 300 * G;
        let mut m = Measure::new(before, 16 * G, 0);
        assert!(!m.push(1, before + 16 * G));
        assert!(!m.push(2, before + 16 * G));
        assert!(m.push(3, before + 16 * G + (1 << 20)));
        let r = m.result();
        assert!(r.settled && !r.other_activity);
        assert_eq!(r.appeared_after_sec, Some(1));
        let (t, b) = measured_words("C:", 16 * G, &r, false);
        assert_eq!(t, "Free space rose on C:");
        assert_eq!(b, "Free space on C: rose by 16.0 GB in the minute after the cleanup (measured; other activity on the drive can add to or hide this). Predicted 16.0 GB.");
    }

    #[test]
    fn measured_waits_for_late_refs_frees_and_never_claims_early() {
        let before = 369 * G;
        let mut m = Measure::new(before, 16 * G, 0);
        // Nothing comes back for 20 minutes: stable, but not done.
        for s in 1..1200 {
            assert!(!m.push(s, before), "must not stop at {} s with nothing back", s);
        }
        assert_eq!(m.result().appeared_after_sec, None);
        // Then it all comes back at once.
        m.push(1200, before + 16 * G);
        m.push(1205, before + 16 * G);
        assert!(m.push(1210, before + 16 * G));
        let r = m.result();
        assert_eq!(r.appeared_after_sec, Some(1200));
        assert!(r.settled);
        // Never back within 30 minutes: stops, says so, and points at the drive's delay.
        let mut m = Measure::new(before, 16 * G, 0);
        let mut s = 0;
        while !m.push(s, before + (200 << 20)) {
            s += 5;
        }
        assert_eq!(s, WATCH_MAX_SEC);
        let r = m.result();
        assert!(!r.settled && r.appeared_after_sec.is_none());
        let (t, b) = measured_words("D:", 16 * G, &r, true);
        assert_eq!(t, "Space hasn't come back yet on D:");
        assert!(b.starts_with("Free space on D: rose by 0.2 GB in the 30 minutes after the cleanup (measured; other activity on the drive can add to or hide this). Predicted 16.0 GB."), "{}", b);
        assert!(b.contains("much later"));
        assert!(m.mismatch());
    }

    #[test]
    fn measured_notes_other_writes() {
        let before = 100 * G;
        // Free space was moving before the delete.
        let mut m = Measure::new(before, 4 * G, 300 << 20);
        m.push(1, before + 4 * G);
        m.push(2, before + 4 * G);
        m.push(3, before + 4 * G);
        let r = m.result();
        assert!(r.other_activity);
        let (_, b) = measured_words("D:", 4 * G, &r, true);
        assert!(b.starts_with("Free space on D: rose by 4.0 GB in the minute after the cleanup (measured;") && b.contains("isn't exact"), "{}", b);
        // A build wrote 2 GB while we watched: the space went down after coming back.
        let mut m = Measure::new(before, 4 * G, 0);
        m.push(1, before + 4 * G);
        m.push(2, before + 2 * G);
        m.push(3, before + 2 * G);
        m.push(4, before + 2 * G);
        assert!(m.result().other_activity);
        // A bin move predicts 0 and settles at about 0.
        let mut m = Measure::new(before, 0, 0);
        let mut s = 0;
        while !m.push(s, before) {
            s += 1;
        }
        assert!(s >= 30 && m.result().settled && m.result().measured_freed == 0);
        // Seen on the team's D:: 2.5 GB predicted, then 105 GB came back at once after 320 s.
        let mut m = Measure::new(before, 2 * G + G / 2, 0);
        for s in 1..320 {
            m.push(s, before - (300 << 20));
        }
        m.push(320, before + 105 * G);
        m.push(325, before + 105 * G);
        m.push(330, before + 105 * G);
        let r = m.result();
        assert!(r.other_activity);
        let (t, b) = measured_words("D:", 2 * G + G / 2, &r, true);
        assert_eq!(t, "Free space jumped on D:");
        assert!(b.contains("rose by 105.0 GB in the 6 minutes after the cleanup (measured)") && b.contains("can't be told apart"), "{}", b);
    }

    /// The measured words state the observation only: a late bulk ReFS return of earlier deletes
    /// rises near the prediction too, so "Freed X GB" would claim a cause nobody measured.
    #[test]
    fn measured_words_state_only_what_was_observed() {
        let m = |d: i64, watched: u64, appeared: bool| Measured { measured_freed: d, watched_sec: watched, appeared_after_sec: appeared.then_some(watched), settled: appeared, ..Default::default() };
        for (meas, pred) in [(m(4 * G as i64, 200, true), 4 * G), (m(G as i64 / 5, WATCH_MAX_SEC, false), 4 * G)] {
            let (t, b) = measured_words("D:", pred, &meas, true);
            assert!(!t.contains("Freed") && !b.contains("Freed") && !b.contains("freed"), "{} / {}", t, b);
            assert!(b.contains("(measured; other activity on the drive can add to or hide this)"), "{}", b);
        }
        let (_, b) = measured_words("D:", 4 * G, &m(4 * G as i64, 200, true), false);
        assert!(b.starts_with("Free space on D: rose by 4.0 GB in the 4 minutes after the cleanup"), "{}", b);
        let (_, b) = measured_words("D:", 4 * G, &m(-(G as i64), WATCH_MAX_SEC, false), false);
        assert!(b.starts_with("Free space on D: didn't rise in the 30 minutes after the cleanup"), "{}", b);
        // A large overshoot still says it can't be told apart.
        let (t, b) = measured_words("D:", G, &m(50 * G as i64, 320, true), true);
        assert_eq!(t, "Free space jumped on D:");
        assert!(b.contains("can't be told apart") && b.contains("in the 6 minutes after the cleanup"), "{}", b);
        // A failed read is "couldn't measure", never 0.
        let u = Measured { unreadable: true, ..Default::default() };
        let (t, b) = measured_words("D:", 4 * G, &u, true);
        assert_eq!(t, "Couldn't measure the free space on D:");
        assert!(!b.contains("0.0 GB") && b.contains("nothing was measured"), "{}", b);
        let mut w = Measure::new(100 * G, 4 * G, 0);
        assert!(!w.skip(5));
        assert!(w.skip(WATCH_MAX_SEC));
        assert!(w.result().unreadable, "no reading at all");
        w.push(WATCH_MAX_SEC, 100 * G);
        assert!(!w.result().unreadable);
        // The JSON field says rise, not freed, and an old record still reads.
        let j = serde_json::to_value(Measured { measured_freed: 5, ..Default::default() }).unwrap();
        assert_eq!(j["free_space_rise"], 5);
        assert!(j.get("measured_freed").is_none());
        let old: Measured = serde_json::from_str(r#"{"measured_freed":7,"settled":true,"other_activity":false,"appeared_after_sec":null,"watched_sec":3}"#).unwrap();
        assert_eq!(old.measured_freed, 7);
        // recent_cleanups: couldnt_measure, never a number.
        let dir = std::env::temp_dir().join(format!("amg-clean-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = DiskFiles::in_dir(&dir);
        let rec = WatchRecord { id: "w".into(), run_id: "r".into(), volume: String::new(), volume_root: "D:\\".into(), fs: "ReFS".into(), started_unix: unix_now(), time: String::new(),
            free_before: 0, pre_motion: 0, predicted: G, predicted_confidence: "exact".into(), titles: vec![], watcher_pid: 0, measured: unmeasurable_watch(&None), toasted: false };
        files.save_watches(&[rec]).unwrap();
        let r = recent_cleanups(&files, 5);
        assert_eq!(r[0]["state"], "couldnt_measure");
        assert!(r[0]["measured_free_space_rise_gb"].is_null() && r[0].get("measured_freed_gb").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(6301), "6,301");
        assert_eq!(thousands(1_669_644), "1,669,644");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(6301, "file"), "6,301 files");
        assert_eq!(size_words(30 << 20), "30 MB");
        assert_eq!(size_words(16 << 30), "16.0 GB");
        assert_eq!(size_words(0), "0.0 GB");
        assert_eq!(gb_words(Some(30 << 20), "lower_bound"), "at least 30 MB");
    }
}

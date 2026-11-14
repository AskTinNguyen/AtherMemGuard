//! The settings window's content as data: sections, rows, group cards, with the current
//! values read from a `SettingsStore` and the latest `Status`. No drawing here, so search,
//! value formatting and the row table are unit-tested.

use super::kit::{format_gb, pct_to_gb, ButtonStyle};
use super::settings_store::{group_path, GroupInfo, Layer, Setting, SettingsStore, Tier, Value};
use crate::engine::{GroupStatus, Status};

pub const PRESETS: [(&str, &str); 3] = [("gentle", "Gentle"), ("balanced", "Balanced"), ("aggressive", "Aggressive")];
pub const PRIORITIES: [(&str, &str); 5] =
    [("normal", "Normal"), ("belownormal", "Slightly low"), ("medium", "Medium"), ("low", "Low"), ("verylow", "Very low")];
pub const SPEEDS: [(&str, &str); 4] = [("normal", "Normal"), ("below", "Slower"), ("idle", "Slowest"), ("eco", "Efficiency mode")];

pub const COMMIT_HINT: &str = "Memory Windows has promised to apps. When this runs out, apps fail even if RAM is free.";
pub const HEADROOM_HINT: &str = "What memguard wait and agents wait for before starting this kind of work.";
pub const GUIDE_URL: &str = "https://github.com/AskTinNguyen/AtherMemGuard/blob/main/docs/AI-AGENTS.md";

/// The label for a stored enum value, from one of the tables above.
pub fn label_of(table: &[(&str, &'static str)], value: &str) -> &'static str {
    table.iter().find(|(v, _)| v.eq_ignore_ascii_case(value)).map(|(_, l)| *l).unwrap_or(table[0].1)
}

/// "48", "6.5": numbers without a pointless ".0".
pub fn format_number(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        let s = format!("{:.2}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Every word of `query` must appear in one of `haystack` (case-insensitive).
pub fn matches(query: &str, haystack: &[&str]) -> bool {
    let hay = haystack.join(" \u{1} ").to_lowercase();
    query.split_whitespace().all(|w| hay.contains(&w.to_lowercase()))
}

/// What the update row shows (the tray loop owns the updater).
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    Unknown,
    Checking,
    /// `checked` reads well after "Checked": "today at 14:02".
    UpToDate { checked: String },
    Available { version: String },
    Downloading { pct: u32 },
    Failed(String),
}

impl UpdateState {
    pub fn hint(&self) -> String {
        match self {
            UpdateState::Unknown => "Not checked yet.".into(),
            UpdateState::Checking => "Checking…".into(),
            UpdateState::UpToDate { checked } => format!("Up to date. Checked {}.", checked),
            UpdateState::Available { version } => format!("Version {} is ready to install. Your settings stay.", version),
            UpdateState::Downloading { pct } => format!("Downloading… {}%", pct),
            UpdateState::Failed(reason) => format!("Couldn't check: {}", reason),
        }
    }
}

/// Buttons in rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonAct {
    AddGroup,
    OpenHistory,
    CheckNow,
    UpdateNow,
    CopySetup,
    OpenGuide,
    LoadTeam,
    RemoveTeam,
    Export,
    Import,
    OpenFile,
    ResetAll,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ctl {
    Segmented { path: String, options: Vec<(&'static str, &'static str)>, selected: usize },
    Toggle { path: String, on: bool },
    Startup { on: bool },
    Hotkey { path: String, keys: String },
    Number { path: String, value: f64, unit: String, min: f64, max: f64, integer: bool, off_at_zero: bool },
    Chips { path: String, items: Vec<String>, add_label: &'static str },
    /// A group's apps: chips plus "+ Add app…".
    GroupApps { group: String, items: Vec<String> },
    Buttons(Vec<(String, ButtonAct, ButtonStyle)>),
    Headroom { ram_path: String, ram: f64, commit_path: String, commit: f64 },
    /// No control (the Agents group's explanation).
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub section: &'static str,
    pub label: String,
    pub hint: String,
    pub ctl: Ctl,
    /// Red label: can freeze or crash apps.
    pub danger: bool,
    /// Paths a right-click resets.
    pub resets: Vec<String>,
    /// Highest layer among `resets` (User shows the dot, Team the tag).
    pub layer: Layer,
    /// What reset falls back to ("Balanced") and whether that is a team value.
    pub fallback: Option<(String, Layer)>,
    /// The current value as words, for search and menus.
    pub value_text: String,
    /// Wider control column (chips, three buttons).
    pub wide: bool,
}

impl Row {
    pub fn haystack(&self) -> Vec<&str> {
        vec![&self.label, &self.hint, self.section, &self.value_text]
    }
}

/// One tier of a group card.
#[derive(Debug, Clone, PartialEq)]
pub struct TierModel {
    pub tier: Tier,
    /// Set at this tier (any layer); `None` inherits.
    pub own_pct: Option<f64>,
    pub pct: f64,
    pub own_cpu: Option<String>,
    pub cpu: String,
    pub pct_layer: Option<Layer>,
}

#[derive(Debug, Clone)]
pub struct GroupModel {
    pub info: GroupInfo,
    pub status: Option<GroupStatus>,
    pub priority: String,
    pub priority_layer: Layer,
    pub tiers: Vec<TierModel>,
    /// Apps, Freeze, Hard cap (or the Agents explanation).
    pub rows: Vec<Row>,
}

impl GroupModel {
    pub fn header_haystack(&self) -> Vec<String> {
        vec![self.info.name.clone(), "App groups".into(), label_of(&PRIORITIES, &self.priority).into()]
    }
    pub fn tiers_haystack(&self) -> String {
        let mut s = String::from("When Always Unreal not running Unreal is running Memory is tight Memory is critical Soft cap share of RAM Limit Speed");
        for t in &self.tiers {
            s.push(' ');
            s.push_str(label_of(&SPEEDS, &t.cpu));
        }
        s
    }
}

#[derive(Debug, Clone)]
pub enum Block {
    Section(&'static str),
    Row(Row),
    Group(GroupModel),
}

pub struct ModelInput<'a> {
    pub store: &'a dyn SettingsStore,
    pub status: Option<&'a Status>,
    pub app_version: &'a str,
    pub update: &'a UpdateState,
}

fn value_words(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "On" } else { "Off" }.into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format_number(*f),
        Value::Text(s) => s.clone(),
        Value::List(l) => format!("{} apps", l.len()),
    }
}

struct Builder<'a> {
    input: &'a ModelInput<'a>,
    blocks: Vec<Block>,
    section: &'static str,
}

impl<'a> Builder<'a> {
    fn get(&self, path: &str) -> Option<Setting> {
        self.input.store.get(path)
    }
    fn section(&mut self, name: &'static str) {
        self.section = name;
        self.blocks.push(Block::Section(name));
    }
    fn row(&self, label: &str, hint: &str, ctl: Ctl, resets: &[&str], format: impl Fn(&Value) -> String) -> Row {
        let mut layer = Layer::Builtin;
        let mut fallback_parts = Vec::new();
        let mut fallback_layer = Layer::Builtin;
        let mut has_fallback = true;
        let mut value_parts = Vec::new();
        for p in resets {
            match self.get(p) {
                Some(s) => {
                    if s.layer == Layer::User || (s.layer == Layer::Team && layer == Layer::Builtin) {
                        layer = s.layer;
                    }
                    value_parts.push(format(&s.value));
                    match &s.fallback {
                        Some((v, l)) => {
                            fallback_parts.push(format(v));
                            if *l == Layer::Team {
                                fallback_layer = Layer::Team;
                            }
                        }
                        None => has_fallback = false,
                    }
                }
                None => has_fallback = false,
            }
        }
        Row {
            section: self.section,
            label: label.to_string(),
            hint: hint.to_string(),
            ctl,
            danger: false,
            resets: resets.iter().map(|s| s.to_string()).collect(),
            layer,
            fallback: if has_fallback && !fallback_parts.is_empty() { Some((fallback_parts.join(" / "), fallback_layer)) } else { None },
            value_text: value_parts.join(" "),
            wide: false,
        }
    }
    fn push(&mut self, r: Row) {
        self.blocks.push(Block::Row(r));
    }
    fn bool_of(&self, path: &str) -> bool {
        self.get(path).and_then(|s| s.value.as_bool()).unwrap_or(false)
    }
    fn num_of(&self, path: &str) -> f64 {
        self.get(path).and_then(|s| s.value.as_f64()).unwrap_or(0.0)
    }
    fn is_int(&self, path: &str) -> bool {
        matches!(self.get(path).map(|s| s.value), Some(Value::Int(_)))
    }
    fn toggle(&mut self, label: &str, hint: &str, path: &str) -> Row {
        let on = self.bool_of(path);
        self.row(label, hint, Ctl::Toggle { path: path.into(), on }, &[path], value_words)
    }
    fn number(&mut self, label: &str, hint: &str, path: &str, unit: &str, min: f64, max: f64) -> Row {
        let value = self.num_of(path);
        let integer = self.is_int(path);
        let u = unit.to_string();
        let ctl = Ctl::Number { path: path.into(), value, unit: unit.into(), min, max, integer, off_at_zero: false };
        self.row(label, hint, ctl, &[path], move |v| format!("{} {}", value_words(v), u))
    }
}

/// Build the whole page.
pub fn build(input: &ModelInput) -> Vec<Block> {
    let mut b = Builder { input, blocks: Vec::new(), section: "" };
    let ram_total = input.status.map(|s| s.system.ram_total_gb).unwrap_or(0.0);
    let commit_total = input.status.map(|s| s.system.commit_limit_gb).unwrap_or(0.0);

    // ---------------------------------------------------------------- GENERAL
    b.section("General");
    let preset = b.get("general.preset").and_then(|s| s.value.as_str().map(str::to_string)).unwrap_or_else(|| "balanced".into());
    let selected = PRESETS.iter().position(|(v, _)| v.eq_ignore_ascii_case(&preset)).unwrap_or(1);
    let r = b.row(
        "How hard to push other apps",
        "Balanced keeps browsers and chat usable. Aggressive halves every limit and slows other apps while Unreal runs.",
        Ctl::Segmented { path: "general.preset".into(), options: PRESETS.to_vec(), selected },
        &["general.preset"],
        |v| label_of(&PRESETS, v.as_str().unwrap_or("")).to_string(),
    );
    b.push(r);
    let r = b.toggle(
        "Free memory when a build or cook starts",
        "Holds back every app group the moment UnrealBuildTool or a cook begins.",
        "general.auto_prime",
    );
    b.push(r);
    let startup = input.store.run_at_startup();
    let mut r = b.row("Run at Windows startup", "Starts in the tray when you sign in.", Ctl::Startup { on: startup }, &[], value_words);
    r.value_text = if startup { "On" } else { "Off" }.into();
    b.push(r);
    let r = b.toggle("Notifications", "Memory warnings, updates and hints. Nothing else.", "general.notifications");
    b.push(r);
    let keys = b.get("general.prime_hotkey").and_then(|s| s.value.as_str().map(str::to_string)).unwrap_or_default();
    let r = b.row(
        "Free memory now",
        "Shortcut that works in any app.",
        Ctl::Hotkey { path: "general.prime_hotkey".into(), keys },
        &["general.prime_hotkey"],
        |v| match v.as_str() {
            Some("") | None => "Not set".into(),
            Some(k) => k.to_string(),
        },
    );
    b.push(r);

    // ---------------------------------------------------------------- APP GROUPS
    b.section("App groups");
    for info in input.store.groups() {
        let g = group_model(&b, info, input.status, ram_total);
        b.blocks.push(Block::Group(g));
    }
    let r = b.row(
        "Add a group",
        "Pick apps from what's running now.",
        Ctl::Buttons(vec![("Add group…".into(), ButtonAct::AddGroup, ButtonStyle::Normal)]),
        &[],
        value_words,
    );
    b.push(r);

    // ---------------------------------------------------------------- UNREAL
    b.section("Unreal");
    let protect: Vec<String> = b.get("ue.protect").and_then(|s| s.value.as_list().map(|l| l.to_vec())).unwrap_or_default();
    let mut r = b.row(
        "Never touched",
        "Unreal's own tools. Listed apps are skipped even if a group names them.",
        Ctl::Chips { path: "ue.protect".into(), items: protect.clone(), add_label: "+ Add…" },
        &["ue.protect"],
        value_words,
    );
    r.value_text = protect.join(" ");
    r.wide = true;
    b.push(r);
    let r = b.number(
        "Suggest restarting the editor above",
        "One notification when an editor uses more than this.",
        "ue.editor_restart_hint_gb",
        "GB",
        4.0,
        1024.0,
    );
    b.push(r);
    let r = b.number(
        "Warn when the editor grows faster than",
        "Measured over the last 30 minutes. 0 turns the warning off.",
        "ue.editor_growth_alert_gb_per_hour",
        "GB / hour",
        0.0,
        256.0,
    );
    b.push(r);
    let mut r = b.toggle(
        "Pause compile workers when memory is critical",
        "The newest workers wait and resume when memory recovers. The build is slower instead of failing.",
        "ue.worker_throttle",
    );
    r.danger = true;
    let throttle = matches!(r.ctl, Ctl::Toggle { on: true, .. });
    b.push(r);
    if throttle {
        let r = b.number("Keep at least this many workers running", "Never pauses below this many.", "ue.min_workers", "workers", 1.0, 256.0);
        b.push(r);
    }

    // ---------------------------------------------------------------- MEMORY PRESSURE
    b.section("Memory pressure");
    let pressure_rows = [
        ("Memory is tight when RAM free drops below", "Background apps are held back harder from here.", "pressure.elevated_ram_free_pct", ram_total),
        ("…or commit free drops below", COMMIT_HINT, "pressure.elevated_commit_free_pct", commit_total),
        ("Memory is critical when RAM free drops below", "Background apps get the strictest limits.", "pressure.critical_ram_free_pct", ram_total),
        ("…or commit free drops below", COMMIT_HINT, "pressure.critical_commit_free_pct", commit_total),
    ];
    for (label, hint, path, total) in pressure_rows {
        let value = b.num_of(path);
        let unit = if total > 0.0 { format!("% · {}", format_gb(pct_to_gb(value, total))) } else { "%".into() };
        let ctl = Ctl::Number { path: path.into(), value, unit: unit.clone(), min: 0.5, max: 50.0, integer: false, off_at_zero: false };
        let r = b.row(label, hint, ctl, &[path], move |v| format!("{}%", value_words(v)));
        b.push(r);
    }

    // ---------------------------------------------------------------- HEADROOM
    b.section("Headroom");
    for (label, w) in [("Editor", "editor"), ("Build", "build"), ("Cook", "cook")] {
        let (rp, cp) = (format!("prime.{}.ram_gb", w), format!("prime.{}.commit_gb", w));
        let ctl = Ctl::Headroom { ram_path: rp.clone(), ram: b.num_of(&rp), commit_path: cp.clone(), commit: b.num_of(&cp) };
        let r = b.row(label, HEADROOM_HINT, ctl, &[&rp, &cp], |v| format!("{} GB", value_words(v)));
        b.push(r);
    }

    // ---------------------------------------------------------------- HISTORY
    b.section("History");
    let r = b.toggle("Record memory history", "One line every 10 seconds, kept on this PC.", "history.enabled");
    b.push(r);
    let r = b.number("Keep for", "Older days are deleted.", "history.keep_days", "days", 1.0, 365.0);
    b.push(r);
    let r = b.row(
        "Memory history",
        "A chart of RAM free, commit free and Unreal over time.",
        Ctl::Buttons(vec![("Open memory history".into(), ButtonAct::OpenHistory, ButtonStyle::Normal)]),
        &[],
        value_words,
    );
    b.push(r);

    // ---------------------------------------------------------------- UPDATES
    b.section("Updates");
    let r = b.toggle("Check for updates automatically", "Once a day. Only the version check is sent.", "updates.check_automatically");
    b.push(r);
    let button = match input.update {
        UpdateState::Available { .. } => ("Update now".to_string(), ButtonAct::UpdateNow, ButtonStyle::Primary),
        _ => ("Check now".to_string(), ButtonAct::CheckNow, ButtonStyle::Normal),
    };
    let r = b.row(&format!("Version {}", input.app_version), &input.update.hint(), Ctl::Buttons(vec![button]), &[], value_words);
    b.push(r);

    // ---------------------------------------------------------------- AI AGENTS
    b.section("AI agents");
    let r = b.row(
        "Let agents check and free memory",
        "Agents use the memguard command or its MCP server. Paste this into your agent's setup.",
        Ctl::Buttons(vec![("Copy setup".into(), ButtonAct::CopySetup, ButtonStyle::Normal)]),
        &[],
        value_words,
    );
    b.push(r);
    let r = b.row(
        "Guide for AI agents",
        "Tools, examples and when an agent should wait for memory.",
        Ctl::Buttons(vec![("Open guide".into(), ButtonAct::OpenGuide, ButtonStyle::Normal)]),
        &[],
        value_words,
    );
    b.push(r);

    // ---------------------------------------------------------------- TEAM AND BACKUP
    b.section("Team and backup");
    let team = input.store.team_info();
    let status = match &team {
        Some(t) => format!("Loaded {}, {} settings.", t.loaded, t.settings),
        None => "None loaded.".into(),
    };
    let mut buttons = vec![("Load team file…".to_string(), ButtonAct::LoadTeam, ButtonStyle::Normal)];
    if team.is_some() {
        buttons.push(("Remove".into(), ButtonAct::RemoveTeam, ButtonStyle::Normal));
    }
    let r = b.row(
        "Team defaults",
        &format!("{} A shared file that sets defaults for everyone. Your own changes still win.", status),
        Ctl::Buttons(buttons),
        &[],
        value_words,
    );
    b.push(r);
    let mut r = b.row(
        "Your settings",
        &format!("settings.toml in {}", short_folder(&input.store.settings_file())),
        Ctl::Buttons(vec![
            ("Export…".into(), ButtonAct::Export, ButtonStyle::Normal),
            ("Import…".into(), ButtonAct::Import, ButtonStyle::Normal),
            ("Open file".into(), ButtonAct::OpenFile, ButtonStyle::Normal),
        ]),
        &[],
        value_words,
    );
    r.wide = true;
    b.push(r);
    let r = b.row(
        "Start over",
        "Puts every setting back to its default. A backup of your settings is kept.",
        Ctl::Buttons(vec![("Reset all settings".into(), ButtonAct::ResetAll, ButtonStyle::Normal)]),
        &[],
        value_words,
    );
    b.push(r);
    b.blocks
}

/// "%APPDATA%\AtherMemGuard" for the usual folder, else the folder itself.
fn short_folder(file: &std::path::Path) -> String {
    let dir = file.parent().map(|d| d.display().to_string()).unwrap_or_default();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let a = appdata.to_string_lossy().to_string();
        if !a.is_empty() && dir.to_lowercase().starts_with(&a.to_lowercase()) {
            return format!("%APPDATA%{}", &dir[a.len()..]);
        }
    }
    let lower = dir.to_lowercase();
    if let Some(i) = lower.find("\\appdata\\roaming") {
        return format!("%APPDATA%{}", &dir[i + "\\appdata\\roaming".len()..]);
    }
    dir
}

fn group_model(b: &Builder, info: GroupInfo, status: Option<&Status>, ram_total: f64) -> GroupModel {
    let name = info.name.clone();
    let st = status.and_then(|s| s.groups.iter().find(|g| g.name == name).cloned());
    let pr = b.get(&group_path(&name, Tier::Always, "priority"));
    let priority = pr.as_ref().and_then(|s| s.value.as_str().map(str::to_string)).unwrap_or_else(|| "normal".into());
    let priority_layer = pr.map(|s| s.layer).unwrap_or(Layer::Builtin);

    let mut tiers: Vec<TierModel> = Vec::new();
    for tier in Tier::ALL {
        let ps = b.get(&group_path(&name, tier, "trim_above_pct"));
        let cs = b.get(&group_path(&name, tier, "cpu"));
        let above = tiers.last();
        let own_pct = ps.as_ref().and_then(|s| s.value.as_f64());
        let own_cpu = cs.as_ref().and_then(|s| s.value.as_str().map(str::to_string));
        tiers.push(TierModel {
            tier,
            pct: own_pct.or(above.map(|a| a.pct)).unwrap_or(0.0),
            cpu: own_cpu.clone().or(above.map(|a| a.cpu.clone())).unwrap_or_else(|| "normal".into()),
            own_pct,
            own_cpu,
            pct_layer: ps.map(|s| s.layer),
        });
    }

    let mut rows = Vec::new();
    let mut apps = b.row(
        "Apps",
        "Matched by process name.",
        Ctl::GroupApps { group: name.clone(), items: info.processes.clone() },
        &[],
        value_words,
    );
    apps.section = "App groups";
    apps.value_text = format!("{} {}", name, info.processes.join(" "));
    apps.wide = true;
    rows.push(apps);
    if info.never_freeze {
        let mut r = b.row(
            "Freeze and hard cap",
            "These run your agents, so they are only ever slowed down and held back, never frozen or capped.",
            Ctl::None,
            &[],
            value_words,
        );
        r.value_text = name.clone();
        rows.push(r);
    } else {
        let fpath = group_path(&name, Tier::Critical, "suspend");
        let on = b.bool_of(&fpath);
        let mut r = b.row(
            "Freeze when memory is critical",
            "The apps stop responding until memory recovers.",
            Ctl::Toggle { path: fpath.clone(), on },
            &[&fpath],
            value_words,
        );
        r.danger = true;
        r.value_text = format!("{} {}", name, r.value_text);
        rows.push(r);
        let hpath = group_path(&name, Tier::Always, "hard_cap_mb");
        let value = b.num_of(&hpath);
        let ctl = Ctl::Number { path: hpath.clone(), value, unit: "MB".into(), min: 0.0, max: 1_048_576.0, integer: true, off_at_zero: true };
        let mut r = b.row(
            "Hard cap",
            "Apps that hit a hard cap usually crash. Browser helper processes can't be capped.",
            ctl,
            &[&hpath],
            |v| match v.as_f64() {
                Some(x) if x > 0.0 => format!("{} MB", format_number(x)),
                _ => "Off".into(),
            },
        );
        if b.get(&hpath).is_none() {
            r.value_text = "Off".into();
        }
        r.danger = true;
        r.value_text = format!("{} {}", name, r.value_text);
        rows.push(r);
    }
    let _ = ram_total;
    GroupModel { info, status: st, priority, priority_layer, tiers, rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::settings_store::FakeStore;
    use crate::ui::status_source::sample_status;

    fn page(store: &FakeStore) -> Vec<Block> {
        let st = sample_status();
        let up = UpdateState::UpToDate { checked: "today at 14:02".into() };
        build(&ModelInput { store, status: Some(&st), app_version: "0.2.0", update: &up })
    }

    fn rows(blocks: &[Block]) -> Vec<&Row> {
        blocks.iter().filter_map(|b| if let Block::Row(r) = b { Some(r) } else { None }).collect()
    }

    #[test]
    fn search_matching() {
        assert!(matches("", &["anything"]));
        assert!(matches("hard CAP", &["Hard cap", "Apps that hit"]));
        assert!(matches("cap crash", &["Hard cap", "Apps that hit a hard cap usually crash."]));
        assert!(!matches("cap freeze", &["Hard cap", "Apps that hit a hard cap usually crash."]));
        assert!(matches("balanced", &["How hard to push other apps", "", "General", "Balanced"]));
        assert!(matches("  notif  ", &["Notifications"]));
        // Words may match different fields.
        assert!(matches("general startup", &["Run at Windows startup", "Starts in the tray", "General", "On"]));
    }

    #[test]
    fn row_table_is_complete() {
        let store = FakeStore::mockup();
        let blocks = page(&store);
        let sections: Vec<&str> = blocks.iter().filter_map(|b| if let Block::Section(s) = b { Some(*s) } else { None }).collect();
        assert_eq!(
            sections,
            ["General", "App groups", "Unreal", "Memory pressure", "Headroom", "History", "Updates", "AI agents", "Team and backup"]
        );
        let labels: Vec<&str> = rows(&blocks).iter().map(|r| r.label.as_str()).collect();
        for l in [
            "How hard to push other apps",
            "Free memory when a build or cook starts",
            "Run at Windows startup",
            "Notifications",
            "Free memory now",
            "Add a group",
            "Never touched",
            "Suggest restarting the editor above",
            "Warn when the editor grows faster than",
            "Pause compile workers when memory is critical",
            "Memory is tight when RAM free drops below",
            "Memory is critical when RAM free drops below",
            "Editor",
            "Build",
            "Cook",
            "Record memory history",
            "Keep for",
            "Check for updates automatically",
            "Version 0.2.0",
            "Let agents check and free memory",
            "Team defaults",
            "Your settings",
            "Start over",
        ] {
            assert!(labels.contains(&l), "missing row {l}");
        }
        // Workers minimum only while the toggle is on.
        assert!(!labels.contains(&"Keep at least this many workers running"));
        let groups: Vec<&GroupModel> = blocks.iter().filter_map(|b| if let Block::Group(g) = b { Some(g) } else { None }).collect();
        assert_eq!(groups.len(), 4);
        let agents = groups.iter().find(|g| g.info.name == "Agents").unwrap();
        assert!(agents.rows.iter().all(|r| r.label != "Hard cap"));
        let browsers = groups[0];
        assert_eq!(browsers.tiers[0].pct, 4.8);
        assert_eq!(browsers.tiers[3].own_cpu, None);
        assert_eq!(browsers.tiers[3].cpu, "below", "critical inherits Slower from tight");
        assert_eq!(agents.tiers[1].pct, 0.0, "nothing set above: Off");
    }

    #[test]
    fn layers_dots_and_fallbacks() {
        let mut store = FakeStore::mockup();
        let blocks = page(&store);
        let r = rows(&blocks).into_iter().find(|r| r.label == "Free memory now").unwrap();
        assert_eq!(r.layer, Layer::User);
        let k = rows(&blocks).into_iter().find(|r| r.label == "Keep for").unwrap();
        assert_eq!(k.layer, Layer::Team);
        assert_eq!(k.fallback, Some(("7 days".into(), Layer::Builtin)));
        store.set("history.keep_days", Value::Int(30)).unwrap();
        let blocks = page(&store);
        let k = rows(&blocks).into_iter().find(|r| r.label == "Keep for").unwrap();
        assert_eq!(k.layer, Layer::User);
        assert_eq!(k.fallback, Some(("14 days".into(), Layer::Team)));
        let p = rows(&blocks).into_iter().find(|r| r.label == "Memory is tight when RAM free drops below").unwrap();
        assert!(matches!(&p.ctl, Ctl::Number { unit, .. } if unit == "% · 12.8 GB"));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_number(48.0), "48");
        assert_eq!(format_number(6.5), "6.5");
        assert_eq!(format_number(0.25), "0.25");
        assert_eq!(label_of(&PRIORITIES, "verylow"), "Very low");
        assert_eq!(label_of(&SPEEDS, "eco"), "Efficiency mode");
        assert_eq!(UpdateState::UpToDate { checked: "today at 14:02".into() }.hint(), "Up to date. Checked today at 14:02.");
        assert_eq!(short_folder(std::path::Path::new(r"C:\Users\you\AppData\Roaming\AtherMemGuard\settings.toml")), r"%APPDATA%\AtherMemGuard");
    }
}

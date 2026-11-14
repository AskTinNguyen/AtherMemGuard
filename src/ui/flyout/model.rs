//! What the flyout shows, built from the engine's `Status` (pure): the RAM rows, the Unreal
//! band, the critical box, and the menu each row opens.

use super::super::kit::format_gb;
use super::super::theme::*;
use super::super::commands::{Command, UiAction};
use crate::breakdown::{RamBreakdown, RamPart};
use crate::config::Pressure;
use crate::engine::{Level, Status};

pub const CHECKUP_LABEL: &str = "Run a checkup";
pub const FIX_LABEL: &str = "Review and close";
pub const COMMIT_HINT: &str = "Room left for apps to ask for more memory: RAM plus the page file. When it runs out, apps crash even if RAM is free.";


#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    pub label: String,
    /// "45.1 of 128 GB"
    pub value: String,
    /// Share free, 0..1.
    pub frac: f32,
    pub color: Rgb,
}

/// One part of the "where your RAM is" bar: a share of total RAM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub frac: f32,
    pub color: Rgb,
}

/// One line of the RAM list.
#[derive(Debug, Clone, PartialEq)]
pub struct RamRow {
    pub name: String,
    /// "4.1 GB"; empty for a link.
    pub value: String,
    pub swatch: Rgb,
    pub kind: RowKind,
}

/// What a RAM row stands for: how it is drawn and what a click on it does.
#[derive(Debug, Clone, PartialEq)]
pub enum RowKind {
    /// A part of the RAM in use besides the groups: a main row with a swatch and a menu.
    Part(RamPart),
    /// One app group, or several small ones sharing a row: a main row with a menu.
    Groups(Vec<String>),
    /// One named app inside "Other apps": an indented row with a menu.
    App(String),
    /// "61 smaller apps": an indented row, nothing to click.
    Rest,
    /// A link under the row above ("Empty it now"): a click runs the command.
    Link(Command),
}

impl RowKind {
    /// A main row (swatch, bold value), not an indented one.
    pub fn is_main(&self) -> bool {
        matches!(self, RowKind::Part(_) | RowKind::Groups(_))
    }
}

/// One line of a row menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    Action(String, UiAction),
    /// Greyed-out text: what the row is, when there's nothing to do about it.
    Info(String),
    Separator,
}

/// Everything the flyout shows.
#[derive(Debug, Clone, PartialEq)]
pub struct FlyoutData {
    /// "EDITOR + BUILD", "IDLE"
    pub eyebrow: String,
    /// "MEMORY IS FINE", "MEMORY IS TIGHT", "MEMORY IS CRITICAL", "PAUSED"
    pub headline: String,
    pub headline_color: Rgb,
    /// "92.6 of 96 GB"
    pub ram_in_use: String,
    /// "3.4 GB free", coloured like a free-memory meter.
    pub ram_free: String,
    pub ram_free_color: Rgb,
    /// The stacked bar, in the order of `rows` (Unreal first). The empty rest is free.
    pub segments: Vec<Segment>,
    /// "Unreal · 52.9 GB" or "Unreal isn't running"
    pub ue_title: String,
    /// Lines under the title, each wrapped on its own: what Unreal's RAM is, then
    /// "Up 30 min · growing 2.1 GB an hour · Windows promised it 47.0 GB".
    pub ue_detail: Vec<String>,
    /// Ranked, largest first; they add up to the RAM in use with Unreal.
    pub rows: Vec<RamRow>,
    /// When memory is critical: what to close and what it gets back. Shown in a red-edged box
    /// whose link opens the checkup.
    pub fix: Option<String>,
    pub commit: Meter,
    /// One muted line above the buttons (at most two lines when painted).
    pub advice: Option<String>,
    pub paused: bool,
    /// For the row menus: every group's name, the biggest other apps (exe, GB) and whether
    /// Unreal is running.
    pub group_names: Vec<String>,
    pub top_apps: Vec<(String, f64)>,
    pub ue_running: bool,
}

/// "editor+build" -> "Editor + build", "idle" -> "Idle".
pub fn mode_words(mode: &str) -> String {
    let s = mode.split('+').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" + ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "Idle".into(),
    }
}

/// The pressure in the UI's words.
pub fn pressure_words(level: Level) -> &'static str {
    match level {
        Level::Normal => "fine",
        Level::Elevated => "tight",
        Level::Critical => "critical",
    }
}

/// Colour of a free-memory meter: lime above the tight threshold, amber below it, red below
/// the critical one (percent free).
pub fn meter_color(free_pct: f64, tight_pct: f64, critical_pct: f64) -> Rgb {
    if free_pct < critical_pct {
        ERROR
    } else if free_pct < tight_pct {
        WARN
    } else {
        ACCENT
    }
}

fn gb_text(gb: f64) -> String {
    format_gb(gb)
}

/// "Up 30 min", "Up 7 h 30 min".
pub fn uptime_words(min: u64) -> String {
    if min < 60 {
        format!("Up {} min", min)
    } else {
        format!("Up {} h {} min", min / 60, min % 60)
    }
}

/// Swatch colours of the RAM parts (docs/UI.md section 3): lime is Unreal, off-white is
/// what you could close yourself, olive is what MemGuard already holds back, the greys are
/// Windows' own.
pub const SW_UNREAL: Rgb = ACCENT;
pub const SW_OTHER: Rgb = TEXT;
pub const SW_GROUP: Rgb = Rgb(159, 167, 122);
pub const SW_CACHE: Rgb = Rgb(112, 136, 146);
pub const SW_WINDOWS: Rgb = Rgb(124, 125, 117);
pub const SW_SQUEEZED: Rgb = Rgb(90, 91, 84);
pub const SW_SHARED: Rgb = Rgb(68, 70, 63);

/// A group gets its own row from this size; smaller groups share one.
const GROUP_ROW_GB: f64 = 1.0;
/// The critical box mentions the file cache from this size.
const CACHE_IN_FIX_FROM_GB: f64 = 2.0;
/// "Empty it now" shows under "Windows file cache" from this size.
const EMPTY_CACHE_FROM_GB: f64 = 1.0;
pub const EMPTY_CACHE_LABEL: &str = "Empty it now (needs admin)";
/// How many other apps are named.
const NAMED_APPS: usize = 3;
/// Other apps are named from this size, or whenever memory is tight.
const NAME_APPS_FROM_GB: f64 = 1.0;

/// A friendlier name for a few executables whose names say little.
pub fn app_words(exe: &str) -> String {
    match exe.to_ascii_lowercase().as_str() {
        "vmmem" | "vmmemwsl" => "Linux VMs (WSL)".into(),
        "rider64" => "Rider".into(),
        "devenv" => "Visual Studio".into(),
        "code" => "VS Code".into(),
        "msedge" => "Edge".into(),
        "chrome" => "Chrome".into(),
        "firefox" => "Firefox".into(),
        "zenserver" => "Unreal Zen server".into(),
        _ => exe.to_string(),
    }
}

/// "A, B and C"
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {}", rest.join(", "), last),
    }
}

impl FlyoutData {
    /// The flyout for `st`. `growth` is the editor's growth in GB an hour (from the memory
    /// history), `pause_note` the line shown while paused ("Other apps are back to normal
    /// until 15:42.").
    pub fn new(st: &Status, pressure: &Pressure, growth: Option<f64>, pause_note: Option<String>) -> FlyoutData {
        let sys = &st.system;
        let br = &st.ram;
        let in_use = if br.in_use_gb > 0.0 { br.in_use_gb } else { (sys.ram_total_gb - sys.ram_free_gb).max(0.0) };
        let (ue_running, ue_title, ue_detail) = unreal_band(st, growth);
        let (segments, rows) = ram_rows(br, sys.ram_total_gb.max(in_use), st.pressure >= Level::Elevated);
        let (headline, headline_color) = headline(st);
        let fix = (!st.paused && st.pressure == Level::Critical).then(|| fix_text(br));
        // The engine's own "Memory is critical" line says the box's numbers again: it gives way.
        let engine_advice = || st.advice.iter().find(|a| fix.is_none() || !a.starts_with("Memory is critical")).cloned();
        let advice = if st.paused { pause_note.or_else(engine_advice) } else { engine_advice() };
        FlyoutData {
            eyebrow: mode_words(&st.mode).to_uppercase(),
            headline,
            headline_color,
            ram_in_use: format!("{:.1} of {:.0} GB", in_use, sys.ram_total_gb),
            ram_free: format!("{} free", gb_text(sys.ram_free_gb)),
            ram_free_color: meter_color(sys.ram_free_pct, pressure.elevated_ram_free_pct, pressure.critical_ram_free_pct),
            segments,
            ue_title,
            ue_detail,
            rows,
            fix,
            commit: Meter {
                label: "Commit free".into(),
                value: format!("{:.1} of {:.0} GB", sys.commit_free_gb, sys.commit_limit_gb),
                frac: if sys.commit_limit_gb > 0.0 { (sys.commit_free_gb / sys.commit_limit_gb).clamp(0.0, 1.0) as f32 } else { 0.0 },
                color: meter_color(sys.commit_free_pct, pressure.elevated_commit_free_pct, pressure.critical_commit_free_pct),
            },
            advice,
            paused: st.paused,
            group_names: st.groups.iter().map(|g| g.name.clone()).collect(),
            top_apps: br.top_apps.iter().filter(|a| a.ram_gb >= 0.1).map(|a| (a.name.clone(), a.ram_gb)).collect(),
            ue_running,
        }
    }

    /// The Unreal band's menu.
    pub fn unreal_menu(&self) -> Vec<MenuItem> {
        let mut m = Vec::new();
        if self.ue_running {
            m.push(MenuItem::Info("Restarting the editor gets back the memory it has piled up.".into()));
            m.push(MenuItem::Separator);
        }
        m.push(MenuItem::Action("End leftover Unreal helpers…".into(), UiAction::Run(Command::EndUnrealLeftovers)));
        m.push(MenuItem::Action("Memory history".into(), UiAction::Run(Command::OpenHistory)));
        m
    }

    /// The menu a click on a row of `kind` opens (empty: nothing to open).
    pub fn row_menu(&self, kind: &RowKind) -> Vec<MenuItem> {
        use MenuItem::{Action, Info, Separator};
        let run = |label: &str, c: Command| Action(label.to_string(), UiAction::Run(c));
        let empty_caches = || run("Empty Windows caches (needs admin)", Command::FreeWindowsCache);
        match kind {
            RowKind::Rest | RowKind::Link(_) => Vec::new(),
            RowKind::Groups(names) => {
                let mut m: Vec<MenuItem> =
                    names.iter().map(|n| Action(format!("Free memory from {} now", n), UiAction::FreeGroup(n.clone()))).collect();
                m.push(Separator);
                m.push(run(if names.len() == 1 { "Change how it's held back…" } else { "Change how they're held back…" }, Command::OpenSettings));
                m
            }
            RowKind::App(exe) => {
                let mut m = vec![Action(format!("Close {}", app_words(exe)), UiAction::CloseApp(exe.clone()))];
                if !self.group_names.is_empty() {
                    m.push(Separator);
                    m.extend(
                        self.group_names
                            .iter()
                            .map(|g| Action(format!("Hold it back with {}", g), UiAction::AddToGroup { app: exe.clone(), group: g.clone() })),
                    );
                }
                m
            }
            RowKind::Part(RamPart::OtherApps) => {
                let mut m: Vec<MenuItem> = self
                    .top_apps
                    .iter()
                    .take(5)
                    .map(|(exe, gb)| Action(format!("Close {} ({})", app_words(exe), gb_text(*gb)), UiAction::CloseApp(exe.clone())))
                    .collect();
                if !m.is_empty() {
                    m.push(Separator);
                }
                m.push(run("Run a checkup", Command::OpenDoctor));
                m
            }
            RowKind::Part(RamPart::FileCache) => vec![Info("Files Windows keeps in memory after reading them.".into()), Separator, empty_caches()],
            RowKind::Part(RamPart::Shared) => {
                vec![Info("Memory apps share, and changed files waiting to be written.".into()), Separator, empty_caches()]
            }
            RowKind::Part(RamPart::Windows) => vec![
                Info("Windows itself, its services and hardware drivers.".into()),
                Separator,
                run("Which drivers use it?", Command::ShowDriverReport),
            ],
            RowKind::Part(RamPart::Squeezed) => vec![
                Info("Other apps' memory, packed smaller. It shrinks as they close.".into()),
                Separator,
                run("Run a checkup", Command::OpenDoctor),
            ],
            // Groups come as `RowKind::Groups`.
            RowKind::Part(RamPart::Group(_)) => Vec::new(),
        }
    }

    /// The three buttons: label, command, primary.
    pub fn buttons(&self) -> [(&'static str, Command); 3] {
        [
            ("Free memory", Command::FreeMemory),
            if self.paused { ("Resume", Command::Resume) } else { ("Pause 30 min", Command::Pause30) },
            ("Settings", Command::OpenSettings),
        ]
    }

    /// The link that opens the checkup: inside the red box when there is one.
    pub fn checkup_label(&self) -> &'static str {
        if self.fix.is_some() {
            FIX_LABEL
        } else {
            CHECKUP_LABEL
        }
    }
}

/// The Unreal band: (running, title, detail lines).
fn unreal_band(st: &Status, growth: Option<f64>) -> (bool, String, Vec<String>) {
    let br = &st.ram;
    let editor = st.ue.processes.iter().filter(|p| p.kind == "editor").max_by(|a, b| a.committed_gb.total_cmp(&b.committed_gb));
    let running = editor.is_some() || !st.ue.processes.is_empty() || st.activities.any();
    if !running {
        return (false, "Unreal isn't running".to_string(), Vec::new());
    }
    let workers = match st.ue.workers {
        0 => None,
        1 => Some("1 compile worker".to_string()),
        n => Some(format!("{} compile workers", n)),
    };
    let mut lines = Vec::new();
    let mut split = Vec::new();
    if br.unreal_editor_gb > 0.0 {
        split.push(format!("Editor {}", gb_text(br.unreal_editor_gb)));
    }
    if let Some(w) = &workers {
        split.push(format!("{} {}", w, gb_text(br.unreal_workers_gb)));
    }
    let rest = br.unreal_gb - br.unreal_editor_gb - br.unreal_workers_gb;
    if rest >= 0.05 && !split.is_empty() {
        split.push(format!("other {}", gb_text(rest)));
    }
    if !split.is_empty() {
        lines.push(split.join(" · "));
    }
    let mut parts = Vec::new();
    if let Some(p) = editor.or_else(|| st.ue.processes.first()) {
        parts.push(uptime_words(p.uptime_min));
        if p.kind == "editor" {
            if let Some(g) = growth.filter(|g| *g >= 0.1) {
                parts.push(format!("growing {:.1} GB an hour", g));
            }
        }
    }
    if st.ue.total_committed_gb > 0.0 {
        parts.push(format!("Windows promised it {}", gb_text(st.ue.total_committed_gb)));
    }
    if !parts.is_empty() {
        lines.push(parts.join(" · "));
    }
    (true, format!("Unreal · {}", gb_text(br.unreal_gb)), lines)
}

/// The swatch of a part.
fn swatch(part: RamPart) -> Rgb {
    match part {
        RamPart::OtherApps => SW_OTHER,
        RamPart::Group(_) => SW_GROUP,
        RamPart::FileCache => SW_CACHE,
        RamPart::Windows => SW_WINDOWS,
        RamPart::Squeezed => SW_SQUEEZED,
        RamPart::Shared => SW_SHARED,
    }
}

/// The stacked bar (Unreal first) and the rows, ranked by the size shown (0.1 GB steps), so
/// the order always matches the numbers; ties keep `RamBreakdown::parts`' order. Groups under
/// 1 GB share one row. `tight` names the biggest other apps even when they are small.
fn ram_rows(br: &RamBreakdown, total_gb: f64, tight: bool) -> (Vec<Segment>, Vec<RamRow>) {
    // (name, GB, swatch, kind, the lines under it)
    let mut parts: Vec<(String, f64, Rgb, RowKind, Vec<RamRow>)> = Vec::new();
    let mut small: Vec<&str> = Vec::new();
    let mut small_gb = 0.0;
    for (part, gb) in br.parts() {
        match part {
            RamPart::Group(i) if gb < GROUP_ROW_GB => {
                small.push(&br.groups[i].name);
                small_gb += gb;
            }
            RamPart::Group(i) => parts.push((br.label(part), gb, SW_GROUP, RowKind::Groups(vec![br.groups[i].name.clone()]), Vec::new())),
            _ => parts.push((br.label(part), gb, swatch(part), RowKind::Part(part), under(br, part, tight))),
        }
    }
    if !small.is_empty() {
        let names: Vec<String> = small.iter().map(|n| n.to_string()).collect();
        parts.push((names.join(", "), small_gb, SW_GROUP, RowKind::Groups(names), Vec::new()));
    }
    parts.sort_by_key(|p| std::cmp::Reverse((p.1 * 10.0).round() as i64));

    let share = |gb: f64| if total_gb > 0.0 { (gb / total_gb).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let mut segments = vec![Segment { frac: share(br.unreal_gb), color: SW_UNREAL }];
    let mut rows = Vec::new();
    for (name, gb, swatch, kind, under) in parts {
        segments.push(Segment { frac: share(gb), color: swatch });
        rows.push(RamRow { name, value: gb_text(gb), swatch, kind });
        rows.extend(under);
    }
    segments.retain(|s| s.frac > 0.0);
    (segments, rows)
}

/// The lines under a part's row: the biggest other apps, or the file cache's link.
fn under(br: &RamBreakdown, part: RamPart, tight: bool) -> Vec<RamRow> {
    let line = |name: String, value: String, kind: RowKind| RamRow { name, value, swatch: SW_OTHER, kind };
    match part {
        RamPart::OtherApps if tight || br.other_apps_gb >= NAME_APPS_FROM_GB => {
            let named: Vec<_> = br.top_apps.iter().filter(|a| a.ram_gb >= 0.1).take(NAMED_APPS).collect();
            let mut v: Vec<RamRow> = named.iter().map(|a| line(app_words(&a.name), gb_text(a.ram_gb), RowKind::App(a.name.clone()))).collect();
            let rest_n = br.other_apps_count.saturating_sub(named.len());
            if !named.is_empty() && rest_n > 0 {
                let rest_gb = br.other_apps_gb - named.iter().map(|a| a.ram_gb).sum::<f64>();
                let what = if rest_n == 1 { "1 smaller app".to_string() } else { format!("{} smaller apps", rest_n) };
                v.push(line(what, gb_text(rest_gb.max(0.0)), RowKind::Rest));
            }
            v
        }
        RamPart::FileCache if br.file_cache_gb >= EMPTY_CACHE_FROM_GB => {
            vec![line(EMPTY_CACHE_LABEL.into(), String::new(), RowKind::Link(Command::FreeWindowsCache))]
        }
        _ => Vec::new(),
    }
}

/// The headline and its colour.
fn headline(st: &Status) -> (String, Rgb) {
    if st.paused {
        return ("PAUSED".to_string(), TEXT);
    }
    match st.pressure {
        Level::Normal => ("MEMORY IS FINE".to_string(), TEXT),
        Level::Elevated => ("MEMORY IS TIGHT".to_string(), WARN),
        Level::Critical => ("MEMORY IS CRITICAL".to_string(), ERROR),
    }
}

/// The critical box: what emptying the file cache and closing the biggest other apps gets back.
fn fix_text(br: &RamBreakdown) -> String {
    let named: Vec<_> = br.top_apps.iter().filter(|a| a.ram_gb >= 0.5).take(NAMED_APPS).collect();
    let names: Vec<String> = named.iter().map(|a| app_words(&a.name)).collect();
    let back: f64 = named.iter().map(|a| a.ram_gb).sum();
    let cache = br.file_cache_gb >= CACHE_IN_FIX_FROM_GB;
    match (cache, named.is_empty()) {
        (false, true) => "Don't start a build or cook until this clears.".to_string(),
        (false, false) => format!("Closing {} gets about {:.1} GB back. Don't start a build or cook yet.", and_list(&names), back),
        (true, true) => format!("Emptying Windows' file cache gets about {:.1} GB back. Don't start a build or cook yet.", br.file_cache_gb),
        (true, false) => format!(
            "Emptying Windows' file cache gets about {:.1} GB back, closing {} about {:.1} GB. Don't start a build or cook yet.",
            br.file_cache_gb,
            and_list(&names),
            back
        ),
    }
}

/// The four sample states (snapshot name, data). Normal and paused use the mockup's numbers;
/// critical is a 96 GB PC with 3.4 GB free where the editor has 47 GB committed.
pub fn samples() -> Vec<(&'static str, FlyoutData)> {
    use crate::ui::status_source::sample_status;
    use crate::breakdown::{AppRam, GroupRam, RamBreakdown};
    let pressure = crate::config::Config::builtin().pressure;
    let normal = sample_status();

    let mut tight = sample_status();
    tight.pressure = Level::Elevated;
    tight.system.ram_free_gb = 11.3;
    tight.system.ram_free_pct = 8.8;
    tight.system.commit_free_gb = 21.0;
    tight.system.commit_free_pct = 14.6;
    tight.ue.processes[0].committed_gb = 58.2;
    tight.ue.processes[0].uptime_min = 245;
    tight.ue.total_committed_gb = 62.3;
    tight.ram.in_use_gb = 116.4;
    tight.ram.unreal_gb = 49.8;
    tight.ram.unreal_editor_gb = 45.9;
    tight.ram.file_cache_gb = 35.3;
    tight.ram.unattributed_gb = 6.7;

    let mut critical = sample_status();
    critical.mode = "editor".into();
    critical.activities = crate::engine::Activities { editor: true, build: false, cook: false };
    critical.pressure = Level::Critical;
    critical.system.ram_total_gb = 96.0;
    critical.system.ram_free_gb = 3.4;
    critical.system.ram_free_pct = 3.5;
    critical.system.commit_limit_gb = 128.0;
    critical.system.commit_free_gb = 45.0;
    critical.system.commit_free_pct = 35.2;
    critical.ue.processes[0].committed_gb = 47.0;
    critical.ue.processes[0].uptime_min = 5;
    critical.ue.workers = 0;
    critical.ue.workers_committed_gb = 0.0;
    critical.ue.total_committed_gb = 47.0;
    let app = |name: &str, processes: usize, ram_gb: f64| AppRam { name: name.into(), processes, ram_gb };
    let group = |name: &str, ram_gb: f64| GroupRam { name: name.into(), ram_gb };
    critical.ram = RamBreakdown {
        in_use_gb: 92.6,
        unreal_gb: 40.2,
        unreal_editor_gb: 38.5,
        unreal_workers_gb: 0.0,
        groups: vec![group("Browsers", 0.1), group("Chat", 0.1), group("Agents", 0.8), group("Background", 0.0)],
        other_apps_gb: 6.1,
        other_apps_count: 64,
        top_apps: vec![app("rider64", 1, 2.9), app("vmmem", 1, 1.6), app("devenv", 2, 0.8), app("Discord", 6, 0.2)],
        file_cache_gb: 31.5,
        windows_gb: 7.4,
        compressed_gb: 2.1,
        unattributed_gb: 4.3,
    };
    critical.groups.iter_mut().for_each(|g| g.ram_mb /= 40);
    critical.advice = vec!["Memory is critical: 3.4 GB RAM free, 45.0 GB commit free. Don't start a build or cook until this clears.".into()];

    let mut paused = sample_status();
    paused.paused = true;

    vec![
        ("normal", FlyoutData::new(&normal, &pressure, Some(2.1), None)),
        ("tight", FlyoutData::new(&tight, &pressure, Some(6.8), None)),
        ("critical", FlyoutData::new(&critical, &pressure, None, None)),
        ("paused", FlyoutData::new(&paused, &pressure, Some(2.1), Some("Other apps are back to normal until 15:42.".into()))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::status_source::sample_status;

    fn data() -> FlyoutData {
        samples().remove(0).1
    }

    fn gb(v: &str) -> f64 {
        if let Some(mb) = v.strip_suffix(" MB") {
            mb.parse::<f64>().unwrap() / 1024.0
        } else {
            v.strip_suffix(" GB").unwrap().parse().unwrap()
        }
    }

    #[test]
    fn words_and_colours() {
        assert_eq!(mode_words("editor+build"), "Editor + build");
        assert_eq!(mode_words("idle"), "Idle");
        assert_eq!(mode_words(""), "Idle");
        assert_eq!(pressure_words(Level::Elevated), "tight");
        assert_eq!(meter_color(35.0, 10.0, 4.0), ACCENT);
        assert_eq!(meter_color(8.0, 10.0, 4.0), WARN);
        assert_eq!(meter_color(3.0, 10.0, 4.0), ERROR);
        assert_eq!(uptime_words(30), "Up 30 min");
        assert_eq!(uptime_words(450), "Up 7 h 30 min");
        assert_eq!(app_words("vmmemWSL"), "Linux VMs (WSL)");
        assert_eq!(app_words("Notion"), "Notion");
        assert_eq!(and_list(&["A".into(), "B".into(), "C".into()]), "A, B and C");
        assert_eq!(and_list(&["A".into()]), "A");
    }

    #[test]
    fn data_from_status() {
        let d = data();
        assert_eq!(d.eyebrow, "EDITOR + BUILD");
        assert_eq!(d.headline, "MEMORY IS FINE");
        assert_eq!(d.ram_in_use, "82.6 of 128 GB");
        assert_eq!((d.ram_free.as_str(), d.ram_free_color), ("45.1 GB free", ACCENT));
        assert_eq!(d.commit.value, "27.6 of 144 GB");
        assert_eq!(d.ue_title, "Unreal · 34.0 GB");
        assert_eq!(d.ue_detail, vec!["Editor 30.1 GB · 3 compile workers 2.4 GB · other 1.5 GB", "Up 30 min · growing 2.1 GB an hour · Windows promised it 47.9 GB"]);
        // Ranked, largest first; groups of 1 GB and up get a row, the rest share one.
        let names: Vec<&str> = d.rows.iter().filter(|r| r.kind.is_main()).map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Windows file cache", "Other apps", "Agents", "Windows and drivers", "Browsers", "Shared or not itemized", "Squeezed by Windows", "Chat, Background"]);
        // Other apps is over 1 GB, so its biggest three are named, then the rest.
        let other = d.rows.iter().position(|r| r.name == "Other apps").unwrap();
        let subs: Vec<(&str, &str)> = d.rows[other + 1..other + 5].iter().map(|r| (r.name.as_str(), r.value.as_str())).collect();
        assert_eq!(subs, [("Rider", "2.8 GB"), ("Linux VMs (WSL)", "1.9 GB"), ("Visual Studio", "717 MB"), ("58 smaller apps", "819 MB")]);
        assert!(d.rows[other + 1..other + 5].iter().all(|r| !r.kind.is_main()));
        // The parts add up to the RAM in use, and the bar to its share of total RAM.
        let sum: f64 = d.rows.iter().filter(|r| r.kind.is_main()).map(|r| gb(&r.value)).sum::<f64>() + 34.0;
        assert!((sum - 82.6).abs() < 0.15, "{}", sum);
        let bar: f32 = d.segments.iter().map(|s| s.frac).sum();
        assert!((bar - 82.6 / 127.7).abs() < 0.01, "{}", bar);
        assert_eq!(d.segments[0].color, SW_UNREAL);
        assert!(d.fix.is_none() && d.advice.is_none());
        // The file cache row is followed by its "Empty it now" link.
        let cache = d.rows.iter().position(|r| r.name == "Windows file cache").unwrap();
        assert_eq!(d.rows[cache + 1].kind, RowKind::Link(Command::FreeWindowsCache));
        assert!(d.rows[cache + 1].value.is_empty());
        assert_eq!(d.rows.iter().filter(|r| matches!(r.kind, RowKind::Link(_))).count(), 1);
        assert_eq!(d.checkup_label(), CHECKUP_LABEL);
        assert_eq!(d.buttons()[1], ("Pause 30 min", Command::Pause30));

        let (_, crit) = samples().remove(2);
        assert_eq!((crit.headline.as_str(), crit.headline_color), ("MEMORY IS CRITICAL", ERROR));
        assert_eq!((crit.ram_in_use.as_str(), crit.ram_free.as_str(), crit.ram_free_color), ("92.6 of 96 GB", "3.4 GB free", ERROR));
        assert_eq!(crit.ue_detail, vec!["Editor 38.5 GB · other 1.7 GB", "Up 5 min · Windows promised it 47.0 GB"]);
        assert_eq!(
            crit.fix.as_deref(),
            Some("Emptying Windows' file cache gets about 31.5 GB back, closing Rider, Linux VMs (WSL) and Visual Studio about 5.3 GB. Don't start a build or cook yet.")
        );
        assert_eq!(crit.checkup_label(), FIX_LABEL);
        assert!(crit.advice.is_none(), "the box replaces the engine's critical line");
        assert_eq!(crit.rows[0].name, "Windows file cache");

        let (_, p) = samples().remove(3);
        assert_eq!(p.headline, "PAUSED");
        assert_eq!(p.buttons()[1], ("Resume", Command::Resume));
        assert_eq!(p.advice.as_deref(), Some("Other apps are back to normal until 15:42."));
        assert!(p.fix.is_none());

        let mut idle = sample_status();
        idle.mode = "idle".into();
        idle.activities = Default::default();
        idle.ue = Default::default();
        idle.ram.unreal_gb = 0.0;
        idle.ram.other_apps_gb = 0.4;
        let d = FlyoutData::new(&idle, &crate::config::Config::builtin().pressure, None, None);
        assert_eq!((d.eyebrow.as_str(), d.ue_title.as_str()), ("IDLE", "Unreal isn't running"));
        assert!(d.ue_detail.is_empty());
        assert!(!d.rows.iter().any(|r| matches!(r.kind, RowKind::App(_) | RowKind::Rest)), "small other apps aren't itemized when memory is fine");
    }
}

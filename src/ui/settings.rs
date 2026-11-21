//! The settings window (docs/UI.md section 1): search, hero with the live strip, every
//! section, group cards with the four tiers, chips and the add-app popup, danger controls with
//! confirmation, layer dot / TEAM tag, right-click reset, footer, remembered placement.
//!
//! Content comes from `settings_model::build` over a `SettingsStore`; each paint lays the page
//! out into a flat list of `Item`s (rect + what to draw + what a click does), which hit tests,
//! keyboard focus and the layout tests use.

use std::collections::HashSet;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

use super::canvas::{Align, Canvas};
use super::commands::Command;
use super::geom::{Pt, Rect};
use super::kit::*;
use super::settings_model::*;
use super::settings_store::{group_path, strip_exe, Layer, RunningApp, SettingsStore, Tier, Value};
use super::status_source::StatusSource;
use super::theme::*;
use super::toast;
use super::window::{self, Cursor, Event, Host, MenuItem, Mods, MouseButton, Surface, WindowSpec};
use crate::engine::{Level, Status};

pub use super::settings_model::UpdateState;

/// Window kind for `window::find`.
pub const KIND: &str = "settings";
pub const TITLE: &str = "Ather MemGuard — Settings";

const TIMER_REFRESH: usize = 1;
const TIMER_CARET: usize = 2;

/// Everything the settings window talks to.
pub struct SettingsHost {
    pub store: Box<dyn SettingsStore>,
    pub status: Box<dyn StatusSource>,
    /// Commands for the tray loop: OpenHistory, CheckUpdates, InstallUpdate, CopyAgentSetup.
    pub commands: Box<dyn Fn(Command)>,
    /// Shown as "Version X".
    pub app_version: String,
    pub update_state: Box<dyn Fn() -> UpdateState>,
}

/// Open the settings window, or bring the open one to the front. Call on the UI thread
/// (it needs a message loop). Size and position are remembered in ui-state.toml.
pub fn open_settings(host: SettingsHost) -> HWND {
    if let Some(h) = window::find(KIND) {
        window::bring_to_front(h);
        return h;
    }
    let spec = WindowSpec {
        kind: KIND,
        title: TITLE.into(),
        size: (900, 760),
        min_size: (720, 480),
        placement: super::ui_state::load(KIND),
        resizable: true,
    };
    window::open(spec, Box::new(SettingsWindow::new(host)))
}

// ---------------------------------------------------------------- items

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChipOwner {
    List(String),
    Group(String),
}

/// What a click or Enter on an item does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Toggle(String),
    Startup,
    Seg(String),
    Hotkey(String),
    Number(String),
    Chips(ChipOwner),
    Button(ButtonAct),
    GroupHeader(String),
    Priority(String),
    Slider(String, Tier),
    Speed(String, Tier),
}

/// What right-click on a row offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// A row in `model[block]`, or row `inner` of the group card at `model[block]`.
    Row { block: usize, inner: Option<usize> },
    GroupHeader(String),
    Tier(String, Tier),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    RowBg,
    Section(String),
    Label { label: String, hint: Vec<String>, danger: bool, layer: Layer },
    Toggle(Toggle),
    Seg(Segmented),
    Pill(Pill),
    Num(NumberField),
    Slider(Slider),
    Chips(Chips),
    Button(Button),
    Hotkey(HotkeyField),
    Bar(Bar),
    Text { s: String, font: Font, color: Rgb, align: Align },
    Hairline(Rgb),
    Card,
    Chevron(bool),
    GroupName { name: String, sub: String, layer: Layer, enabled: bool },
    Read { value: String, sub: String },
    TierLabel { label: String, hint: Option<String> },
    TierHead(String, Align),
    /// Live strip caption: small uppercase, letter-spaced, right-aligned.
    Caption(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub rect: Rect,
    pub paint: Paint,
    pub act: Option<Act>,
    pub row: Option<RowKey>,
    pub faded: bool,
}

impl Item {
    fn new(rect: Rect, paint: Paint) -> Item {
        Item { rect, paint, act: None, row: None, faded: false }
    }
    fn act(mut self, a: Act) -> Item {
        self.act = Some(a);
        self
    }
    fn row(mut self, k: RowKey) -> Item {
        self.row = Some(k);
        self
    }
    fn faded(mut self, f: bool) -> Item {
        self.faded = f;
        self
    }
}

enum Pending {
    Reset(RowKey),
    Priority(String),
    Speed(String, Tier),
    AddApp { group: String, create: bool, apps: Vec<RunningApp> },
    GroupMenu(String),
}

enum EditTarget {
    Number(String),
    AddChip(String),
    AddApp { group: String, create: bool },
    AddGroup,
    Rename(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SettingsHover {
    act: Option<Act>,
    sub: Option<usize>,
    row: Option<RowKey>,
    dialog: Option<bool>,
}

enum Confirm {
    HardCap { path: String, mb: f64 },
    ResetAll,
    RemoveGroup(String),
}

enum Drag {
    Slider { group: String, tier: Tier, rect: Rect, pct: f64 },
    Thumb { grab: i32 },
}

const MENU_RESET: u32 = 1;
const MENU_PICK: u32 = 2;
const MENU_ID_RESET: u32 = 1;
const MENU_ID_ABOVE: u32 = 2;
const MENU_ID_TURN: u32 = 10;
const MENU_ID_RENAME: u32 = 11;
const MENU_ID_REMOVE: u32 = 12;
const MENU_ID_TYPE: u32 = 999;
const FILE_EXPORT: u32 = 1;
const FILE_IMPORT: u32 = 2;
const FILE_TEAM: u32 = 3;

pub struct SettingsWindow {
    host: SettingsHost,
    model: Vec<Block>,
    status: Option<Status>,
    update: UpdateState,
    revision: u64,
    pub query: String,
    open_group: Option<String>,
    expanded_chips: HashSet<ChipOwner>,
    items: Vec<Item>,
    scroll: Scroll,
    mouse: Option<Pt>,
    hover: SettingsHover,
    dialog_rects: Option<(Rect, Rect)>,
    pressed: Option<Act>,
    focus: Option<Act>,
    drag: Option<Drag>,
    recording: Option<String>,
    rec_partial: String,
    editing: Option<EditTarget>,
    pending: Option<Pending>,
    confirm: Option<Confirm>,
    caret_on: bool,
    focused: bool,
    minimized: bool,
    tier_speed_click: bool,
    scale: Scale,
    size: (i32, i32),
}

impl SettingsWindow {
    pub fn new(host: SettingsHost) -> SettingsWindow {
        let mut w = SettingsWindow {
            host,
            model: Vec::new(),
            status: None,
            update: UpdateState::Unknown,
            revision: 0,
            query: String::new(),
            open_group: None,
            expanded_chips: HashSet::new(),
            items: Vec::new(),
            scroll: Scroll::default(),
            mouse: None,
            hover: SettingsHover::default(),
            dialog_rects: None,
            pressed: None,
            focus: None,
            drag: None,
            recording: None,
            rec_partial: String::new(),
            editing: None,
            pending: None,
            confirm: None,
            caret_on: true,
            focused: true,
            minimized: false,
            tier_speed_click: false,
            scale: Scale(1.0),
            size: (0, 0),
        };
        w.status = w.host.status.latest();
        w.reload();
        w
    }

    /// Re-read every value from the store.
    pub fn reload(&mut self) {
        self.update = (self.host.update_state)();
        self.revision = self.host.store.revision();
        let input = ModelInput { store: self.host.store.as_ref(), status: self.status.as_ref(), app_version: &self.host.app_version, update: &self.update };
        self.model = build(&input);
        if let Some(g) = &self.open_group {
            if !self.model.iter().any(|b| matches!(b, Block::Group(m) if &m.info.name == g)) {
                self.open_group = None;
            }
        }
    }

    #[cfg(any(test, feature = "ui-snapshots"))]
    pub fn set_query(&mut self, q: &str) {
        self.query = q.to_string();
        self.scroll.y = 0;
    }

    #[cfg(any(test, feature = "ui-snapshots"))]
    pub fn set_open_group(&mut self, name: Option<&str>) {
        self.open_group = name.map(str::to_string);
    }

    /// Show the hard-cap confirmation (snapshots).
    #[cfg(any(test, feature = "ui-snapshots"))]
    pub fn confirm_hard_cap(&mut self, group: &str, mb: f64) {
        self.confirm = Some(Confirm::HardCap { path: group_path(group, Tier::Always, "hard_cap_mb"), mb });
        self.dialog_rects = None;
    }

    /// Put the mouse somewhere (snapshots of hover states).
    #[cfg(any(test, feature = "ui-snapshots"))]
    pub fn set_mouse(&mut self, p: Option<Pt>) {
        self.mouse = p;
    }

    /// The items of the last layout (tests).
    #[cfg(any(test, feature = "ui-snapshots"))]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Whole page height (search bar + content + footer) at `width`, for full-page snapshots.
    #[cfg(any(test, feature = "ui-snapshots"))]
    pub fn page_height(&mut self, c: &Canvas) -> i32 {
        let saved = self.scroll.y;
        self.scroll.y = 0;
        self.layout(c);
        self.scroll.y = saved;
        c.s(60.0) + self.scroll.content + c.s(44.0)
    }

    fn row(&self, key: &RowKey) -> Option<&Row> {
        match key {
            RowKey::Row { block, inner: None } => match self.model.get(*block) {
                Some(Block::Row(r)) => Some(r),
                _ => None,
            },
            RowKey::Row { block, inner: Some(i) } => match self.model.get(*block) {
                Some(Block::Group(g)) => g.rows.get(*i),
                _ => None,
            },
            _ => None,
        }
    }

    fn group(&self, name: &str) -> Option<&GroupModel> {
        self.model.iter().find_map(|b| match b {
            Block::Group(g) if g.info.name == name => Some(g),
            _ => None,
        })
    }

    fn ctl_for_path(&self, path: &str) -> Option<(&Row, &Ctl)> {
        let all = self.model.iter().flat_map(|b| match b {
            Block::Row(r) => vec![r],
            Block::Group(g) => g.rows.iter().collect(),
            _ => vec![],
        });
        for r in all {
            match &r.ctl {
                Ctl::Number { path: p, .. } | Ctl::Toggle { path: p, .. } | Ctl::Segmented { path: p, .. } | Ctl::Hotkey { path: p, .. } if p == path => {
                    return Some((r, &r.ctl))
                }
                Ctl::Headroom { ram_path, commit_path, disk_path, .. } if ram_path == path || commit_path == path || disk_path == path => {
                    return Some((r, &r.ctl))
                }
                _ => {}
            }
        }
        None
    }

    fn ram_total(&self) -> f64 {
        self.status.as_ref().map(|s| s.system.ram_total_gb).unwrap_or(0.0)
    }

    // ------------------------------------------------------------ layout

    fn view_rect(&self, c: &Canvas) -> Rect {
        let top = c.s(60.0) + c.px1(1.0);
        Rect::ltrb(0, top, c.width(), c.height() - c.s(44.0))
    }

    /// Which blocks are visible for the current query, and which groups open.
    fn visibility(&self) -> (Vec<bool>, Vec<Vec<bool>>, Vec<bool>, Vec<bool>) {
        let q = self.query.trim();
        let n = self.model.len();
        let mut vis = vec![true; n];
        let mut inner: Vec<Vec<bool>> = vec![Vec::new(); n];
        let mut tiers_vis = vec![true; n];
        let mut open = vec![false; n];
        for (i, b) in self.model.iter().enumerate() {
            match b {
                Block::Section(_) => {}
                Block::Row(r) => vis[i] = matches(q, &r.haystack()),
                Block::Group(g) => {
                    let head: Vec<String> = g.header_haystack();
                    let head_refs: Vec<&str> = head.iter().map(|s| s.as_str()).collect();
                    let head_match = !q.is_empty() && matches(q, &head_refs);
                    let rows: Vec<bool> = g
                        .rows
                        .iter()
                        .map(|r| {
                            let mut h = r.haystack();
                            h.push(&g.info.name);
                            q.is_empty() || head_match || matches(q, &h)
                        })
                        .collect();
                    let tiers_text = g.tiers_haystack();
                    let tiers = q.is_empty() || head_match || matches(q, &[&tiers_text, &g.info.name]);
                    let any_inner = !q.is_empty() && (rows.iter().any(|v| *v) || tiers);
                    vis[i] = q.is_empty() || head_match || any_inner;
                    open[i] = if q.is_empty() { self.open_group.as_deref() == Some(g.info.name.as_str()) } else { any_inner };
                    inner[i] = rows;
                    tiers_vis[i] = tiers;
                }
            }
        }
        // Sections only when something under them shows.
        for i in 0..n {
            if let Block::Section(_) = self.model[i] {
                let mut any = false;
                let mut j = i + 1;
                while j < n && !matches!(self.model[j], Block::Section(_)) {
                    any |= vis[j];
                    j += 1;
                }
                vis[i] = any;
            }
        }
        (vis, inner, tiers_vis, open)
    }

    /// Lay the page out for the canvas size; fills `self.items` and the scroll extent.
    pub fn layout(&mut self, c: &Canvas) {
        let mut items: Vec<Item> = Vec::new();
        let view = self.view_rect(c);
        self.scroll.view = view;
        let w = c.width();
        let content_w = (w - 2 * c.s(32.0)).min(c.s(1040.0));
        let left = (w - content_w) / 2;
        let right = left + content_w;
        let top = view.y + c.s(28.0) - self.scroll.y;
        let mut y = top;

        if self.query.trim().is_empty() {
            y = self.layout_hero(c, &mut items, left, right, y);
            y += c.s(8.0);
        }

        let (vis, inner, tiers_vis, open) = self.visibility();
        let n = self.model.len();
        for i in 0..n {
            if !vis[i] {
                continue;
            }
            match &self.model[i] {
                Block::Section(name) => {
                    let lh = c.line_height(F_SECTION);
                    let h = c.s(26.0) + lh + c.s(8.0);
                    items.push(Item::new(Rect::new(left, y + c.s(26.0), content_w, lh), Paint::Section(name.to_uppercase())));
                    items.push(Item::new(Rect::new(left, y + h - c.px1(1.0), content_w, c.px1(1.0)), Paint::Hairline(BORDER)));
                    y += h;
                }
                Block::Row(r) => {
                    let key = RowKey::Row { block: i, inner: None };
                    y += self.layout_row(c, &mut items, r, key, left, right, y, c.s(66.0), true);
                }
                Block::Group(g) => {
                    y += c.s(12.0);
                    y = self.layout_group(c, &mut items, i, g, open[i], &inner[i], tiers_vis[i], left, right, y);
                    let next_is_group = (i + 1..n).find(|j| vis[*j]).map(|j| matches!(self.model[j], Block::Group(_))).unwrap_or(false);
                    if !next_is_group {
                        y += c.s(12.0);
                    }
                }
            }
        }
        y += c.s(8.0);
        self.scroll.content = y - top + c.s(28.0);
        self.scroll.clamp();
        self.items = items;
    }

    fn layout_hero(&self, c: &Canvas, items: &mut Vec<Item>, left: i32, right: i32, y: i32) -> i32 {
        let ls = c.s(64.0);
        items.push(Item::new(Rect::new(left, y, ls, ls), Paint::Text { s: String::new(), font: F_BODY, color: BG, align: Align::Left }));
        // The logo is painted from the first hero item (see paint_item); text follows.
        let tx = left + ls + c.s(18.0);
        let eh = c.line_height(F_EYEBROW);
        let hh = c.s(44.0);
        let block = eh + c.s(6.0) + hh;
        let ty = y + (ls - block) / 2;
        let (headline_w, _) = c.measure("SETTINGS", F_HEADLINE);
        let eyebrow_w = c.measure_spaced("ATHER MEMGUARD", F_EYEBROW, c.s(3.0)).0;
        items.push(Item::new(Rect::new(tx, ty, eyebrow_w + c.s(4.0), eh), Paint::Text { s: "ATHER MEMGUARD".into(), font: F_EYEBROW, color: MUTED, align: Align::Left }));
        items.push(Item::new(Rect::new(tx, ty + eh + c.s(6.0), headline_w + c.s(4.0), hh), Paint::Text { s: "SETTINGS".into(), font: F_HEADLINE, color: TEXT, align: Align::Left }));

        // Live strip: four right-aligned cells.
        let cells = self.strip_cells();
        let gap = c.s(22.0);
        let vh = c.line_height(F_STRIP);
        let ch = c.line_height(F_CAPTION);
        let widths: Vec<i32> = cells
            .iter()
            .map(|(v, cap, _)| c.measure(v, F_STRIP).0.max(c.measure_spaced(cap, F_CAPTION, c.s(1.3)).0) + c.s(2.0))
            .collect();
        let strip_w: i32 = widths.iter().sum::<i32>() + gap * (widths.len() as i32 - 1);
        let text_right = tx + headline_w.max(eyebrow_w) + c.s(24.0);
        let (strip_y, bottom) = if right - strip_w >= text_right {
            (y + (ls - (vh + c.s(2.0) + ch)) / 2, y + ls)
        } else {
            // Narrow window: the strip goes under the headline.
            (y + ls + c.s(14.0), y + ls + c.s(14.0) + vh + c.s(2.0) + ch)
        };
        let mut x = right;
        for (i, (v, cap, col)) in cells.iter().enumerate().rev() {
            let cw = widths[i];
            x -= cw;
            items.push(Item::new(Rect::new(x, strip_y, cw, vh), Paint::Text { s: v.clone(), font: F_STRIP, color: *col, align: Align::Right }));
            items.push(Item::new(Rect::new(x, strip_y + vh + c.s(2.0), cw, ch), Paint::Caption(cap.clone())));
            x -= gap;
        }
        bottom
    }

    fn strip_cells(&self) -> Vec<(String, String, Rgb)> {
        match &self.status {
            Some(st) => {
                let mode = if st.paused {
                    "PAUSED".to_string()
                } else if st.activities.any() {
                    st.activities.label().replace('+', " + ").to_uppercase()
                } else {
                    "IDLE".to_string()
                };
                let (p, col) = match st.pressure {
                    Level::Normal => ("NORMAL", ACCENT),
                    Level::Elevated => ("TIGHT", WARN),
                    Level::Critical => ("CRITICAL", ERROR),
                };
                vec![
                    (mode, "UNREAL".into(), TEXT),
                    (p.into(), "PRESSURE".into(), col),
                    (format!("{:.1} GB", st.system.ram_free_gb), "RAM FREE".into(), TEXT),
                    (format!("{:.1} GB", st.system.commit_free_gb), "COMMIT FREE".into(), TEXT),
                ]
            }
            None => vec![
                ("—".into(), "UNREAL".into(), MUTED),
                ("—".into(), "PRESSURE".into(), MUTED),
                ("—".into(), "RAM FREE".into(), MUTED),
                ("—".into(), "COMMIT FREE".into(), MUTED),
            ],
        }
    }

    /// One settings row; returns its height.
    #[allow(clippy::too_many_arguments)]
    fn layout_row(&self, c: &Canvas, items: &mut Vec<Item>, r: &Row, key: RowKey, x0: i32, x1: i32, y: i32, min_h: i32, line: bool) -> i32 {
        let ctl_w = if r.wide { c.s(420.0) } else { c.s(280.0) };
        let gap = c.s(16.0);
        let text_w = (x1 - x0 - ctl_w - gap).max(c.s(120.0));
        let ctl_w = ctl_w.min(x1 - x0 - text_w - gap);
        let ctl_h = self.ctl_height(c, r, ctl_w);
        let hint = if r.hint.is_empty() { Vec::new() } else { c.wrap_lines(&r.hint, F_HINT, text_w, 2) };
        let text_h = c.line_height(F_LABEL) + hint.len() as i32 * c.line_height(F_HINT);
        let h = min_h.max(text_h + c.s(16.0)).max(ctl_h + c.s(16.0));
        let rect = Rect::new(x0, y, x1 - x0, h);
        items.push(Item::new(rect, Paint::RowBg).row(key.clone()));
        items.push(
            Item::new(Rect::new(x0, y + (h - text_h) / 2, text_w, text_h), Paint::Label { label: r.label.clone(), hint, danger: r.danger, layer: r.layer })
                .row(key.clone()),
        );
        let ctl = Rect::new(x1 - ctl_w, y + (h - ctl_h) / 2, ctl_w, ctl_h);
        self.layout_ctl(c, items, r, ctl);
        if line {
            items.push(Item::new(Rect::new(x0, y + h - c.px1(1.0), x1 - x0, c.px1(1.0)), Paint::Hairline(HAIRLINE)));
        }
        h
    }

    fn chips_for(&self, c: &Canvas, r: &Row, max_w: i32) -> Option<(Chips, ChipOwner)> {
        match &r.ctl {
            Ctl::Chips { path, items, add_label } => {
                let owner = ChipOwner::List(path.clone());
                let expanded = self.expanded_chips.contains(&owner) || !self.query.is_empty();
                let compact = items.iter().map(|s| compact_chip_label(c, s, max_w)).collect();
                Some((Chips { items: compact, add_label: add_label.to_string(), max_lines: 1, expanded }, owner))
            }
            Ctl::GroupApps { group, items } => {
                let owner = ChipOwner::Group(group.clone());
                let expanded = self.expanded_chips.contains(&owner) || !self.query.is_empty();
                let compact = items.iter().map(|s| compact_chip_label(c, s, max_w)).collect();
                Some((Chips { items: compact, add_label: "+ Add app…".into(), max_lines: 3, expanded }, owner))
            }
            _ => None,
        }
    }

    fn ctl_height(&self, c: &Canvas, r: &Row, w: i32) -> i32 {
        match &r.ctl {
            Ctl::Toggle { .. } | Ctl::Startup { .. } => c.s(24.0),
            Ctl::Segmented { .. } => c.s(38.0),
            Ctl::Chips { .. } | Ctl::GroupApps { .. } => self.chips_for(c, r, w).map(|(ch, _)| ch.height(c, w)).unwrap_or(0),
            Ctl::None => 0,
            _ => c.s(CTRL_H),
        }
    }

    fn layout_ctl(&self, c: &Canvas, items: &mut Vec<Item>, r: &Row, ctl: Rect) {
        let editing_path = match &self.editing {
            Some(EditTarget::Number(p)) => Some(p.clone()),
            _ => None,
        };
        match &r.ctl {
            Ctl::Segmented { path, options, selected } => {
                let seg = Segmented { options: options.iter().map(|(_, l)| l.to_string()).collect(), selected: *selected };
                items.push(Item::new(ctl, Paint::Seg(seg)).act(Act::Seg(path.clone())));
            }
            Ctl::Toggle { path, on } => {
                let (tw, th) = Toggle::size(c);
                items.push(Item::new(ctl.right_part(tw, th), Paint::Toggle(Toggle { on: *on })).act(Act::Toggle(path.clone())));
            }
            Ctl::Startup { on } => {
                let (tw, th) = Toggle::size(c);
                items.push(Item::new(ctl.right_part(tw, th), Paint::Toggle(Toggle { on: *on })).act(Act::Startup));
            }
            Ctl::Hotkey { path, keys } => {
                let recording = self.recording.as_deref() == Some(path.as_str());
                let f = HotkeyField { keys: keys.clone(), recording, partial: self.rec_partial.clone() };
                let shown = if recording { "Press a shortcut… (Esc cancels)".to_string() } else { HotkeyField::display(keys) };
                let w = (c.measure(&shown, F_VALUE).0 + c.s(28.0)).max(c.s(120.0)).min(ctl.w);
                items.push(Item::new(ctl.right_part(w, ctl.h), Paint::Hotkey(f)).act(Act::Hotkey(path.clone())));
            }
            Ctl::Number { path, value, unit, off_at_zero, .. } => {
                let unit_w = c.measure(unit, F_HINT).0.max(c.s(62.0));
                let num_w = c.s(96.0);
                let unit_r = Rect::new(ctl.right() - unit_w, ctl.y, unit_w, ctl.h);
                let num_r = Rect::new(unit_r.x - c.s(10.0) - num_w, ctl.y, num_w, ctl.h);
                let off = *off_at_zero && *value <= 0.0;
                let text = if off { "Off".to_string() } else { format_number(*value) };
                let editing = editing_path.as_deref() == Some(path.as_str());
                items.push(Item::new(num_r, Paint::Num(NumberField { text, muted: off, editing })).act(Act::Number(path.clone())));
                items.push(Item::new(unit_r, Paint::Text { s: unit.clone(), font: F_HINT, color: MUTED, align: Align::Left }));
            }
            Ctl::Headroom { ram_path, ram, commit_path, commit, disk_path, disk } => {
                let num_w = c.s(56.0);
                let gb_w = c.measure("GB", F_HINT).0;
                let mut x = ctl.right() - gb_w;
                items.push(Item::new(Rect::new(x, ctl.y, gb_w, ctl.h), Paint::Text { s: "GB".into(), font: F_HINT, color: MUTED, align: Align::Left }));
                for (path, v, cap, off_zero) in [
                    (disk_path, disk, "Disk", true),
                    (commit_path, commit, "Commit", false),
                    (ram_path, ram, "RAM", false),
                ] {
                    x -= c.s(8.0) + num_w;
                    let off = off_zero && *v <= 0.0;
                    let text = if off { "Off".to_string() } else { format_number(*v) };
                    let editing = editing_path.as_deref() == Some(path.as_str());
                    items.push(
                        Item::new(Rect::new(x, ctl.y, num_w, ctl.h), Paint::Num(NumberField { text, muted: off, editing }))
                            .act(Act::Number(path.clone())),
                    );
                    let cw = c.measure(cap, F_HINT).0;
                    x -= c.s(8.0) + cw;
                    items.push(Item::new(Rect::new(x, ctl.y, cw, ctl.h), Paint::Text { s: cap.into(), font: F_HINT, color: MUTED, align: Align::Left }));
                    x -= c.s(6.0);
                }
            }
            Ctl::Chips { .. } | Ctl::GroupApps { .. } => {
                if let Some((chips, owner)) = self.chips_for(c, r, ctl.w) {
                    items.push(Item::new(ctl, Paint::Chips(chips)).act(Act::Chips(owner)));
                }
            }
            Ctl::Buttons(buttons) => {
                let mut x = ctl.right();
                let mut out = Vec::new();
                for (label, act, style) in buttons.iter().rev() {
                    let b = Button::new(label, *style);
                    let (bw, bh) = b.size(c);
                    x -= bw;
                    out.push(Item::new(Rect::new(x, ctl.cy() - bh / 2, bw, bh), Paint::Button(b)).act(Act::Button(*act)));
                    x -= c.s(10.0);
                }
                out.reverse();
                items.extend(out);
            }
            Ctl::None => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_group(
        &self,
        c: &Canvas,
        items: &mut Vec<Item>,
        block: usize,
        g: &GroupModel,
        open: bool,
        inner: &[bool],
        tiers_vis: bool,
        x0: i32,
        x1: i32,
        y: i32,
    ) -> i32 {
        let name = g.info.name.clone();
        let card_index = items.len();
        items.push(Item::new(Rect::default(), Paint::Card));
        let pad = c.s(16.0);
        let name_h = c.line_height(F_GROUP) + c.line_height(F_HINT);
        let head_h = c.s(28.0) + name_h.max(c.s(CTRL_H));
        let head = Rect::new(x0, y, x1 - x0, head_h);
        let cy = head.cy();
        let mut x = x0 + pad;
        items.push(Item::new(Rect::new(x, cy - c.s(8.0), c.s(14.0), c.s(16.0)), Paint::Chevron(open)));
        x += c.s(14.0) + c.s(14.0);
        let sub = if !g.info.enabled {
            "Turned off".to_string()
        } else {
            match &g.status {
                Some(s) => format!("{} processes", s.processes),
                None => "Not running".to_string(),
            }
        };
        let name_w = c.s(150.0);
        items.push(Item::new(
            Rect::new(x, cy - name_h / 2, name_w, name_h),
            Paint::GroupName { name: name.clone(), sub, layer: g.info.layer, enabled: g.info.enabled },
        ));
        x += name_w + c.s(14.0);
        // Right side: priority pill, readout, usage bar.
        let pill = Pill::new(label_of(&PRIORITIES, &g.priority));
        let pw = pill.width(c, 96.0);
        let pill_r = Rect::new(x1 - pad - pw, cy - c.s(CTRL_H) / 2, pw, c.s(CTRL_H));
        let read_w = c.s(92.0);
        let read_r = Rect::new(pill_r.x - c.s(14.0) - read_w, cy - name_h / 2, read_w, name_h);
        let (ram_mb, cap_mb) = g.status.as_ref().map(|s| (s.ram_mb, s.policy.trim_above_mb)).unwrap_or((0, 0));
        let bar_r = Rect::new(x, cy - c.s(4.0), (read_r.x - c.s(14.0) - x).max(0), c.s(8.0));
        items.push(Item::new(head, Paint::RowBg).act(Act::GroupHeader(name.clone())).row(RowKey::GroupHeader(name.clone())));
        if bar_r.w >= c.s(40.0) {
            items.push(Item::new(bar_r, Paint::Bar(usage_bar(ram_mb, cap_mb))).faded(!g.info.enabled));
        }
        let cap_text = if cap_mb == 0 { "no cap".to_string() } else { format!("cap {}", format_gb(cap_mb as f64 / 1024.0)) };
        items.push(Item::new(read_r, Paint::Read { value: format_gb(ram_mb as f64 / 1024.0), sub: cap_text }).faded(!g.info.enabled));
        items.push(Item::new(pill_r, Paint::Pill(pill)).act(Act::Priority(name.clone())).faded(g.priority_layer == Layer::Builtin && false));
        let mut yy = head.bottom();

        if open {
            items.push(Item::new(Rect::new(x0, yy, x1 - x0, c.px1(1.0)), Paint::Hairline(BORDER)));
            let bx0 = x0 + c.s(44.0);
            let bx1 = x1 - pad;
            // Apps row first, then the tiers, then the rest.
            let visible_rows: Vec<usize> = (0..g.rows.len()).filter(|i| inner.get(*i).copied().unwrap_or(true)).collect();
            let mut first = true;
            for &ri in visible_rows.iter().filter(|i| **i == 0) {
                yy += self.layout_row(c, items, &g.rows[ri], RowKey::Row { block, inner: Some(ri) }, bx0, bx1, yy, c.s(54.0), true);
                first = false;
            }
            if tiers_vis {
                yy = self.layout_tiers(c, items, g, bx0, bx1, yy);
                first = false;
            }
            let rest: Vec<usize> = visible_rows.iter().copied().filter(|i| *i != 0).collect();
            for (k, &ri) in rest.iter().enumerate() {
                let last = k + 1 == rest.len();
                yy += self.layout_row(c, items, &g.rows[ri], RowKey::Row { block, inner: Some(ri) }, bx0, bx1, yy, c.s(54.0), !last);
            }
            let _ = first;
            yy += c.s(8.0);
        }
        items[card_index].rect = Rect::new(x0, y, x1 - x0, yy - y);
        yy
    }

    fn layout_tiers(&self, c: &Canvas, items: &mut Vec<Item>, g: &GroupModel, x0: i32, x1: i32, y: i32) -> i32 {
        let gap = c.s(14.0);
        let c1 = c.s(170.0);
        let c3 = c.s(92.0);
        let c4 = c.s(110.0);
        let c2 = (x1 - x0 - c1 - c3 - c4 - 3 * gap).max(c.s(60.0));
        let cols = [x0, x0 + c1 + gap, x0 + c1 + gap + c2 + gap, x0 + c1 + gap + c2 + gap + c3 + gap];
        let mut yy = y + c.s(12.0);
        let hh = c.line_height(F_TAG);
        for (i, (t, align)) in [("WHEN", Align::Left), ("SOFT CAP (SHARE OF RAM)", Align::Left), ("LIMIT", Align::Right), ("SPEED", Align::Left)].iter().enumerate() {
            let w = [c1, c2, c3, c4][i];
            items.push(Item::new(Rect::new(cols[i], yy, w, hh), Paint::TierHead(t.to_string(), *align)));
        }
        yy += hh + c.s(8.0);
        let total = self.ram_total();
        let name = &g.info.name;
        for tm in &g.tiers {
            let (label, hint) = match tm.tier {
                Tier::Always => ("Always", Some("Unreal not running")),
                Tier::Unreal => ("Unreal is running", None),
                Tier::Tight => ("Memory is tight", None),
                Tier::Critical => ("Memory is critical", None),
            };
            let rh = c.s(38.0);
            let key = RowKey::Tier(name.clone(), tm.tier);
            items.push(Item::new(Rect::new(x0, yy - c.s(2.0), x1 - x0, rh + c.s(4.0)), Paint::RowBg).row(key.clone()));
            items.push(Item::new(Rect::new(cols[0], yy, c1, rh), Paint::TierLabel { label: label.into(), hint: hint.map(str::to_string) }).row(key.clone()));
            let inherited = tm.tier != Tier::Always && tm.own_pct.is_none();
            let pct = match &self.drag {
                Some(Drag::Slider { group, tier, pct, .. }) if group == name && *tier == tm.tier => *pct,
                _ => tm.pct,
            };
            let dragging = matches!(&self.drag, Some(Drag::Slider { group, tier, .. }) if group == name && *tier == tm.tier);
            let slider_r = Rect::new(cols[1], yy + (rh - c.s(24.0)) / 2, c2, c.s(24.0));
            items.push(Item::new(slider_r, Paint::Slider(Slider { frac: frac_from_pct(pct) })).act(Act::Slider(name.clone(), tm.tier)).faded(inherited && !dragging));
            let (value, sub) = if pct <= 0.0 {
                ("Off".to_string(), String::new())
            } else {
                (format_pct(pct), if total > 0.0 { format_gb(pct_to_gb(pct, total)) } else { String::new() })
            };
            items.push(Item::new(Rect::new(cols[2], yy, c3, rh), Paint::Read { value, sub }).faded(inherited && !dragging));
            let pill = Pill::new(label_of(&SPEEDS, &tm.cpu));
            let pill_r = Rect::new(cols[3], yy + (rh - c.s(32.0)) / 2, c4, c.s(32.0));
            let cpu_inherited = tm.tier != Tier::Always && tm.own_cpu.is_none();
            items.push(Item::new(pill_r, Paint::Pill(pill)).act(Act::Speed(name.clone(), tm.tier)).faded(cpu_inherited));
            yy += rh + c.s(8.0);
        }
        yy += c.s(4.0);
        items.push(Item::new(Rect::new(x0, yy - c.px1(1.0), x1 - x0, c.px1(1.0)), Paint::Hairline(HAIRLINE)));
        yy
    }

    // ------------------------------------------------------------ painting

    fn state_for(&self, it: &Item) -> WState {
        let over = self.confirm.is_none() && self.mouse.map(|m| it.rect.contains(m) && self.scroll.view.contains(m)).unwrap_or(false);
        let mut st = WState {
            hover: over,
            pressed: over && it.act.is_some() && self.pressed == it.act,
            focused: it.act.is_some() && self.focus == it.act,
            disabled: false,
            mouse: if over { self.mouse } else { None },
        };
        if let (Some(Act::Slider(g, t)), Some(Drag::Slider { group, tier, .. })) = (&it.act, &self.drag) {
            if g == group && t == tier {
                st.pressed = true;
            }
        }
        st
    }

    fn paint_item(&self, c: &mut Canvas, it: &Item, is_logo: bool) {
        let st = self.state_for(it);
        if it.faded {
            c.set_fade(Some((SURFACE, 0.5)));
        }
        let r = it.rect;
        match &it.paint {
            Paint::RowBg => {
                if st.hover {
                    let g = c.s(12.0);
                    c.fill_round_rect(r.inset(-g, 0), c.s(RADIUS), SELECTED);
                }
            }
            Paint::Section(s) => c.spaced_text(r, s, F_SECTION, ACCENT, c.s(3.1), Align::Left),
            Paint::Label { label, hint, danger, layer } => {
                let lh = c.line_height(F_LABEL);
                let (lw, _) = c.measure(label, F_LABEL);
                let marker_w = match layer {
                    Layer::User => c.s(14.0),
                    Layer::Team => c.s(56.0),
                    Layer::Builtin => 0,
                };
                let label_w = lw.min(r.w - marker_w);
                c.text(Rect::new(r.x, r.y, label_w, lh), label, F_LABEL, if *danger { ERROR } else { TEXT }, Align::Left);
                let mx = r.x + label_w + c.s(8.0);
                match layer {
                    Layer::User => {
                        layer_dot(c, mx, r.y + lh / 2 + c.s(1.0));
                    }
                    Layer::Team => {
                        team_tag(c, mx, r.y + lh / 2);
                    }
                    Layer::Builtin => {}
                }
                let hh = c.line_height(F_HINT);
                for (i, line) in hint.iter().enumerate() {
                    c.text_top(Rect::new(r.x, r.y + lh + i as i32 * hh, r.w, hh), line, F_HINT, MUTED, Align::Left);
                }
            }
            Paint::Toggle(t) => t.paint(c, r, &st),
            Paint::Seg(s) => s.paint(c, r, &st),
            Paint::Pill(p) => p.paint(c, r, &st),
            Paint::Num(n) => n.paint(c, r, &st),
            Paint::Slider(s) => s.paint(c, r, &st),
            Paint::Chips(ch) => ch.paint(c, r, &st),
            Paint::Button(b) => b.paint(c, r, &st),
            Paint::Hotkey(h) => h.paint(c, r, &st),
            Paint::Bar(b) => b.paint(c, r, &st),
            Paint::Text { s, font, color, align } => {
                if is_logo {
                    c.logo(r);
                } else {
                    c.text(r, s, *font, *color, *align);
                }
            }
            Paint::Hairline(col) => c.fill_rect(r, *col),
            Paint::Card => c.fill_round_rect_bordered(r, c.s(CARD_RADIUS), SURFACE, BORDER),
            Paint::Chevron(open) => {
                let g = if *open { glyph::CHEVRON_DOWN } else { glyph::CHEVRON_RIGHT };
                c.icon(r, g, Font::new(Face::Icon, 10.0, REGULAR), MUTED);
            }
            Paint::GroupName { name, sub, layer, enabled } => {
                let nh = c.line_height(F_GROUP);
                let (nw, _) = c.measure(name, F_GROUP);
                let marker = match layer {
                    Layer::Team => c.s(56.0),
                    Layer::User => c.s(14.0),
                    Layer::Builtin => 0,
                };
                let w = nw.min(r.w - marker);
                c.text(Rect::new(r.x, r.y, w, nh), name, F_GROUP, if *enabled { TEXT } else { MUTED }, Align::Left);
                match layer {
                    Layer::Team => {
                        team_tag(c, r.x + w + c.s(8.0), r.y + nh / 2);
                    }
                    Layer::User => {
                        layer_dot(c, r.x + w + c.s(8.0), r.y + nh / 2 + c.s(1.0));
                    }
                    Layer::Builtin => {}
                }
                c.text_top(Rect::new(r.x, r.y + nh, r.w, c.line_height(F_HINT)), sub, F_HINT, MUTED, Align::Left);
            }
            Paint::Read { value, sub } => {
                let vh = c.line_height(F_BODY);
                let sh = c.line_height(F_READ_SMALL);
                let top = r.y + (r.h - vh - sh) / 2;
                c.text_top(Rect::new(r.x, top, r.w, vh), value, F_BODY, TEXT, Align::Right);
                c.text_top(Rect::new(r.x, top + vh, r.w, sh), sub, F_READ_SMALL, MUTED, Align::Right);
            }
            Paint::TierLabel { label, hint } => {
                let lh = c.line_height(F_LABEL);
                let hh = if hint.is_some() { c.line_height(F_HINT) } else { 0 };
                let top = r.y + (r.h - lh - hh) / 2;
                c.text_top(Rect::new(r.x, top, r.w, lh), label, F_LABEL, TEXT, Align::Left);
                if let Some(h) = hint {
                    c.text_top(Rect::new(r.x, top + lh, r.w, hh), h, F_HINT, MUTED, Align::Left);
                }
            }
            Paint::TierHead(s, align) => c.spaced_text(r, s, F_TAG, MUTED, c.s(2.0), *align),
            Paint::Caption(s) => c.spaced_text(r, s, F_CAPTION, MUTED, c.s(1.3), Align::Right),
        }
        if it.faded {
            c.set_fade(None);
        }
    }

    fn paint_search(&self, c: &mut Canvas) {
        let w = c.width();
        let th = c.s(60.0);
        c.fill_rect(Rect::new(0, 0, w, th), SURFACE);
        c.hline(0, th, w, BORDER);
        let x = c.s(24.0);
        c.icon(Rect::new(x, 0, c.s(18.0), th), glyph::SEARCH, Font::new(Face::Icon, 15.0, REGULAR), MUTED);
        let tx = x + c.s(18.0) + c.s(12.0);
        let caret_on = self.caret_on && self.caret_visible();
        let caret_h = c.s(20.0);
        let cw = c.px1(2.0);
        if self.query.is_empty() {
            if caret_on {
                c.fill_rect(Rect::new(tx, th / 2 - caret_h / 2, cw, caret_h), ACCENT);
            }
            c.text(Rect::new(tx + cw + c.s(10.0), 0, w - tx - c.s(24.0), th), "Search settings…", F_SEARCH, MUTED, Align::Left);
        } else {
            c.text(Rect::new(tx, 0, w - tx - c.s(24.0), th), &self.query, F_SEARCH, TEXT, Align::Left);
            let qw = c.measure(&self.query, F_SEARCH).0;
            if caret_on {
                c.fill_rect(Rect::new(tx + qw + c.s(2.0), th / 2 - caret_h / 2, cw, caret_h), ACCENT);
            }
        }
    }

    fn paint_footer(&self, c: &mut Canvas) {
        let fh = c.s(44.0);
        let y = c.height() - fh;
        let w = c.width();
        c.fill_rect(Rect::new(0, y, w, fh), SURFACE);
        c.hline(0, y, w, BORDER);
        let r = Rect::new(c.s(32.0), y, w - c.s(64.0), fh);
        c.text(r, "Changes are saved automatically", F_HINT, MUTED, Align::Left);
        c.text(r, "Right-click a setting to reset it", F_HINT, MUTED, Align::Right);
    }

    fn dialog(&self) -> Option<Dialog> {
        let (title, body, ok) = match self.confirm.as_ref()? {
            Confirm::HardCap { path, mb } => {
                let group = path.split('.').nth(1).unwrap_or("this group").to_string();
                (
                    "Set a hard cap?".to_string(),
                    format!("If {} reaches {} MB its apps will fail to get more memory and will probably crash.", group, format_number(*mb)),
                    "Set hard cap",
                )
            }
            Confirm::ResetAll => (
                "Reset all settings?".into(),
                "Every setting goes back to its default and groups you added are removed. A backup of your settings is kept.".into(),
                "Reset all settings",
            ),
            Confirm::RemoveGroup(name) => (
                format!("Remove {}?", name),
                format!("The apps in {} go back to normal and the group's settings are deleted.", name),
                "Remove group",
            ),
        };
        Some(Dialog { title, body, ok: Button::new(ok, ButtonStyle::Danger), cancel: Button::new("Cancel", ButtonStyle::Normal) })
    }

    fn dialog_area(&self, c: &Canvas) -> Rect {
        c.bounds()
    }

    // ------------------------------------------------------------ actions

    fn write(&mut self, f: impl FnOnce(&mut dyn SettingsStore) -> Result<(), String>) -> bool {
        let r = f(self.host.store.as_mut());
        if let Err(e) = &r {
            toast::show_always(toast::catalog::save_failed(e));
        }
        self.reload();
        r.is_ok()
    }

    fn item_with(&self, act: &Act) -> Option<&Item> {
        self.items.iter().find(|i| i.act.as_ref() == Some(act))
    }

    fn activate(&mut self, act: Act, p: Option<Pt>, host: &mut Host) {
        match act.clone() {
            Act::Toggle(path) => {
                let on = matches!(self.ctl_for_path(&path), Some((_, Ctl::Toggle { on: true, .. })));
                self.write(|s| s.set(&path, Value::Bool(!on)));
            }
            Act::Startup => {
                let on = self.host.store.run_at_startup();
                self.write(|s| s.set_run_at_startup(!on));
            }
            Act::Seg(path) => {
                let Some((_, Ctl::Segmented { options, selected, .. })) = self.ctl_for_path(&path) else { return };
                let options = options.clone();
                let idx = match (p, self.item_with(&act)) {
                    (Some(p), Some(it)) => match &it.paint {
                        Paint::Seg(seg) => {
                            let probe = Canvas::probe(self.scale);
                            seg.hit(&probe, it.rect, p).unwrap_or(*selected)
                        }
                        _ => *selected,
                    },
                    _ => (*selected + 1) % options.len(),
                };
                let v = options[idx].0;
                self.write(|s| s.set(&path, Value::Text(v.into())));
            }
            Act::Hotkey(path) => {
                if self.recording.as_deref() == Some(path.as_str()) {
                    self.recording = None;
                } else {
                    self.recording = Some(path);
                    self.rec_partial.clear();
                }
            }
            Act::Number(path) => {
                let Some(it) = self.item_with(&act) else { return };
                let rect = it.rect;
                let text = match self.ctl_for_path(&path) {
                    Some((_, Ctl::Number { value, off_at_zero, .. })) => {
                        if *off_at_zero && *value <= 0.0 {
                            String::new()
                        } else {
                            format_number(*value)
                        }
                    }
                    Some((_, Ctl::Headroom { ram_path, ram, commit_path, commit, disk_path, disk })) => {
                        if *disk_path == path {
                            if *disk <= 0.0 {
                                String::new()
                            } else {
                                format_number(*disk)
                            }
                        } else if *ram_path == path {
                            format_number(*ram)
                        } else if *commit_path == path {
                            format_number(*commit)
                        } else {
                            String::new()
                        }
                    }
                    _ => String::new(),
                };
                let probe = Canvas::probe(self.scale);
                self.editing = Some(EditTarget::Number(path));
                host.edit(0, NumberField::edit_rect(&probe, rect), &text, true);
            }
            Act::Chips(owner) => {
                let Some(it) = self.item_with(&act) else { return };
                let (rect, chips) = match &it.paint {
                    Paint::Chips(ch) => (it.rect, ch.clone()),
                    _ => return,
                };
                let probe = Canvas::probe(self.scale);
                let hit = match p {
                    Some(p) => chips.hit(&probe, rect, p),
                    None => Some(ChipHit::Add),
                };
                let add_rect = chips.layout(&probe, rect.w).into_iter().find(|(_, k)| *k == ChipKind::Add).map(|(r, _)| r.offset(rect.x, rect.y)).unwrap_or(rect);
                match (hit, owner) {
                    (Some(ChipHit::Remove(i)), ChipOwner::List(path)) => {
                        let mut items = self.host.store.get(&path).and_then(|s| s.value.as_list().map(|l| l.to_vec())).unwrap_or_default();
                        if i < items.len() {
                            items.remove(i);
                            self.write(|s| s.set(&path, Value::List(items)));
                        }
                    }
                    (Some(ChipHit::Remove(i)), ChipOwner::Group(g)) => {
                        if let Some(name) = self.group(&g).and_then(|gm| gm.info.processes.get(i)).cloned() {
                            self.write(|s| s.remove_process(&g, &name));
                        }
                    }
                    (Some(ChipHit::More), owner) => {
                        self.expanded_chips.insert(owner);
                    }
                    (Some(ChipHit::Add), ChipOwner::List(path)) => {
                        self.editing = Some(EditTarget::AddChip(path));
                        let w = c_max(add_rect.w, probe.s(180.0));
                        let r = Rect::new(add_rect.right() - w, add_rect.y, w, add_rect.h);
                        host.edit(0, r.inset(probe.s(6.0), probe.s(2.0)), "", false);
                    }
                    (Some(ChipHit::Add), ChipOwner::Group(g)) => self.add_app_menu(g, false, Pt::new(add_rect.x, add_rect.bottom()), host),
                    _ => {}
                }
            }
            Act::Button(b) => self.button(b, host),
            Act::GroupHeader(name) => {
                self.open_group = if self.open_group.as_deref() == Some(name.as_str()) { None } else { Some(name) };
            }
            Act::Priority(name) => {
                let Some(it) = self.item_with(&act) else { return };
                let at = Pt::new(it.rect.x, it.rect.bottom());
                let cur = self.group(&name).map(|g| g.priority.clone()).unwrap_or_default();
                let items = PRIORITIES.iter().enumerate().map(|(i, (v, l))| MenuItem::new(i as u32 + 1, l).checked(*v == cur)).collect();
                self.pending = Some(Pending::Priority(name));
                host.menu(MENU_PICK, items, at);
            }
            Act::Speed(name, tier) => {
                let Some(it) = self.item_with(&act) else { return };
                let at = Pt::new(it.rect.x, it.rect.bottom());
                let cur = self.group(&name).and_then(|g| g.tiers.iter().find(|t| t.tier == tier)).map(|t| t.cpu.clone()).unwrap_or_default();
                let items = SPEEDS.iter().enumerate().map(|(i, (v, l))| MenuItem::new(i as u32 + 1, l).checked(*v == cur)).collect();
                self.pending = Some(Pending::Speed(name, tier));
                host.menu(MENU_PICK, items, at);
            }
            Act::Slider(..) => {}
        }
        host.invalidate();
    }

    fn add_app_menu(&mut self, group: String, create: bool, at: Pt, host: &mut Host) {
        let mut apps = self.host.store.running_apps();
        apps.truncate(30);
        let mut items: Vec<MenuItem> = apps
            .iter()
            .enumerate()
            .map(|(i, a)| MenuItem::new(i as u32 + 1, &format!("{} · {} processes · {}", a.name, a.processes, format_gb(a.ram_gb))))
            .collect();
        if !items.is_empty() {
            items.push(MenuItem::separator());
        }
        items.push(MenuItem::new(MENU_ID_TYPE, "Type a name…"));
        self.pending = Some(Pending::AddApp { group, create, apps });
        host.menu(MENU_PICK, items, at);
    }

    fn button(&mut self, b: ButtonAct, host: &mut Host) {
        let filters: &[(&str, &str)] = &[("Settings (*.toml)", "*.toml"), ("All files", "*.*")];
        match b {
            ButtonAct::AddGroup => {
                if let Some(it) = self.item_with(&Act::Button(ButtonAct::AddGroup)) {
                    let probe = Canvas::probe(self.scale);
                    let r = Rect::new(it.rect.right() - probe.s(240.0), it.rect.y, probe.s(240.0), it.rect.h);
                    self.editing = Some(EditTarget::AddGroup);
                    host.edit(0, NumberField::edit_rect(&probe, r), "New group", false);
                }
            }
            ButtonAct::OpenHistory => (self.host.commands)(Command::OpenHistory),
            ButtonAct::CheckNow => (self.host.commands)(Command::CheckUpdates),
            ButtonAct::UpdateNow => (self.host.commands)(Command::InstallUpdate),
            ButtonAct::CopySetup => (self.host.commands)(Command::CopyAgentSetup),
            ButtonAct::OpenGuide => host.shell_open(GUIDE_URL),
            ButtonAct::LoadTeam => host.open_file(FILE_TEAM, "Load team defaults", filters),
            ButtonAct::RemoveTeam => {
                self.write(|s| s.remove_team_file());
            }
            ButtonAct::Export => host.save_file(FILE_EXPORT, "Export settings", "AtherMemGuard-settings.toml", filters),
            ButtonAct::Import => host.open_file(FILE_IMPORT, "Import settings", filters),
            ButtonAct::OpenFile => {
                let f = self.host.store.settings_file();
                host.shell_open(&f.to_string_lossy());
            }
            ButtonAct::ResetAll => {
                self.confirm = Some(Confirm::ResetAll);
                self.dialog_rects = None;
            }
        }
        host.invalidate();
    }

    fn context_menu(&mut self, key: RowKey, at: Pt, host: &mut Host) {
        let items = match &key {
            RowKey::Row { .. } => {
                let Some(r) = self.row(&key) else { return };
                if r.resets.is_empty() {
                    return;
                }
                let can = r.layer == Layer::User;
                let label = match &r.fallback {
                    Some((v, Layer::Team)) => format!("Reset to team default ({})", v),
                    Some((v, _)) => format!("Reset to default ({})", v),
                    None => "Reset to default".to_string(),
                };
                vec![MenuItem::new(MENU_ID_RESET, &label).disabled(!can)]
            }
            RowKey::Tier(g, t) => {
                let Some(gm) = self.group(g) else { return };
                let Some(tm) = gm.tiers.iter().find(|x| x.tier == *t) else { return };
                let on_speed = self.items.iter().any(|i| i.act == Some(Act::Speed(g.clone(), *t)) && i.rect.contains(at));
                let path = group_path(g, *t, if on_speed { "cpu" } else { "trim_above_pct" });
                let setting = self.host.store.get(&path);
                if *t == Tier::Always {
                    let user = setting.as_ref().map(|s| s.layer == Layer::User).unwrap_or(false);
                    let fb = setting.as_ref().and_then(|s| s.fallback.clone());
                    let label = match fb {
                        Some((v, l)) => {
                            let v = match &v {
                                Value::Float(f) => format_pct(*f),
                                Value::Text(s) => label_of(&SPEEDS, s).to_string(),
                                other => format!("{:?}", other),
                            };
                            if l == Layer::Team {
                                format!("Reset to team default ({})", v)
                            } else {
                                format!("Reset to default ({})", v)
                            }
                        }
                        None => "Reset to default".into(),
                    };
                    vec![MenuItem::new(MENU_ID_RESET, &label).disabled(!user)]
                } else {
                    let own = if on_speed { tm.own_cpu.is_some() } else { tm.own_pct.is_some() };
                    let user = setting.as_ref().map(|s| s.layer == Layer::User).unwrap_or(false);
                    vec![MenuItem::new(MENU_ID_ABOVE, "Use the value above").disabled(!(own && user))]
                }
            }
            RowKey::GroupHeader(g) => {
                let Some(gm) = self.group(g) else { return };
                vec![
                    MenuItem::new(MENU_ID_TURN, if gm.info.enabled { "Turn off this group" } else { "Turn on" }),
                    MenuItem::new(MENU_ID_RENAME, "Rename…").disabled(gm.info.builtin),
                    MenuItem::new(MENU_ID_REMOVE, "Remove group").disabled(gm.info.builtin),
                ]
            }
        };
        let key = match key {
            RowKey::Tier(g, t) => {
                // Remember which part was clicked through the path in Pending.
                let on_speed = self.items.iter().any(|i| i.act == Some(Act::Speed(g.clone(), t)) && i.rect.contains(at));
                self.pending = Some(Pending::Reset(RowKey::Tier(g.clone(), t)));
                self.tier_speed_click = on_speed;
                host.menu(MENU_RESET, items, at);
                return;
            }
            RowKey::GroupHeader(g) => {
                self.pending = Some(Pending::GroupMenu(g));
                host.menu(MENU_RESET, items, at);
                return;
            }
            k => k,
        };
        self.pending = Some(Pending::Reset(key));
        host.menu(MENU_RESET, items, at);
    }

    fn on_menu(&mut self, choice: Option<u32>, host: &mut Host) {
        let Some(pending) = self.pending.take() else { return };
        let Some(id) = choice else { return };
        match pending {
            Pending::Reset(key) => match key {
                RowKey::Row { .. } => {
                    let paths = self.row(&key).map(|r| r.resets.clone()).unwrap_or_default();
                    self.write(|s| paths.iter().try_for_each(|p| s.reset(p)));
                }
                RowKey::Tier(g, t) => {
                    let path = group_path(&g, t, if self.tier_speed_click { "cpu" } else { "trim_above_pct" });
                    self.write(|s| s.reset(&path));
                }
                RowKey::GroupHeader(_) => {}
            },
            Pending::GroupMenu(g) => match id {
                MENU_ID_TURN => {
                    let on = self.group(&g).map(|m| m.info.enabled).unwrap_or(true);
                    self.write(|s| s.set_group_enabled(&g, !on));
                }
                MENU_ID_RENAME => {
                    let rect = self.items.iter().find(|i| matches!(&i.paint, Paint::GroupName { name, .. } if *name == g)).map(|i| i.rect);
                    if let Some(r) = rect {
                        let probe = Canvas::probe(self.scale);
                        self.editing = Some(EditTarget::Rename(g.clone()));
                        let er = Rect::new(r.x - probe.s(4.0), r.y, probe.s(200.0), probe.line_height(F_GROUP) + probe.s(4.0));
                        host.edit(0, er, &g, false);
                    }
                }
                MENU_ID_REMOVE => {
                    self.confirm = Some(Confirm::RemoveGroup(g));
                    self.dialog_rects = None;
                }
                _ => {}
            },
            Pending::Priority(g) => {
                if let Some((v, _)) = PRIORITIES.get(id as usize - 1) {
                    let path = group_path(&g, Tier::Always, "priority");
                    self.write(|s| s.set(&path, Value::Text(v.to_string())));
                }
            }
            Pending::Speed(g, t) => {
                if let Some((v, _)) = SPEEDS.get(id as usize - 1) {
                    let path = group_path(&g, t, "cpu");
                    self.write(|s| s.set(&path, Value::Text(v.to_string())));
                }
            }
            Pending::AddApp { group, create, apps } => {
                if id == MENU_ID_TYPE {
                    let rect = if create {
                        self.item_with(&Act::Button(ButtonAct::AddGroup)).map(|i| i.rect)
                    } else {
                        self.items.iter().find(|i| i.act == Some(Act::Chips(ChipOwner::Group(group.clone())))).map(|i| i.rect)
                    };
                    if let Some(r) = rect {
                        let probe = Canvas::probe(self.scale);
                        let w = probe.s(200.0).min(r.w);
                        let ch = Chips::chip_h(&probe);
                        let er = if create {
                            Rect::new(r.right() - w, r.y, w, r.h).inset(probe.s(4.0), probe.s(2.0))
                        } else {
                            Rect::new(r.right() - w, r.bottom() - ch, w, ch).inset(probe.s(4.0), probe.s(2.0))
                        };
                        self.editing = Some(EditTarget::AddApp { group, create });
                        host.edit(0, er, "", false);
                    }
                } else if let Some(app) = id.checked_sub(1).and_then(|i| apps.get(i as usize)).cloned() {
                    if create {
                        if self.write(|s| s.add_group(&group, std::slice::from_ref(&app.name))) {
                            self.open_group = Some(group);
                        }
                    } else {
                        self.write(|s| s.add_process(&group, &app.name));
                    }
                }
            }
        }
        host.invalidate();
    }

    fn on_edit_done(&mut self, commit: bool, text: String, host: &mut Host) {
        let Some(target) = self.editing.take() else { return };
        host.invalidate();
        if !commit {
            return;
        }
        match target {
            EditTarget::Number(path) => {
                let Some((row, ctl)) = self.ctl_for_path(&path) else { return };
                let label = row.label.clone();
                let (min, max, integer, off_at_zero, old) = match ctl {
                    Ctl::Number { min, max, integer, off_at_zero, value, .. } => (*min, *max, *integer, *off_at_zero, *value),
                    Ctl::Headroom { ram_path, ram, commit_path, commit, disk_path, disk } => {
                        if *disk_path == path {
                            (0.0, 100_000.0, false, true, *disk)
                        } else if *ram_path == path {
                            (0.0, 1024.0, false, false, *ram)
                        } else if *commit_path == path {
                            (0.0, 1024.0, false, false, *commit)
                        } else {
                            return;
                        }
                    }
                    _ => return,
                };
                let t = text.trim().replace(',', ".");
                let parsed = if off_at_zero && (t.is_empty() || t.eq_ignore_ascii_case("off")) { Some(0.0) } else { t.parse::<f64>().ok() };
                let Some(v) = parsed.filter(|v| v.is_finite()) else {
                    toast::show_always(toast::catalog::save_failed(&format!(
                        "{} needs a number between {} and {}.",
                        label,
                        format_number(min),
                        format_number(max)
                    )));
                    return;
                };
                let v = v.clamp(min, max);
                let v = if integer { v.round() } else { (v * 100.0).round() / 100.0 };
                if (v - old).abs() < 1e-9 {
                    return;
                }
                let value = if integer { Value::Int(v as i64) } else { Value::Float(v) };
                if path.ends_with(".hard_cap_mb") && off_at_zero && old <= 0.0 && v > 0.0 {
                    self.confirm = Some(Confirm::HardCap { path, mb: v });
                    self.dialog_rects = None;
                    return;
                }
                self.write(|s| s.set(&path, value));
            }
            EditTarget::AddChip(path) => {
                let name = if chip_strip_exe(&path) {
                    strip_exe(&text)
                } else {
                    text.trim().trim_matches('"').trim().to_string()
                };
                if name.is_empty() {
                    return;
                }
                let mut items = self.host.store.get(&path).and_then(|s| s.value.as_list().map(|l| l.to_vec())).unwrap_or_default();
                if !items.iter().any(|x| x.eq_ignore_ascii_case(&name)) {
                    items.push(name);
                }
                self.write(|s| s.set(&path, Value::List(items)));
            }
            EditTarget::AddApp { group, create } => {
                let proc_name = strip_exe(&text);
                if proc_name.is_empty() {
                    return;
                }
                if create {
                    if self.write(|s| s.add_group(&group, std::slice::from_ref(&proc_name))) {
                        self.open_group = Some(group);
                    }
                } else {
                    self.write(|s| s.add_process(&group, &proc_name));
                }
            }
            EditTarget::AddGroup => {
                let name = text.trim().to_string();
                if name.is_empty() {
                    return;
                }
                if name.contains('.') {
                    toast::show_always(toast::catalog::save_failed("A group needs a name without dots."));
                    return;
                }
                if self.host.store.groups().iter().any(|g| g.name.eq_ignore_ascii_case(&name)) {
                    toast::show_always(toast::catalog::save_failed(&format!("There is already a group called {}.", name)));
                    return;
                }
                let at = self
                    .item_with(&Act::Button(ButtonAct::AddGroup))
                    .map(|it| Pt::new(it.rect.x, it.rect.bottom()))
                    .unwrap_or_default();
                self.add_app_menu(name, true, at, host);
            }
            EditTarget::Rename(old) => {
                let new_name = text.trim().to_string();
                if !new_name.is_empty() && new_name != old && self.write(|s| s.rename_group(&old, &new_name)) {
                    if self.open_group.as_deref() == Some(old.as_str()) {
                        self.open_group = Some(new_name.clone());
                    }
                    if self.expanded_chips.remove(&ChipOwner::Group(old)) {
                        self.expanded_chips.insert(ChipOwner::Group(new_name));
                    }
                }
            }
        }
    }

    fn on_file(&mut self, tag: u32, path: Option<PathBuf>) {
        let Some(path) = path else { return };
        match tag {
            FILE_EXPORT => {
                self.write(|s| s.export_to(&path));
            }
            FILE_IMPORT => {
                self.write(|s| s.import_from(&path));
            }
            FILE_TEAM => {
                self.write(|s| s.load_team_file(&path));
            }
            _ => {}
        }
    }

    fn confirm_ok(&mut self) {
        self.dialog_rects = None;
        match self.confirm.take() {
            Some(Confirm::HardCap { path, mb }) => {
                self.write(|s| s.set(&path, Value::Int(mb as i64)));
            }
            Some(Confirm::ResetAll) => {
                self.write(|s| s.reset_all());
            }
            Some(Confirm::RemoveGroup(g)) => {
                self.write(|s| s.remove_group(&g));
            }
            None => {}
        }
    }

    // ------------------------------------------------------------ keyboard

    fn focusables(&self) -> Vec<Act> {
        let mut out: Vec<Act> = Vec::new();
        for it in &self.items {
            if let Some(a) = &it.act {
                if !out.contains(a) {
                    out.push(a.clone());
                }
            }
        }
        out
    }

    fn move_focus(&mut self, back: bool) {
        let list = self.focusables();
        if list.is_empty() {
            return;
        }
        let idx = self.focus.as_ref().and_then(|f| list.iter().position(|a| a == f));
        let next = match (idx, back) {
            (None, false) => 0,
            (None, true) => list.len() - 1,
            (Some(i), false) => (i + 1) % list.len(),
            (Some(i), true) => (i + list.len() - 1) % list.len(),
        };
        self.focus = Some(list[next].clone());
        if let Some(r) = self.item_with(&list[next]).map(|i| i.rect) {
            let probe_margin = (self.scale.0 * 24.0) as i32;
            self.scroll.reveal(r, probe_margin);
        }
    }

    fn slider_step(&mut self, group: String, tier: Tier, delta: f64) {
        let cur = self.group(&group).and_then(|g| g.tiers.iter().find(|t| t.tier == tier)).map(|t| t.pct).unwrap_or(0.0);
        let v = snap_pct(cur + delta);
        let path = group_path(&group, tier, "trim_above_pct");
        self.write(|s| s.set(&path, Value::Float(v)));
    }

    fn record_key(&mut self, vk: u16, m: Mods, host: &mut Host) -> bool {
        let Some(path) = self.recording.clone() else { return false };
        let win = unsafe { GetKeyState(VK_LWIN as i32) < 0 || GetKeyState(VK_RWIN as i32) < 0 };
        if is_modifier(vk) {
            self.rec_partial = modifier_text(m, win);
            host.invalidate();
            return true;
        }
        let none = !m.ctrl && !m.alt && !m.shift && !win;
        if none && vk == VK_ESCAPE {
            self.recording = None;
        } else if none && (vk == VK_BACK || vk == VK_DELETE) {
            self.recording = None;
            self.write(|s| s.set(&path, Value::Text(String::new())));
        } else if let Some(text) = hotkey_text(m, win, vk) {
            self.recording = None;
            self.write(|s| s.set(&path, Value::Text(text)));
        }
        host.invalidate();
        true
    }

    fn hit(&self, p: Pt) -> Option<&Item> {
        if !self.scroll.view.contains(p) {
            return None;
        }
        // Topmost first: later items paint over earlier ones.
        self.items.iter().rev().find(|i| i.act.is_some() && i.rect.contains(p))
    }

    fn row_at(&self, p: Pt) -> Option<RowKey> {
        if !self.scroll.view.contains(p) {
            return None;
        }
        self.items.iter().rev().find(|i| i.row.is_some() && matches!(i.paint, Paint::RowBg) && i.rect.contains(p)).and_then(|i| i.row.clone())
    }
}

fn c_max(a: i32, b: i32) -> i32 {
    a.max(b)
}

fn compact_chip_label(c: &Canvas, s: &str, max_w: i32) -> String {
    let avail = (max_w - c.s(20.0) - 2 * c.px1(1.0)).max(c.s(40.0));
    if c.measure(s, F_CHIP).0 <= avail {
        return s.to_string();
    }
    if let Some(last_sep) = s.rfind(['\\', '/']) {
        let tail = &s[last_sep..];
        let prefix = s.get(..3).filter(|p| p.ends_with(['\\', '/'])).unwrap_or("");
        let candidate = format!("{}…{}", prefix, tail);
        if c.measure(&candidate, F_CHIP).0 <= avail {
            return candidate;
        }
    }
    c.ellipsize(s, F_CHIP, avail)
}

fn scroll_thumb(scroll: &Scroll, scale: Scale) -> Option<Rect> {
    if scroll.content <= scroll.view.h || scroll.view.h <= 0 {
        return None;
    }
    let w = scale.s(4.0).max(3);
    let track = scroll.view.inset(0, scale.s(4.0));
    let h = ((track.h as i64 * scroll.view.h as i64 / scroll.content as i64) as i32).max(scale.s(32.0));
    let y = track.y + ((track.h - h) as i64 * scroll.y as i64 / scroll.max().max(1) as i64) as i32;
    Some(Rect::new(scroll.view.right() - w - scale.s(3.0), y, w, h))
}

fn scroll_y_for_thumb(scroll: &Scroll, scale: Scale, thumb_top: i32) -> i32 {
    let Some(t) = scroll_thumb(scroll, scale) else { return 0 };
    let track = scroll.view.inset(0, scale.s(4.0));
    let span = (track.h - t.h).max(1);
    ((thumb_top - track.y).clamp(0, span) as i64 * scroll.max() as i64 / span as i64) as i32
}

fn slider_frac_at(scale: Scale, r: Rect, x: i32) -> f32 {
    let k = scale.s(8.0);
    let (x0, x1) = (r.x + k, r.right() - k);
    if x1 <= x0 {
        0.0
    } else {
        ((x - x0) as f32 / (x1 - x0) as f32).clamp(0.0, 1.0)
    }
}

fn clip_rect(a: Rect, b: Rect) -> Rect {
    Rect::ltrb(a.x.max(b.x), a.y.max(b.y), a.right().min(b.right()), a.bottom().min(b.bottom()))
}

fn is_modifier(vk: u16) -> bool {
    matches!(vk, VK_CONTROL | VK_MENU | VK_SHIFT | VK_LWIN | VK_RWIN | VK_LCONTROL | VK_RCONTROL | VK_LMENU | VK_RMENU | VK_LSHIFT | VK_RSHIFT)
}

fn modifier_text(m: Mods, win: bool) -> String {
    let mut parts = Vec::new();
    if m.ctrl {
        parts.push("Ctrl");
    }
    if m.alt {
        parts.push("Alt");
    }
    if m.shift {
        parts.push("Shift");
    }
    if win {
        parts.push("Win");
    }
    parts.join(" + ")
}

/// The name of a key as it goes into `general.prime_hotkey`.
pub fn key_name(vk: u16) -> Option<String> {
    Some(match vk {
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        VK_SPACE => "Space".into(),
        VK_HOME => "Home".into(),
        VK_END => "End".into(),
        VK_INSERT => "Insert".into(),
        VK_PRIOR => "PageUp".into(),
        VK_NEXT => "PageDown".into(),
        VK_UP => "Up".into(),
        VK_DOWN => "Down".into(),
        VK_LEFT => "Left".into(),
        VK_RIGHT => "Right".into(),
        VK_PAUSE => "Pause".into(),
        _ => return None,
    })
}

/// "Ctrl+Alt+M" for a key press, or `None` when it isn't a usable shortcut (a plain letter
/// would stop that key from typing; F-keys may stand alone).
pub fn hotkey_text(m: Mods, win: bool, vk: u16) -> Option<String> {
    let key = key_name(vk)?;
    let fkey = (0x70..=0x87).contains(&vk);
    if !(m.ctrl || m.alt || win) && !fkey {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if m.ctrl {
        parts.push("Ctrl".into());
    }
    if m.alt {
        parts.push("Alt".into());
    }
    if m.shift {
        parts.push("Shift".into());
    }
    if win {
        parts.push("Win".into());
    }
    parts.push(key);
    Some(parts.join("+"))
}

impl SettingsWindow {
    /// The search bar across the top, where the caret blinks (device pixels).
    fn search_bar(&self) -> Rect {
        Rect::new(0, 0, self.size.0, self.scale.s(60.0) + self.scale.px1(1.0))
    }

    fn caret_active(&self) -> bool {
        self.focused && !self.minimized
    }

    fn sync_caret_timer(&mut self, host: &mut Host) {
        if self.caret_active() {
            host.set_timer(TIMER_CARET, 530);
        } else {
            host.kill_timer(TIMER_CARET);
        }
    }

    /// Can the search caret be on screen now?
    fn caret_visible(&self) -> bool {
        self.caret_active() && self.editing.is_none() && self.recording.is_none() && self.confirm.is_none()
    }

    fn dialog_buttons(&mut self) -> Option<(Rect, Rect)> {
        if self.confirm.is_none() {
            self.dialog_rects = None;
            return None;
        }
        if let Some(rects) = self.dialog_rects {
            return Some(rects);
        }
        let d = self.dialog()?;
        let probe = Canvas::probe(self.scale);
        let area = Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1));
        let (_, ok, cancel) = d.layout(&probe, area);
        self.dialog_rects = Some((ok, cancel));
        Some((ok, cancel))
    }

    fn compute_hover(&mut self, p: Option<Pt>) -> SettingsHover {
        let Some(p) = p else {
            return SettingsHover::default();
        };
        if self.confirm.is_some() {
            let dialog = self.dialog_buttons().and_then(|(ok, cancel)| {
                if ok.contains(p) {
                    Some(true)
                } else if cancel.contains(p) {
                    Some(false)
                } else {
                    None
                }
            });
            return SettingsHover { act: None, sub: None, row: None, dialog };
        }
        let row = self.row_at(p);
        let Some(it) = self.hit(p) else {
            return SettingsHover { act: None, sub: None, row, dialog: None };
        };
        let act = it.act.clone();
        let sub = match (&it.paint, it.rect) {
            (Paint::Seg(seg), r) => {
                let probe = Canvas::probe(self.scale);
                seg.hit(&probe, r, p)
            }
            (Paint::Chips(ch), r) => {
                let probe = Canvas::probe(self.scale);
                match ch.hit(&probe, r, p) {
                    Some(ChipHit::Item(i) | ChipHit::Remove(i)) => Some(i),
                    Some(ChipHit::More) => Some(usize::MAX - 1),
                    Some(ChipHit::Add) => Some(usize::MAX),
                    None => None,
                }
            }
            _ => None,
        };
        SettingsHover { act, sub, row, dialog: None }
    }

    fn invalidate_hover(&mut self, old: &SettingsHover, next: &SettingsHover, host: &mut Host) {
        if old.dialog != next.dialog {
            let pad = self.scale.s(4.0);
            if let Some((ok, cancel)) = self.dialog_buttons() {
                if old.dialog == Some(true) || next.dialog == Some(true) {
                    host.invalidate_rect(ok.inset(-pad, -pad));
                }
                if old.dialog == Some(false) || next.dialog == Some(false) {
                    host.invalidate_rect(cancel.inset(-pad, -pad));
                }
            } else {
                host.invalidate();
            }
            return;
        }
        let view = self.scroll.view;
        let row_pad_x = self.scale.s(12.0);
        let row_pad_y = self.scale.s(4.0);
        let act_pad = self.scale.s(4.0);
        let mut inval = |r: Rect| {
            let c = clip_rect(r, view);
            if !c.is_empty() {
                host.invalidate_rect(c);
            }
        };
        if old.row != next.row {
            for r in [&old.row, &next.row].into_iter().flatten() {
                if let Some(bg) = self.items.iter().find(|i| matches!(i.paint, Paint::RowBg) && i.row.as_ref() == Some(r)) {
                    inval(bg.rect.inset(-row_pad_x, -row_pad_y));
                }
            }
            for a in [&old.act, &next.act].into_iter().flatten() {
                if let Some(it) = self.item_with(a) {
                    inval(it.rect.inset(-act_pad, -act_pad));
                }
            }
        } else if old.act != next.act {
            for a in [&old.act, &next.act].into_iter().flatten() {
                if let Some(it) = self.item_with(a) {
                    inval(it.rect.inset(-act_pad, -act_pad));
                }
            }
        } else if old.sub != next.sub {
            if let Some(a) = &next.act {
                if let Some(it) = self.item_with(a) {
                    inval(it.rect.inset(-act_pad, -act_pad));
                }
            }
        }
    }
}

impl Surface for SettingsWindow {
    fn paint(&mut self, c: &mut Canvas) {
        // A caret blink invalidates the search bar only: draw just that.
        if self.size == (c.width(), c.height()) && self.scale == c.scale && self.dialog().is_none() {
            let bar = self.search_bar();
            if let Some(clip) = c.clip() {
                if clip.x >= bar.x && clip.y >= bar.y && clip.right() <= bar.right() && clip.bottom() <= bar.bottom() {
                    self.paint_search(c);
                    return;
                }
            }
        }
        self.scale = c.scale;
        self.size = (c.width(), c.height());
        self.layout(c);
        c.fill_rect(c.bounds(), BG);
        let view = self.scroll.view;
        c.push_clip(view);
        let items = std::mem::take(&mut self.items);
        let mut logo_done = false;
        for it in &items {
            if !it.rect.intersects(&view) && !matches!(it.paint, Paint::Card) {
                continue;
            }
            let is_logo = !logo_done && matches!(&it.paint, Paint::Text { s, .. } if s.is_empty());
            if is_logo {
                logo_done = true;
            }
            self.paint_item(c, it, is_logo);
        }
        self.items = items;
        c.pop_clip();
        self.scroll.paint(c, matches!(self.drag, Some(Drag::Thumb { .. })));
        self.paint_search(c);
        self.paint_footer(c);
        if let Some(d) = self.dialog() {
            let area = self.dialog_area(c);
            let (_, ok, cancel) = d.layout(c, area);
            self.dialog_rects = Some((ok, cancel));
            d.paint(c, area, self.mouse, false);
        } else {
            self.dialog_rects = None;
        }
        self.hover = self.compute_hover(self.mouse);
    }

    fn event(&mut self, ev: Event, host: &mut Host) -> bool {
        self.scale = host.scale;
        match ev {
            Event::Resized => {
                self.dialog_rects = None;
                host.set_timer(TIMER_REFRESH, 1000);
                self.sync_caret_timer(host);
                host.invalidate();
            }
            Event::Focus(on) => {
                if self.focused != on {
                    self.focused = on;
                    self.caret_on = self.caret_active();
                    self.sync_caret_timer(host);
                    host.invalidate_rect(self.search_bar());
                }
            }
            Event::Minimized(min) => {
                if self.minimized != min {
                    self.minimized = min;
                    self.caret_on = self.caret_active();
                    self.sync_caret_timer(host);
                    if !min {
                        host.invalidate();
                    }
                }
            }
            Event::Timer(TIMER_REFRESH) => {
                // Status-derived values (group usage, GB units) change with each engine tick,
                // and the store revision catches edits made on disk. Rebuild the page model
                // and repaint only when one of them (or the update row) changed.
                let status = self.host.status.latest();
                let update = (self.host.update_state)();
                let stamp = |s: &Option<Status>| s.as_ref().map(|s| (s.time.clone(), s.paused));
                if stamp(&status) != stamp(&self.status) || self.host.store.revision() != self.revision || update != self.update {
                    self.status = status;
                    self.reload();
                    host.invalidate();
                }
            }
            Event::Timer(TIMER_CARET) => {
                if self.caret_active() {
                    self.caret_on = !self.caret_on;
                    if self.caret_visible() {
                        host.invalidate_rect(self.search_bar());
                    }
                }
            }
            Event::MouseMove(p) => {
                self.mouse = Some(p);
                let mut dragged = false;
                let mut drag_changed = false;
                match &mut self.drag {
                    Some(Drag::Slider { rect, pct, .. }) => {
                        dragged = true;
                        let next = pct_from_frac(slider_frac_at(self.scale, *rect, p.x));
                        if (next - *pct).abs() > 1e-9 {
                            *pct = next;
                            drag_changed = true;
                        }
                    }
                    Some(Drag::Thumb { grab }) => {
                        dragged = true;
                        let top = p.y - *grab;
                        let next_y = scroll_y_for_thumb(&self.scroll, self.scale, top);
                        if next_y != self.scroll.y {
                            self.scroll.y = next_y;
                            drag_changed = true;
                        }
                    }
                    None => {}
                }
                if dragged {
                    host.set_cursor(if self.confirm.is_none() { Cursor::Hand } else { Cursor::Arrow });
                    if drag_changed {
                        host.invalidate();
                    }
                    return true;
                }
                let next = self.compute_hover(Some(p));
                let hand = self.confirm.is_none() && next.act.is_some();
                host.set_cursor(if hand { Cursor::Hand } else { Cursor::Arrow });
                if next != self.hover {
                    let old = std::mem::replace(&mut self.hover, next.clone());
                    self.invalidate_hover(&old, &next, host);
                }
            }
            Event::MouseLeave => {
                self.mouse = None;
                let old = std::mem::take(&mut self.hover);
                if old != SettingsHover::default() {
                    self.invalidate_hover(&old, &SettingsHover::default(), host);
                }
            }
            Event::MouseDown(p, MouseButton::Left) => {
                if self.confirm.is_some() {
                    return true;
                }
                if self.recording.is_some() {
                    self.recording = None;
                }
                if let Some(t) = scroll_thumb(&self.scroll, self.scale) {
                    if t.inset(-self.scale.s(4.0), 0).contains(p) {
                        self.drag = Some(Drag::Thumb { grab: p.y - t.y });
                        host.capture(true);
                        host.invalidate();
                        return true;
                    }
                }
                let hit = self.hit(p).map(|i| (i.act.clone().unwrap(), i.rect));
                if let Some((act, rect)) = hit {
                    self.focus = Some(act.clone());
                    if let Act::Slider(group, tier) = act {
                        let pct = pct_from_frac(slider_frac_at(self.scale, rect, p.x));
                        self.drag = Some(Drag::Slider { group, tier, rect, pct });
                        host.capture(true);
                    } else {
                        self.pressed = Some(act);
                    }
                } else {
                    self.focus = None;
                }
                host.invalidate();
            }
            Event::MouseUp(p, MouseButton::Left) => {
                if self.confirm.is_some() {
                    if let Some((ok, cancel)) = self.dialog_buttons() {
                        if ok.contains(p) {
                            self.confirm_ok();
                        } else if cancel.contains(p) {
                            self.confirm = None;
                            self.dialog_rects = None;
                        }
                    }
                    host.invalidate();
                    return true;
                }
                match self.drag.take() {
                    Some(Drag::Slider { group, tier, pct, .. }) => {
                        host.capture(false);
                        let path = group_path(&group, tier, "trim_above_pct");
                        self.write(|s| s.set(&path, Value::Float(pct)));
                    }
                    Some(Drag::Thumb { .. }) => host.capture(false),
                    None => {
                        let pressed = self.pressed.take();
                        let hit = self.hit(p).and_then(|i| i.act.clone());
                        if let (Some(a), Some(h)) = (pressed, hit) {
                            if a == h {
                                self.activate(a, Some(p), host);
                            }
                        }
                    }
                }
                host.invalidate();
            }
            Event::MouseUp(p, MouseButton::Right) => {
                if self.confirm.is_some() {
                    return true;
                }
                if let Some(key) = self.row_at(p) {
                    self.context_menu(key, p, host);
                }
            }
            Event::MouseDown(_, MouseButton::Right) => {}
            Event::CaptureLost => {
                if let Some(Drag::Slider { group, tier, pct, .. }) = self.drag.take() {
                    let path = group_path(&group, tier, "trim_above_pct");
                    self.write(|s| s.set(&path, Value::Float(pct)));
                }
                host.invalidate();
            }
            Event::Wheel(delta) => {
                if self.editing.is_some() {
                    host.end_edit(true);
                }
                let step = (self.scale.0 * 66.0) as i32;
                self.scroll.wheel(delta, step);
                host.invalidate();
            }
            Event::Key(vk, m) => {
                if self.record_key(vk, m, host) {
                    return true;
                }
                if self.confirm.is_some() {
                    match vk {
                        VK_ESCAPE | VK_RETURN => {
                            self.confirm = None;
                            self.dialog_rects = None;
                        }
                        _ => {}
                    }
                    host.invalidate();
                    return true;
                }
                if m.alt {
                    return false;
                }
                let page = self.scroll.view.h - (self.scale.0 * 40.0) as i32;
                match vk {
                    VK_ESCAPE => {
                        if !self.query.is_empty() {
                            self.query.clear();
                            self.scroll.y = 0;
                        } else {
                            host.close();
                        }
                    }
                    0x57 if m.ctrl => host.close(), // Ctrl+W
                    VK_TAB => self.move_focus(m.shift),
                    VK_BACK => {
                        self.query.pop();
                        self.scroll.y = 0;
                    }
                    VK_RETURN | VK_SPACE => {
                        if let Some(a) = self.focus.clone() {
                            self.activate(a, None, host);
                        } else if vk == VK_SPACE && !self.query.is_empty() {
                            self.query.push(' ');
                        }
                    }
                    VK_LEFT | VK_RIGHT | VK_PRIOR | VK_NEXT | VK_UP | VK_DOWN => {
                        let big = vk == VK_PRIOR || vk == VK_NEXT;
                        let up = vk == VK_RIGHT || vk == VK_PRIOR || vk == VK_UP;
                        match self.focus.clone() {
                            Some(Act::Slider(g, t)) => self.slider_step(g, t, if big { 1.0 } else { 0.1 } * if up { 1.0 } else { -1.0 }),
                            Some(Act::Seg(path)) if vk == VK_LEFT || vk == VK_RIGHT => {
                                if let Some((_, Ctl::Segmented { options, selected, .. })) = self.ctl_for_path(&path) {
                                    let n = options.len();
                                    let i = if up { (*selected + 1).min(n - 1) } else { selected.saturating_sub(1) };
                                    let v = options[i].0;
                                    self.write(|s| s.set(&path, Value::Text(v.into())));
                                }
                            }
                            _ => {
                                let d = if big { page } else { (self.scale.0 * 40.0) as i32 };
                                self.scroll.y += if up { -d } else { d };
                                self.scroll.clamp();
                            }
                        }
                    }
                    VK_HOME if self.query.is_empty() => self.scroll.y = 0,
                    VK_END if self.query.is_empty() => self.scroll.y = self.scroll.max(),
                    _ => {}
                }
                self.caret_on = true;
                host.invalidate();
            }
            Event::KeyUp(vk, m) => {
                if self.recording.is_some() && is_modifier(vk) {
                    let win = unsafe { GetKeyState(VK_LWIN as i32) < 0 || GetKeyState(VK_RWIN as i32) < 0 };
                    self.rec_partial = modifier_text(m, win);
                    host.invalidate();
                    return true;
                }
                return false;
            }
            Event::Char(ch) => {
                if self.recording.is_some() || self.confirm.is_some() || (ch == ' ' && (self.focus.is_some() || self.query.is_empty())) {
                    return true;
                }
                if self.query.chars().count() < 60 {
                    self.query.push(ch);
                    self.scroll.y = 0;
                    self.focus = None;
                    self.caret_on = true;
                }
                host.invalidate();
            }
            Event::EditDone { commit, text, .. } => self.on_edit_done(commit, text, host),
            Event::Menu { choice, .. } => self.on_menu(choice, host),
            Event::FileChosen { tag, path } => {
                self.on_file(tag, path);
                host.invalidate();
            }
            Event::Destroyed => {
                if let Some(r) = host.window_rect() {
                    let _ = super::ui_state::save(KIND, r);
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::snapshots::sample_settings;

    fn check_layout(scale: f32, width: i32, open: Option<&str>, query: &str) {
        let mut w = sample_settings();
        w.set_open_group(open);
        w.set_query(query);
        let s = Scale(scale);
        let probe = Canvas::new(s.s(width as f32), s.s(760.0), s);
        let h = w.page_height(&probe);
        let c = Canvas::new(s.s(width as f32), h, s);
        w.layout(&c);
        let items = w.items().to_vec();
        assert!(!items.is_empty());
        let ctx = format!("scale {scale} width {width} open {open:?} query {query:?}");
        // Everything inside the window horizontally.
        for it in &items {
            assert!(it.rect.x >= 0 && it.rect.right() <= c.width(), "{ctx}: {:?} outside", it.paint);
        }
        // Controls never overlap each other.
        let controls: Vec<&Item> = items.iter().filter(|i| i.act.is_some() && !matches!(i.paint, Paint::RowBg)).collect();
        for (i, a) in controls.iter().enumerate() {
            for b in &controls[i + 1..] {
                assert!(!a.rect.intersects(&b.rect), "{ctx}: {:?} overlaps {:?}", a.paint, b.paint);
            }
        }
        // Labels never run into controls, and every control sits inside its row.
        let labels: Vec<&Item> =
            items.iter().filter(|i| matches!(i.paint, Paint::Label { .. } | Paint::GroupName { .. } | Paint::TierLabel { .. })).collect();
        for l in &labels {
            for c in &controls {
                assert!(!l.rect.intersects(&c.rect), "{ctx}: label {:?} overlaps {:?}", l.paint, c.paint);
            }
        }
        let rows: Vec<&Item> = items.iter().filter(|i| matches!(i.paint, Paint::RowBg)).collect();
        for c in &controls {
            assert!(rows.iter().any(|r| r.rect.encloses(&c.rect)), "{ctx}: {:?} not inside a row", c.paint);
        }
        // Hero text and the live strip don't collide.
        let hero: Vec<&Item> =
            items.iter().filter(|i| matches!(&i.paint, Paint::Text { font, .. } if *font == F_HEADLINE || *font == F_STRIP)).collect();
        for (i, a) in hero.iter().enumerate() {
            for b in &hero[i + 1..] {
                assert!(!a.rect.intersects(&b.rect), "{ctx}: hero {:?} / {:?}", a.paint, b.paint);
            }
        }
    }

    #[test]
    fn layout_does_not_overlap_at_two_scales() {
        for scale in [1.0, 1.5] {
            for width in [900, 720] {
                check_layout(scale, width, None, "");
                check_layout(scale, width, Some("Browsers"), "");
                check_layout(scale, width, Some("Agents"), "");
                check_layout(scale, width, None, "cap");
            }
        }
    }

    #[test]
    fn search_filters_rows_and_opens_groups() {
        let mut w = sample_settings();
        let c = Canvas::new(900, 2000, Scale(1.0));
        w.set_query("hard cap");
        w.layout(&c);
        let labels: Vec<String> =
            w.items().iter().filter_map(|i| if let Paint::Label { label, .. } = &i.paint { Some(label.clone()) } else { None }).collect();
        assert_eq!(labels, ["Hard cap", "Hard cap", "Freeze and hard cap", "Hard cap"], "three hard caps and the Agents explanation");
        assert!(!w.items().iter().any(|i| matches!(&i.paint, Paint::Text { s, .. } if s == "SETTINGS")), "hero hidden while searching");
        w.set_query("notifications");
        w.layout(&c);
        let sections: Vec<String> = w.items().iter().filter_map(|i| if let Paint::Section(s) = &i.paint { Some(s.clone()) } else { None }).collect();
        assert_eq!(sections, vec!["GENERAL".to_string()]);
        w.set_query("sold slower");
        w.layout(&c);
        let labels: Vec<String> =
            w.items().iter().filter_map(|i| if let Paint::Label { label, .. } = &i.paint { Some(label.clone()) } else { None }).collect();
        assert_eq!(labels, ["Check memory speed"]);
        w.set_query("zzz nothing");
        w.layout(&c);
        assert!(w.items().iter().all(|i| !matches!(i.paint, Paint::Label { .. })));
    }

    #[test]
    fn hotkey_names() {
        let ctrl_alt = Mods { ctrl: true, alt: true, shift: false };
        assert_eq!(hotkey_text(ctrl_alt, false, 0x4D).as_deref(), Some("Ctrl+Alt+M"));
        assert_eq!(hotkey_text(Mods::default(), false, 0x4D), None, "a plain letter would stop typing");
        assert_eq!(hotkey_text(Mods::default(), false, 0x78).as_deref(), Some("F9"));
        assert_eq!(hotkey_text(Mods { shift: true, ..Mods::default() }, true, 0x31).as_deref(), Some("Shift+Win+1"));
        assert_eq!(HotkeyField::display("Ctrl+Alt+M"), "Ctrl + Alt + M");
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::ui::snapshots::sample_settings;

    fn painted(w: &mut SettingsWindow) -> Canvas {
        let mut c = Canvas::new(900, 2400, Scale(1.0));
        w.paint(&mut c);
        c
    }

    fn click(w: &mut SettingsWindow, p: Pt) {
        let mut host = Host::detached((900, 2400), Scale(1.0));
        w.event(Event::MouseDown(p, MouseButton::Left), &mut host);
        w.event(Event::MouseUp(p, MouseButton::Left), &mut host);
    }

    fn value(w: &SettingsWindow, path: &str) -> Option<Value> {
        w.host.store.get(path).map(|s| s.value)
    }

    #[test]
    fn clicks_write_through_the_store() {
        let mut w = sample_settings();
        painted(&mut w);
        let toggle = w.item_with(&Act::Toggle("general.notifications".into())).unwrap().rect;
        click(&mut w, Pt::new(toggle.cx(), toggle.cy()));
        assert_eq!(value(&w, "general.notifications"), Some(Value::Bool(false)));
        painted(&mut w);
        let toggle = w.item_with(&Act::Toggle("general.check_ram_speed".into())).unwrap().rect;
        click(&mut w, Pt::new(toggle.cx(), toggle.cy()));
        assert_eq!(value(&w, "general.check_ram_speed"), Some(Value::Bool(false)));
        painted(&mut w);
        let seg =w.item_with(&Act::Seg("general.preset".into())).unwrap().rect;
        click(&mut w, Pt::new(seg.right() - 10, seg.cy()));
        assert_eq!(value(&w, "general.preset"), Some(Value::Text("aggressive".into())));

        // Open Browsers, drag its "Unreal is running" slider to the far right.
        painted(&mut w);
        let head = w.items().iter().find(|i| i.act == Some(Act::GroupHeader("Browsers".into()))).unwrap().rect;
        click(&mut w, Pt::new(head.x + 30, head.cy()));
        painted(&mut w);
        let act = Act::Slider("Browsers".into(), Tier::Unreal);
        let s = w.item_with(&act).unwrap().rect;
        let mut host = Host::detached((900, 2400), Scale(1.0));
        w.event(Event::MouseDown(Pt::new(s.x + 20, s.cy()), MouseButton::Left), &mut host);
        w.event(Event::MouseMove(Pt::new(s.right() + 50, s.cy())), &mut host);
        w.event(Event::MouseUp(Pt::new(s.right() + 50, s.cy()), MouseButton::Left), &mut host);
        assert_eq!(value(&w, "group.Browsers.ue.trim_above_pct"), Some(Value::Float(25.0)));
        // Arrow keys step 0.1 %, PgDn 1 %.
        painted(&mut w);
        w.event(Event::Key(VK_LEFT, Mods::default()), &mut host);
        w.event(Event::Key(VK_NEXT, Mods::default()), &mut host);
        assert_eq!(value(&w, "group.Browsers.ue.trim_above_pct"), Some(Value::Float(23.9)));
    }

    #[test]
    fn caret_blink_repaints_only_the_search_bar() {
        let mut w = sample_settings();
        let mut c = painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));
        w.event(Event::Timer(TIMER_CARET), &mut host);
        assert_eq!(host.invalidations(), (0, 1));
        assert!(!w.caret_on);
        let bar = w.search_bar();
        let before: Vec<Rgb> = (0..bar.h).flat_map(|y| (0..bar.w).map(move |x| (x, y))).map(|(x, y)| c.pixel(x, y)).collect();
        c.push_clip(bar);
        w.paint(&mut c);
        c.pop_clip();
        // The partial frame is exactly what a full repaint draws, and the caret did go off.
        let full = painted(&mut w);
        let mut changed = 0;
        for y in (0..2400).step_by(3) {
            for x in 0..900 {
                assert_eq!(c.pixel(x, y), full.pixel(x, y), "({}, {})", x, y);
            }
        }
        for (i, px) in before.iter().enumerate() {
            let (x, y) = (i as i32 % bar.w, i as i32 / bar.w);
            assert_eq!(c.pixel(x, y), full.pixel(x, y));
            changed += (*px != c.pixel(x, y)) as usize;
        }
        assert!(changed > 0);
    }

    #[test]
    fn refresh_timer_repaints_only_when_something_changed() {
        let mut w = sample_settings();
        painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));
        w.event(Event::Timer(TIMER_REFRESH), &mut host);
        w.event(Event::Timer(TIMER_REFRESH), &mut host);
        assert_eq!(host.invalidations(), (0, 0), "same status, same settings");
        w.status.as_mut().unwrap().time = "2000-01-01 00:00:00".into(); // a newer status arrives
        w.event(Event::Timer(TIMER_REFRESH), &mut host);
        assert_eq!(host.invalidations(), (1, 0));
    }

    #[test]
    fn typing_searches_and_escape_clears() {
        let mut w = sample_settings();
        painted(&mut w);
        let mut host = Host::detached((900, 760), Scale(1.0));
        for ch in "notif".chars() {
            w.event(Event::Char(ch), &mut host);
        }
        assert_eq!(w.query, "notif");
        w.event(Event::Key(VK_BACK, Mods::default()), &mut host);
        assert_eq!(w.query, "noti");
        w.event(Event::Key(VK_ESCAPE, Mods::default()), &mut host);
        assert_eq!(w.query, "");
        // Tab walks the controls in order.
        painted(&mut w);
        w.event(Event::Key(VK_TAB, Mods::default()), &mut host);
        assert_eq!(w.focus, Some(Act::Seg("general.preset".into())));
        w.event(Event::Key(VK_TAB, Mods::default()), &mut host);
        assert_eq!(w.focus, Some(Act::Toggle("general.auto_prime".into())));
        w.event(Event::Key(VK_SPACE, Mods::default()), &mut host);
        assert_eq!(value(&w, "general.auto_prime"), Some(Value::Bool(false)));
        w.event(Event::Key(VK_TAB, Mods { shift: true, ..Mods::default() }), &mut host);
        assert_eq!(w.focus, Some(Act::Seg("general.preset".into())));
    }

    #[test]
    fn hard_cap_asks_first() {
        let mut w = sample_settings();
        w.set_open_group(Some("Browsers"));
        painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));
        w.editing = Some(EditTarget::Number("group.Browsers.hard_cap_mb".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "4096".into() }, &mut host);
        assert!(w.confirm.is_some());
        assert_eq!(value(&w, "group.Browsers.hard_cap_mb"), None, "nothing written before the answer");
        w.event(Event::Key(VK_RETURN, Mods::default()), &mut host);
        assert!(w.confirm.is_none());
        assert_eq!(value(&w, "group.Browsers.hard_cap_mb"), None, "Enter is the safe choice");
        w.editing = Some(EditTarget::Number("group.Browsers.hard_cap_mb".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "4096".into() }, &mut host);
        w.confirm_ok();
        assert_eq!(value(&w, "group.Browsers.hard_cap_mb"), Some(Value::Int(4096)));
    }

    #[test]
    fn f62a_controls_edit_headroom_disk_doctor_and_interval() {
        let mut w = sample_settings();
        painted(&mut w);
        let mut host = Host::detached((900, 3200), Scale(1.0));

        // interval_sec (integer)
        w.editing = Some(EditTarget::Number("interval_sec".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "15".into() }, &mut host);
        assert_eq!(value(&w, "interval_sec"), Some(Value::Int(15)));

        // prime.build.disk_gb: setting 0 ("Off") and back to 64.5 does NOT trigger HardCap confirmation.
        w.editing = Some(EditTarget::Number("prime.build.disk_gb".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "off".into() }, &mut host);
        assert_eq!(value(&w, "prime.build.disk_gb"), Some(Value::Float(0.0)));
        assert!(w.confirm.is_none());
        w.editing = Some(EditTarget::Number("prime.build.disk_gb".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "64.5".into() }, &mut host);
        assert_eq!(value(&w, "prime.build.disk_gb"), Some(Value::Float(64.5)));
        assert!(w.confirm.is_none());

        // [disk] float and int edits + toggle
        w.editing = Some(EditTarget::Number("disk.min_age_hours".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "0.75".into() }, &mut host);
        assert_eq!(value(&w, "disk.min_age_hours"), Some(Value::Float(0.75)));
        w.editing = Some(EditTarget::Number("disk.scan_depth".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "5".into() }, &mut host);
        assert_eq!(value(&w, "disk.scan_depth"), Some(Value::Int(5)));

        // Long path in disk.project_dirs compacts cleanly and removes by exact stored index.
        let long_path = r"D:\VeryLongWorkspaceRoot\StudioProjects\Unreal5\MySuperLongProjectName\Saved\DerivedDataCache";
        w.editing = Some(EditTarget::AddChip("disk.project_dirs".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: format!("\"{}\"", long_path) }, &mut host);
        assert_eq!(value(&w, "disk.project_dirs"), Some(Value::List(vec![long_path.into()])));

        w.set_query("disk space");
        let mut c = Canvas::new(720, 2400, Scale(1.5));
        w.paint(&mut c);
        let chips_it = w.item_with(&Act::Chips(ChipOwner::List("disk.project_dirs".into()))).unwrap();
        if let Paint::Chips(ch) = &chips_it.paint {
            for (cr, _) in ch.layout(&c, chips_it.rect.w) {
                assert!(cr.x >= 0 && cr.right() <= chips_it.rect.w, "chip overflowed ctl width: {:?}", cr);
            }
        } else {
            panic!("expected Chips");
        }
    }

    #[test]
    fn f61_dirty_hover_gating_and_no_dialog_dib_alloc() {
        let mut w = sample_settings();
        painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));

        // Moving over unchanged blank margin produces zero invalidations.
        w.event(Event::MouseMove(Pt::new(2, 120)), &mut host);
        assert_eq!(host.invalidations(), (0, 0));
        w.event(Event::MouseLeave, &mut host);
        assert_eq!(host.invalidations(), (0, 0));

        // Moving onto a toggle invalidates once via invalidate_rect; moving 1px inside does not.
        let toggle = w.item_with(&Act::Toggle("general.notifications".into())).unwrap().rect;
        w.event(Event::MouseMove(Pt::new(toggle.cx(), toggle.cy())), &mut host);
        let first_inv = host.invalidations();
        assert_eq!(first_inv.0, 0);
        assert!(first_inv.1 >= 1);
        w.event(Event::MouseMove(Pt::new(toggle.cx() + 1, toggle.cy())), &mut host);
        assert_eq!(host.invalidations(), first_inv);

        // Moving across Segmented options invalidates when segment changes, not within same segment.
        let seg = w.item_with(&Act::Seg("general.preset".into())).unwrap().rect;
        w.event(Event::MouseMove(Pt::new(seg.x + 10, seg.cy())), &mut host);
        let seg1 = host.invalidations();
        w.event(Event::MouseMove(Pt::new(seg.x + 12, seg.cy())), &mut host);
        assert_eq!(host.invalidations(), seg1);
        w.event(Event::MouseMove(Pt::new(seg.right() - 10, seg.cy())), &mut host);
        assert!(host.invalidations().1 > seg1.1);

        // MouseLeave after hover invalidates once, then a second MouseLeave does nothing.
        let before_leave = host.invalidations();
        w.event(Event::MouseLeave, &mut host);
        assert!(host.invalidations().1 > before_leave.1);
        let after_leave = host.invalidations();
        w.event(Event::MouseLeave, &mut host);
        assert_eq!(host.invalidations(), after_leave);

        // Dialog paint caches dialog_rects; lazy fallback via Canvas::probe populates dialog_rects and MouseUp consumes it.
        w.confirm = Some(Confirm::ResetAll);
        painted(&mut w);
        let cached = w.dialog_rects.expect("paint() must cache dialog_rects");
        w.dialog_rects = None;
        let fallback = w.dialog_buttons().expect("dialog_buttons() lazy fallback");
        assert_eq!(fallback, cached);
        assert_eq!(w.dialog_rects, Some(cached));
        let (_ok, cancel) = cached;
        w.event(Event::MouseUp(Pt::new(cancel.cx(), cancel.cy()), MouseButton::Left), &mut host);
        assert!(w.confirm.is_none());
        assert!(w.dialog_rects.is_none());
    }

    #[test]
    fn f61_focus_and_minimize_pause_caret_timer() {
        let mut w = sample_settings();
        painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));
        assert!(w.caret_visible());

        // Losing focus stops caret blinking even if a stale timer or Resized arrives.
        w.event(Event::Focus(false), &mut host);
        assert!(!w.caret_visible());
        let inv = host.invalidations();
        let caret_before = w.caret_on;
        w.event(Event::Timer(TIMER_CARET), &mut host);
        assert_eq!(host.invalidations(), inv);
        assert_eq!(w.caret_on, caret_before);
        w.event(Event::Resized, &mut host);
        assert!(!w.caret_visible());

        // Regaining focus restores caret visibility and blinking.
        w.event(Event::Focus(true), &mut host);
        assert!(w.caret_visible());
        let inv = host.invalidations();
        w.event(Event::Timer(TIMER_CARET), &mut host);
        assert_eq!(host.invalidations().1, inv.1 + 1);

        // Minimizing also pauses caret blinking.
        w.event(Event::Minimized(true), &mut host);
        assert!(!w.caret_visible());
        let inv = host.invalidations();
        w.event(Event::Timer(TIMER_CARET), &mut host);
        assert_eq!(host.invalidations(), inv);
        w.event(Event::Minimized(false), &mut host);
        assert!(w.caret_visible());
    }

    #[test]
    fn bug3_defers_add_group_until_first_process_picked_or_typed() {
        let mut w = sample_settings();
        painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));
        let initial_groups = w.host.store.groups().len();

        // Submitting group name opens add_app_menu WITHOUT creating an empty group yet.
        w.editing = Some(EditTarget::AddGroup);
        w.event(Event::EditDone { tag: 0, commit: true, text: "Renderers".into() }, &mut host);
        assert_eq!(w.host.store.groups().len(), initial_groups);
        assert!(matches!(w.pending, Some(Pending::AddApp { ref group, create: true, .. }) if group == "Renderers"));

        // Canceling the menu leaves groups untouched.
        w.event(Event::Menu { tag: MENU_PICK, choice: None }, &mut host);
        assert_eq!(w.host.store.groups().len(), initial_groups);
        assert!(w.open_group.is_none());

        // Choosing "Type a name..." and submitting empty or canceling leaves groups untouched.
        w.editing = Some(EditTarget::AddGroup);
        w.event(Event::EditDone { tag: 0, commit: true, text: "Renderers".into() }, &mut host);
        w.event(Event::Menu { tag: MENU_PICK, choice: Some(MENU_ID_TYPE) }, &mut host);
        assert!(matches!(w.editing, Some(EditTarget::AddApp { ref group, create: true }) if group == "Renderers"));
        w.event(Event::EditDone { tag: 0, commit: true, text: ".EXE".into() }, &mut host);
        assert_eq!(w.host.store.groups().len(), initial_groups);

        // Typing a valid process creates the group with stripped .exe and opens its card.
        w.editing = Some(EditTarget::AddGroup);
        w.event(Event::EditDone { tag: 0, commit: true, text: "Renderers".into() }, &mut host);
        w.event(Event::Menu { tag: MENU_PICK, choice: Some(MENU_ID_TYPE) }, &mut host);
        w.event(Event::EditDone { tag: 0, commit: true, text: "Blender.EXE".into() }, &mut host);
        let g = w.host.store.groups().into_iter().find(|g| g.name == "Renderers").unwrap();
        assert_eq!(g.processes, vec!["Blender".to_string()]);
        assert_eq!(w.open_group.as_deref(), Some("Renderers"));

        // Picking a running app from the menu also creates the group.
        w.editing = Some(EditTarget::AddGroup);
        w.event(Event::EditDone { tag: 0, commit: true, text: "Design".into() }, &mut host);
        w.event(Event::Menu { tag: MENU_PICK, choice: Some(1) }, &mut host);
        let g2 = w.host.store.groups().into_iter().find(|g| g.name == "Design").unwrap();
        assert_eq!(g2.processes, vec!["Figma".to_string()]);
        assert_eq!(w.open_group.as_deref(), Some("Design"));
    }

    #[test]
    fn bug12_running_apps_snapshot_per_owner_chips_and_case_insensitive_exe_strip() {
        use std::sync::{Arc, Mutex};
        use crate::ui::settings_store::{FakeStore, GroupInfo, RunningApp};
        use crate::ui::status_source::FakeStatus;

        struct SharedStore(Arc<Mutex<FakeStore>>);
        impl SettingsStore for SharedStore {
            fn get(&self, path: &str) -> Option<crate::ui::settings_store::Setting> { self.0.lock().unwrap().get(path) }
            fn set(&mut self, path: &str, value: Value) -> Result<(), String> { self.0.lock().unwrap().set(path, value) }
            fn reset(&mut self, path: &str) -> Result<(), String> { self.0.lock().unwrap().reset(path) }
            fn groups(&self) -> Vec<GroupInfo> { self.0.lock().unwrap().groups() }
            fn add_group(&mut self, name: &str, processes: &[String]) -> Result<(), String> { self.0.lock().unwrap().add_group(name, processes) }
            fn remove_group(&mut self, name: &str) -> Result<(), String> { self.0.lock().unwrap().remove_group(name) }
            fn rename_group(&mut self, from: &str, to: &str) -> Result<(), String> { self.0.lock().unwrap().rename_group(from, to) }
            fn set_group_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> { self.0.lock().unwrap().set_group_enabled(name, enabled) }
            fn add_process(&mut self, group: &str, process: &str) -> Result<(), String> { self.0.lock().unwrap().add_process(group, process) }
            fn remove_process(&mut self, group: &str, process: &str) -> Result<(), String> { self.0.lock().unwrap().remove_process(group, process) }
            fn export_to(&self, path: &std::path::Path) -> Result<(), String> { self.0.lock().unwrap().export_to(path) }
            fn import_from(&mut self, path: &std::path::Path) -> Result<(), String> { self.0.lock().unwrap().import_from(path) }
            fn load_team_file(&mut self, path: &std::path::Path) -> Result<(), String> { self.0.lock().unwrap().load_team_file(path) }
            fn remove_team_file(&mut self) -> Result<(), String> { self.0.lock().unwrap().remove_team_file() }
            fn team_info(&self) -> Option<crate::ui::settings_store::TeamInfo> { self.0.lock().unwrap().team_info() }
            fn reset_all(&mut self) -> Result<(), String> { self.0.lock().unwrap().reset_all() }
            fn run_at_startup(&self) -> bool { self.0.lock().unwrap().run_at_startup() }
            fn set_run_at_startup(&mut self, on: bool) -> Result<(), String> { self.0.lock().unwrap().set_run_at_startup(on) }
            fn running_apps(&self) -> Vec<RunningApp> { self.0.lock().unwrap().running_apps() }
            fn settings_file(&self) -> PathBuf { self.0.lock().unwrap().settings_file() }
            fn revision(&self) -> u64 { self.0.lock().unwrap().revision() }
        }

        let shared = Arc::new(Mutex::new(FakeStore::mockup()));
        let mut w = SettingsWindow::new(SettingsHost {
            store: Box::new(SharedStore(Arc::clone(&shared))),
            status: Box::new(FakeStatus::default()),
            commands: Box::new(|_| {}),
            app_version: "0.2.0".into(),
            update_state: Box::new(|| UpdateState::Unknown),
        });
        painted(&mut w);
        let mut host = Host::detached((900, 2400), Scale(1.0));

        // Snapshot running_apps(): mutate live running list AFTER opening menu, BEFORE picking item 1 ("Figma").
        w.add_app_menu("Browsers".into(), false, Pt::new(100, 100), &mut host);
        {
            let mut s = shared.lock().unwrap();
            s.running.insert(0, RunningApp { name: "NewHugeApp".into(), processes: 2, ram_gb: 9.9 });
        }
        w.event(Event::Menu { tag: MENU_PICK, choice: Some(1) }, &mut host);
        let browsers = shared.lock().unwrap().groups().into_iter().find(|g| g.name == "Browsers").unwrap();
        assert!(browsers.processes.contains(&"Figma".to_string()));
        assert!(!browsers.processes.contains(&"NewHugeApp".to_string()));

        // Per-owner +N chip expansion: expanding Background's GroupApps does NOT expand ue.protect.
        w.set_open_group(Some("Background"));
        let mut c = painted(&mut w);
        let bg_act = Act::Chips(ChipOwner::Group("Background".into()));
        let bg_rect = w.item_with(&bg_act).unwrap().rect;
        let bg_chips = match &w.item_with(&bg_act).unwrap().paint {
            Paint::Chips(ch) => ch.clone(),
            _ => panic!("expected Chips"),
        };
        // Narrow test width if needed so +N appears on Background or verify via direct +N on ue.protect vs GroupApps:
        let _ = (bg_rect, bg_chips);
        // Simulate clicking +N on Group("Background") vs List("ue.protect"):
        w.expanded_chips.insert(ChipOwner::Group("Background".into()));
        w.paint(&mut c);
        assert!(w.expanded_chips.contains(&ChipOwner::Group("Background".into())));
        assert!(!w.expanded_chips.contains(&ChipOwner::List("ue.protect".into())));
        if let Paint::Chips(protect_ch) = &w.item_with(&Act::Chips(ChipOwner::List("ue.protect".into()))).unwrap().paint {
            assert!(!protect_ch.expanded, "expanding a group must not expand ue.protect");
        }

        // Case-insensitive .exe strip on process lists, preserved on disk path lists.
        w.editing = Some(EditTarget::AddChip("ue.protect".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "  MyTool.EXE  ".into() }, &mut host);
        let prot = shared.lock().unwrap().get("ue.protect").unwrap().value.as_list().unwrap().to_vec();
        assert!(prot.contains(&"MyTool".to_string()));

        w.editing = Some(EditTarget::AddChip("doctor.keep".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: "Spotify.Exe".into() }, &mut host);
        let dkeep = shared.lock().unwrap().get("doctor.keep").unwrap().value.as_list().unwrap().to_vec();
        assert_eq!(dkeep, vec!["Spotify".to_string()]);

        w.editing = Some(EditTarget::AddChip("disk.keep".into()));
        w.event(Event::EditDone { tag: 0, commit: true, text: r"C:\Tools\KeepMe.exe".into() }, &mut host);
        let diskk = shared.lock().unwrap().get("disk.keep").unwrap().value.as_list().unwrap().to_vec();
        assert_eq!(diskk, vec![r"C:\Tools\KeepMe.exe".to_string()]);
    }
}

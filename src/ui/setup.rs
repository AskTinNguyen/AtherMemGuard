//! The setup window (docs/UI.md section 4): install, update, reinstall, uninstall.
//!
//! The window owns no install logic. Everything that touches the PC goes through a
//! `SetupBackend`: `RealBackend` calls `install::*`, `FakeBackend` (snapshots, tests, the demo)
//! only waits and reports a configured outcome. Install and uninstall run on a worker thread
//! that posts a message (`WM_TIMER` with `TIMER_WAKE`) to the window when it is done.
//!
//! The flow is a pure state machine (`SetupModel`): inputs (button clicks, toggles, the worker's
//! result) go in, `Effect`s (run a task, open settings, exit with a code) come out.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN, VK_SPACE, VK_TAB};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, PostMessageW, WM_TIMER, WS_CAPTION, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
};

use super::canvas::{Align, Canvas};
use super::geom::{Pt, Rect};
use super::kit::{focus_ring, Toggle, WState};
use super::theme::*;
use super::window::{self, Cursor, Event, Host, MouseButton, Surface, WindowSpec};

pub const KIND: &str = "setup";
pub const TITLE: &str = "Ather MemGuard Setup";
/// Client size at 96 DPI.
pub const SIZE: (i32, i32) = (520, 600);

const TIMER_WAKE: usize = 7001;
const TIMER_ANIM: usize = 7002;

// ---------------------------------------------------------------- backend

/// Everything setup does to the PC. `Send` so install can run on a worker thread.
pub trait SetupBackend: Send + 'static {
    /// The version registered under Settings > Apps, `None` when not installed.
    fn installed_version(&self) -> Option<String>;
    fn install(&self, start_with_windows: bool, add_to_path: bool) -> Result<(), String>;
    fn uninstall(&self) -> Result<(), String>;
    /// Write the portable marker next to this exe and start the tray from here.
    fn run_portable(&self) -> Result<(), String>;
    /// Open settings in the (just installed) tray app.
    fn open_settings(&self);
    /// Addition: (start with Windows, memguard on PATH) of the current install, to pre-set the
    /// toggles for Update / Reinstall. `None` = use the defaults (both on).
    fn installed_options(&self) -> Option<(bool, bool)> {
        None
    }
    /// Addition: "Just run it" on the Reinstall screen starts the installed copy.
    fn run_installed(&self) -> Result<(), String> {
        Ok(())
    }
}

/// The real thing: calls `install::*`. Never construct it in tests, demos or snapshots.
pub struct RealBackend;

impl RealBackend {
    fn installed_exe() -> std::path::PathBuf {
        crate::paths::install_dir().join(crate::paths::TRAY_EXE)
    }

    fn launch(exe: &std::path::Path, args: &str) -> Result<(), String> {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let op = crate::win::wide("open");
        let f = crate::win::wide(&exe.to_string_lossy());
        let a = crate::win::wide(args);
        let r = unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), f.as_ptr(), a.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
        if (r as isize) <= 32 {
            Err(format!("Couldn't start {}.", exe.display()))
        } else {
            Ok(())
        }
    }
}

impl SetupBackend for RealBackend {
    fn installed_version(&self) -> Option<String> {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
        let key = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(crate::paths::UNINSTALL_KEY, KEY_READ).ok()?;
        let v: String = key.get_value("DisplayVersion").ok()?;
        let v = v.trim().to_string();
        (!v.is_empty()).then_some(v)
    }

    fn installed_options(&self) -> Option<(bool, bool)> {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
        self.installed_version()?;
        let dir = crate::paths::install_dir().display().to_string().to_lowercase();
        let on_path = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_READ)
            .and_then(|k| k.get_value::<String, _>("Path"))
            .map(|p| p.split(';').any(|e| e.trim().trim_end_matches('\\').to_lowercase() == dir))
            .unwrap_or(false);
        Some((crate::install::is_run_at_startup(), on_path))
    }

    fn install(&self, start_with_windows: bool, add_to_path: bool) -> Result<(), String> {
        // install::install always turns "run at startup" on; switch it off afterwards when the
        // person said so (see the integration notes: install() should take this as a parameter).
        crate::install::install(add_to_path, true)?;
        if !start_with_windows {
            crate::install::set_run_at_startup(false)?;
        }
        Ok(())
    }

    fn uninstall(&self) -> Result<(), String> {
        crate::install::uninstall()
    }

    fn run_portable(&self) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let marker = exe.with_file_name(crate::paths::PORTABLE_MARKER);
        std::fs::write(&marker, b"Ather MemGuard runs from this folder without installing.\r\n")
            .map_err(|e| format!("Couldn't write {} ({}).", marker.display(), e))?;
        Self::launch(&exe, "")
    }

    fn open_settings(&self) {
        // Needs `--settings` in bin/tray.rs (forwarded to the running tray); see the notes.
        let _ = Self::launch(&Self::installed_exe(), "--settings");
    }

    fn run_installed(&self) -> Result<(), String> {
        Self::launch(&Self::installed_exe(), "")
    }
}

/// For snapshots, tests and the demo: touches nothing, waits `delay`, then reports `outcome`.
#[derive(Clone)]
pub struct FakeBackend {
    pub installed: Option<String>,
    pub options: Option<(bool, bool)>,
    /// What install / uninstall / run return.
    pub outcome: Result<(), String>,
    pub delay: Duration,
    /// Calls made, for tests ("install true false", "uninstall", ...).
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FakeBackend {
    pub fn new(installed: Option<&str>) -> FakeBackend {
        FakeBackend {
            installed: installed.map(String::from),
            options: installed.map(|_| (true, false)),
            outcome: Ok(()),
            delay: Duration::ZERO,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn failing(mut self, message: &str) -> FakeBackend {
        self.outcome = Err(message.to_string());
        self
    }
    pub fn with_delay(mut self, d: Duration) -> FakeBackend {
        self.delay = d;
        self
    }
    fn call(&self, what: String) -> Result<(), String> {
        self.calls.lock().unwrap().push(what);
        std::thread::sleep(self.delay);
        self.outcome.clone()
    }
}

impl SetupBackend for FakeBackend {
    fn installed_version(&self) -> Option<String> {
        self.installed.clone()
    }
    fn installed_options(&self) -> Option<(bool, bool)> {
        self.options
    }
    fn install(&self, start_with_windows: bool, add_to_path: bool) -> Result<(), String> {
        self.call(format!("install {} {}", start_with_windows, add_to_path))
    }
    fn uninstall(&self) -> Result<(), String> {
        self.call("uninstall".into())
    }
    fn run_portable(&self) -> Result<(), String> {
        self.call("run_portable".into())
    }
    fn open_settings(&self) {
        self.calls.lock().unwrap().push("open_settings".into());
    }
    fn run_installed(&self) -> Result<(), String> {
        self.call("run_installed".into())
    }
}

// ---------------------------------------------------------------- state machine

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    Install,
    Uninstall,
}

/// What the first screen offers, from the installed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    Fresh,
    Update { old: String },
    Reinstall,
    /// A newer version is installed (not in docs/UI.md): "REPLACE".
    Downgrade { old: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Install,
    Uninstall,
    Portable,
    RunInstalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Offer(Offer),
    ConfirmUninstall,
    Working(Task),
    Done,
    Uninstalled,
    Failed { task: Task, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// The lime button.
    Primary,
    /// The link or second button.
    Secondary,
    ToggleStart,
    TogglePath,
    /// The worker's result.
    Finished(Result<(), String>),
    /// Esc or the window closing.
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Run { task: Task, start_with_windows: bool, add_to_path: bool },
    OpenSettings,
    /// Close the window; `run_setup` returns this code.
    Exit(i32),
}

/// Exit codes of `run_setup`.
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_CANCELLED: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondaryStyle {
    Link,
    Button,
}

/// What one state shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub eyebrow: String,
    pub headline: String,
    pub body: Option<String>,
    pub options: bool,
    pub bar: bool,
    pub primary: Option<String>,
    pub secondary: Option<(String, SecondaryStyle)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupModel {
    pub version: String,
    pub phase: Phase,
    pub start_with_windows: bool,
    pub add_to_path: bool,
}

pub fn offer_for(installed: Option<&str>, version: &str) -> Offer {
    match installed {
        None => Offer::Fresh,
        Some(old) => match crate::updater::compare_versions(old, version) {
            Ordering::Less => Offer::Update { old: old.to_string() },
            Ordering::Equal => Offer::Reinstall,
            Ordering::Greater => Offer::Downgrade { old: old.to_string() },
        },
    }
}

impl SetupModel {
    pub fn new(mode: SetupMode, version: &str, installed: Option<&str>, options: Option<(bool, bool)>) -> SetupModel {
        let phase = match mode {
            SetupMode::Uninstall => Phase::ConfirmUninstall,
            SetupMode::Install => Phase::Offer(offer_for(installed, version)),
        };
        let (start_with_windows, add_to_path) = options.unwrap_or((true, true));
        SetupModel { version: version.to_string(), phase, start_with_windows, add_to_path }
    }

    fn run(&mut self, task: Task) -> Vec<Effect> {
        self.phase = Phase::Working(task);
        vec![Effect::Run { task, start_with_windows: self.start_with_windows, add_to_path: self.add_to_path }]
    }

    /// Feed one input; returns what the window must do.
    pub fn input(&mut self, input: Input) -> Vec<Effect> {
        match (self.phase.clone(), input) {
            (Phase::Offer(_), Input::ToggleStart) => {
                self.start_with_windows = !self.start_with_windows;
                vec![]
            }
            (Phase::Offer(_), Input::TogglePath) => {
                self.add_to_path = !self.add_to_path;
                vec![]
            }
            (Phase::Offer(_), Input::Primary) => self.run(Task::Install),
            (Phase::Offer(Offer::Fresh), Input::Secondary) => self.run(Task::Portable),
            (Phase::Offer(Offer::Reinstall), Input::Secondary) => self.run(Task::RunInstalled),
            (Phase::Offer(_), Input::Close) => vec![Effect::Exit(EXIT_CANCELLED)],
            (Phase::ConfirmUninstall, Input::Primary) => self.run(Task::Uninstall),
            (Phase::ConfirmUninstall, Input::Secondary | Input::Close) => vec![Effect::Exit(EXIT_CANCELLED)],
            (Phase::Working(task), Input::Finished(Ok(()))) => match task {
                Task::Install => {
                    self.phase = Phase::Done;
                    vec![]
                }
                Task::Uninstall => {
                    self.phase = Phase::Uninstalled;
                    vec![]
                }
                Task::Portable | Task::RunInstalled => vec![Effect::Exit(EXIT_OK)],
            },
            (Phase::Working(task), Input::Finished(Err(message))) => {
                self.phase = Phase::Failed { task, message };
                vec![]
            }
            (Phase::Done, Input::Primary) => vec![Effect::OpenSettings, Effect::Exit(EXIT_OK)],
            (Phase::Done, Input::Secondary | Input::Close) => vec![Effect::Exit(EXIT_OK)],
            (Phase::Uninstalled, Input::Primary | Input::Secondary | Input::Close) => vec![Effect::Exit(EXIT_OK)],
            (Phase::Failed { task, .. }, Input::Primary) => self.run(task),
            (Phase::Failed { .. }, Input::Secondary | Input::Close) => vec![Effect::Exit(EXIT_FAILED)],
            _ => vec![],
        }
    }

    /// The exit code if the window is closed now (title-bar X).
    pub fn exit_code_on_close(&self) -> i32 {
        match self.phase {
            Phase::Done | Phase::Uninstalled => EXIT_OK,
            Phase::Failed { .. } => EXIT_FAILED,
            _ => EXIT_CANCELLED,
        }
    }

    pub fn view(&self) -> View {
        let v = &self.version;
        let eyebrow = format!("ATHER MEMGUARD · V{}", v);
        let mk = |headline: &str, body: Option<String>, options: bool, primary: Option<&str>, secondary: Option<(&str, SecondaryStyle)>| View {
            eyebrow: eyebrow.clone(),
            headline: headline.to_string(),
            body,
            options,
            bar: false,
            primary: primary.map(String::from),
            secondary: secondary.map(|(s, st)| (s.to_string(), st)),
        };
        match &self.phase {
            Phase::Offer(Offer::Fresh) => mk(
                "INSTALL FOR YOU",
                Some("Keeps memory free for Unreal Editor, builds and cooks by holding back other apps. Installs just for your Windows account — no admin rights needed.".into()),
                true,
                Some("INSTALL"),
                Some(("Run without installing", SecondaryStyle::Link)),
            ),
            Phase::Offer(Offer::Update { old }) => mk(
                "UPDATE",
                Some(format!("Version {} is installed. This replaces it with {}. Your settings stay.", old, v)),
                true,
                Some("UPDATE"),
                None,
            ),
            Phase::Offer(Offer::Reinstall) => mk(
                "REINSTALL",
                Some(format!("Version {} is already installed. Reinstalling repairs it. Your settings stay.", v)),
                true,
                Some("REINSTALL"),
                Some(("Just run it", SecondaryStyle::Link)),
            ),
            Phase::Offer(Offer::Downgrade { old }) => mk(
                "REPLACE",
                Some(format!("Version {} is installed. This replaces it with {}. Your settings stay.", old, v)),
                true,
                Some("REPLACE"),
                None,
            ),
            Phase::Working(task) => {
                let headline = match task {
                    Task::Install => "INSTALLING…",
                    Task::Uninstall => "UNINSTALLING…",
                    Task::Portable | Task::RunInstalled => "STARTING…",
                };
                View { bar: true, ..mk(headline, None, false, None, None) }
            }
            Phase::Done => mk(
                "READY",
                Some("Ather MemGuard is in your tray. It starts holding back other apps when Unreal needs the memory.".into()),
                false,
                Some("OPEN SETTINGS"),
                Some(("DONE", SecondaryStyle::Button)),
            ),
            Phase::Failed { task, message } => {
                let headline = match task {
                    Task::Install => "COULDN'T INSTALL",
                    Task::Uninstall => "COULDN'T UNINSTALL",
                    Task::Portable | Task::RunInstalled => "COULDN'T START",
                };
                mk(headline, Some(message.clone()), false, Some("TRY AGAIN"), Some(("CLOSE", SecondaryStyle::Button)))
            }
            Phase::ConfirmUninstall => mk(
                "UNINSTALL?",
                Some("This removes Ather MemGuard and puts every app back to normal. Your settings and history are kept in case you come back.".into()),
                false,
                Some("UNINSTALL"),
                Some(("CANCEL", SecondaryStyle::Button)),
            ),
            Phase::Uninstalled => mk(
                "UNINSTALLED",
                Some("Ather MemGuard is removed and every app is back to normal. Your settings and history are kept in case you come back.".into()),
                false,
                Some("DONE"),
                None,
            ),
        }
    }
}

// ---------------------------------------------------------------- shared drawing helpers

/// Post a wake-up to a window from any thread: it arrives as `Event::Timer(id)`.
pub(crate) fn wake(hwnd: isize, id: usize) {
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd as HWND, WM_TIMER, id, 0) };
    }
}

/// A thin lime indeterminate bar; `t` runs 0..1 and wraps.
pub(crate) fn indeterminate_bar(c: &mut Canvas, r: Rect, t: f32) {
    c.fill_round_rect(r, r.h / 2, RAISED);
    let seg = (r.w as f32 * 0.32) as i32;
    let x = r.x - seg + ((r.w + seg) as f32 * t.rem_euclid(1.0)) as i32;
    let l = x.max(r.x);
    let rr = (x + seg).min(r.right());
    if rr > l {
        c.fill_round_rect(Rect::new(l, r.y, rr - l, r.h), r.h / 2, ACCENT);
    }
}

/// Like `Canvas::wrap_lines`, but words wider than a line (paths in error messages) are
/// broken into pieces that fit instead of being cut with an ellipsis.
pub(crate) fn wrap_hard(c: &Canvas, s: &str, f: Font, width: i32, max_lines: usize) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in s.split(' ').filter(|w| !w.is_empty()) {
        if c.measure(word, f).0 <= width {
            words.push(word.to_string());
            continue;
        }
        let mut piece = String::new();
        for ch in word.chars() {
            piece.push(ch);
            if c.measure(&piece, f).0 > width {
                piece.pop();
                words.push(std::mem::take(&mut piece));
                piece.push(ch);
            }
        }
        if !piece.is_empty() {
            words.push(piece);
        }
    }
    c.wrap_lines(&words.join(" "), f, width, max_lines)
}

pub(crate) const F_BIG_BUTTON: Font = Font::new(Face::Display, 16.0, REGULAR);

/// The large setup buttons: display face, letter-spaced, 12 x 34 padding.
pub(crate) fn big_button_size(c: &Canvas, label: &str) -> (i32, i32) {
    let extra = c.s(1.9);
    let tw = c.measure_spaced(label, F_BIG_BUTTON, extra).0 - extra;
    (tw + 2 * c.s(34.0), c.line_height(F_BIG_BUTTON) + 2 * c.s(12.0))
}

pub(crate) fn paint_big_button(c: &mut Canvas, r: Rect, label: &str, primary: bool, st: &WState) {
    let rad = c.s(RADIUS);
    let col = if primary {
        c.fill_round_rect(r, rad, if st.hover || st.pressed { ACCENT_SOFT } else { ACCENT });
        ON_ACCENT
    } else {
        c.fill_round_rect_bordered(r, rad, if st.pressed { BORDER } else if st.hover { SELECTED } else { RAISED }, BORDER);
        TEXT
    };
    c.spaced_text(r, label, F_BIG_BUTTON, col, c.s(1.9), Align::Center);
    if st.focused {
        focus_ring(c, r, rad);
    }
}

/// An underlined text link; `r` is its exact text box.
pub(crate) fn paint_link(c: &mut Canvas, r: Rect, label: &str, font: Font, color: Rgb, st: &WState) {
    let col = if st.hover { TEXT } else { color };
    c.text(r, label, font, col, Align::Left);
    let (tw, th) = c.measure(label, font);
    c.hline(r.x, r.cy() + th / 2 - c.s(2.0), tw.min(r.w), col);
    if st.focused {
        focus_ring(c, r, c.s(3.0));
    }
}

/// Eyebrow over a display headline, starting at (x, y). Returns the height used.
pub(crate) fn paint_title(c: &mut Canvas, x: i32, y: i32, w: i32, eyebrow: &str, headline: &str, headline_font: Font) -> i32 {
    let eh = c.line_height(F_EYEBROW);
    c.spaced_text(Rect::new(x, y, w, eh), eyebrow, F_EYEBROW, MUTED, c.s(3.0), Align::Left);
    let hh = c.line_height(headline_font);
    c.text(Rect::new(x, y + eh + c.s(4.0), w, hh), headline, headline_font, TEXT, Align::Left);
    eh + c.s(4.0) + hh
}

// ---------------------------------------------------------------- the window

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Primary,
    Secondary,
    ToggleStart,
    TogglePath,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Logo,
    Eyebrow(String),
    Headline(String),
    Body(Vec<String>),
    Bar,
    OptionText { label: String, hint: String },
    Toggle(bool, Act),
    Hairline,
    BigButton { label: String, primary: bool, act: Act },
    Link { label: String, act: Act },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub rect: Rect,
    pub part: Part,
}

impl Item {
    pub fn act(&self) -> Option<Act> {
        match &self.part {
            Part::Toggle(_, a) => Some(*a),
            Part::BigButton { act, .. } | Part::Link { act, .. } => Some(*act),
            _ => None,
        }
    }
}

type Worker = Rc<RefCell<Option<JoinHandle<Result<(), String>>>>>;

pub struct SetupWindow {
    pub model: SetupModel,
    backend: Arc<Mutex<Box<dyn SetupBackend>>>,
    worker: Worker,
    exit: Rc<Cell<Option<i32>>>,
    hwnd: isize,
    scale: Scale,
    mouse: Option<Pt>,
    pressed: Option<Act>,
    focus: Option<Act>,
    anim: f32,
    items: Vec<Item>,
}

impl SetupWindow {
    pub fn new(backend: Box<dyn SetupBackend>, mode: SetupMode, version: &str) -> SetupWindow {
        let installed = if mode == SetupMode::Install { backend.installed_version() } else { None };
        let options = if installed.is_some() { backend.installed_options() } else { None };
        let model = SetupModel::new(mode, version, installed.as_deref(), options);
        SetupWindow {
            model,
            backend: Arc::new(Mutex::new(backend)),
            worker: Rc::new(RefCell::new(None)),
            exit: Rc::new(Cell::new(None)),
            hwnd: 0,
            scale: Scale(1.0),
            mouse: None,
            pressed: None,
            focus: None,
            anim: 0.3,
            items: Vec::new(),
        }
    }

    /// The code `run_setup` returns, once the flow has ended.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit.get()
    }

    pub fn set_mouse(&mut self, p: Option<Pt>) {
        self.mouse = p;
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Lay the window out for a canvas of the window's client size.
    pub fn layout(&mut self, c: &Canvas) {
        let v = self.model.view();
        let mut items = Vec::new();
        let x = c.s(40.0);
        let w = c.width() - 2 * x;
        let mut y = c.s(34.0);
        let ls = c.s(96.0);
        items.push(Item { rect: Rect::new(x, y, ls, ls), part: Part::Logo });
        y += ls + c.s(22.0);
        let eh = c.line_height(F_EYEBROW);
        items.push(Item { rect: Rect::new(x, y, w, eh), part: Part::Eyebrow(v.eyebrow.clone()) });
        y += eh + c.s(4.0);
        let hh = c.line_height(F_HEADLINE);
        items.push(Item { rect: Rect::new(x, y, w, hh), part: Part::Headline(v.headline.clone()) });
        y += hh;
        if v.bar {
            y += c.s(28.0);
            items.push(Item { rect: Rect::new(x, y, w, c.s(3.0).max(2)), part: Part::Bar });
            y += c.s(3.0) + c.s(22.0);
        }
        if let Some(body) = &v.body {
            y += c.s(16.0);
            let lines = wrap_hard(c, body, F_BODY, w, 6);
            let lh = c.line_height(F_BODY) + c.s(2.0);
            let h = lines.len() as i32 * lh;
            items.push(Item { rect: Rect::new(x, y, w, h), part: Part::Body(lines) });
            y += h + c.s(22.0);
        }
        if v.options {
            let opts = [
                ("Start with Windows", "Runs quietly in the tray.", self.model.start_with_windows, Act::ToggleStart),
                ("Add the memguard command", "Lets agents and scripts check and free memory.", self.model.add_to_path, Act::TogglePath),
            ];
            let (tw, th) = Toggle::size(c);
            for (label, hint, on, act) in opts {
                items.push(Item { rect: Rect::new(x, y, w, c.px1(1.0)), part: Part::Hairline });
                let text_h = c.line_height(F_LABEL) + c.line_height(F_HINT);
                let row_h = text_h + 2 * c.s(12.0);
                items.push(Item {
                    rect: Rect::new(x, y + c.s(12.0), w - tw - c.s(14.0), text_h),
                    part: Part::OptionText { label: label.into(), hint: hint.into() },
                });
                items.push(Item { rect: Rect::new(x + w - tw, y + (row_h - th) / 2, tw, th), part: Part::Toggle(on, act) });
                y += row_h;
            }
            y += c.s(22.0) - c.s(22.0) / 2;
        }
        if let Some(p) = &v.primary {
            y += c.s(10.0);
            let (bw, bh) = big_button_size(c, p);
            items.push(Item { rect: Rect::new(x, y, bw, bh), part: Part::BigButton { label: p.clone(), primary: true, act: Act::Primary } });
            let mut bx = x + bw + c.s(18.0);
            if let Some((s, style)) = &v.secondary {
                match style {
                    SecondaryStyle::Button => {
                        let (sw, _) = big_button_size(c, s);
                        items.push(Item { rect: Rect::new(bx, y, sw, bh), part: Part::BigButton { label: s.clone(), primary: false, act: Act::Secondary } });
                        bx += sw;
                    }
                    SecondaryStyle::Link => {
                        let (lw, lh) = c.measure(s, F_VALUE);
                        items.push(Item { rect: Rect::new(bx, y + (bh - lh) / 2, lw + c.s(2.0), lh), part: Part::Link { label: s.clone(), act: Act::Secondary } });
                        bx += lw;
                    }
                }
            }
            let _ = bx;
        }
        self.items = items;
    }

    fn state_for(&self, it: &Item) -> WState {
        let act = it.act();
        let over = act.is_some() && self.mouse.map(|m| it.rect.contains(m)).unwrap_or(false);
        WState { hover: over, pressed: over && act.is_some() && self.pressed == act, focused: act.is_some() && self.focus == act, disabled: false, mouse: None }
    }

    fn hit(&self, p: Pt) -> Option<Act> {
        self.items.iter().rev().find(|i| i.act().is_some() && i.rect.contains(p)).and_then(|i| i.act())
    }

    fn focusables(&self) -> Vec<Act> {
        self.items.iter().filter_map(|i| i.act()).collect()
    }

    fn input(&mut self, input: Input, host: &mut Host) {
        let effects = self.model.input(input);
        for e in effects {
            match e {
                Effect::Run { task, start_with_windows, add_to_path } => self.start(task, start_with_windows, add_to_path, host),
                Effect::OpenSettings => self.backend.lock().unwrap().open_settings(),
                Effect::Exit(code) => {
                    self.exit.set(Some(code));
                    host.close();
                }
            }
        }
        // Focus moves to the new state's primary button.
        let acts = self.focusables_for_view();
        if self.focus.map(|f| !acts.contains(&f)).unwrap_or(false) {
            self.focus = None;
        }
        host.invalidate();
    }

    fn focusables_for_view(&self) -> Vec<Act> {
        let v = self.model.view();
        let mut out = Vec::new();
        if v.options {
            out.push(Act::ToggleStart);
            out.push(Act::TogglePath);
        }
        if v.primary.is_some() {
            out.push(Act::Primary);
        }
        if v.secondary.is_some() {
            out.push(Act::Secondary);
        }
        out
    }

    fn start(&mut self, task: Task, start_with_windows: bool, add_to_path: bool, host: &mut Host) {
        let backend = self.backend.clone();
        let job = move || -> Result<(), String> {
            let b = backend.lock().map_err(|_| "Setup stopped unexpectedly.".to_string())?;
            match task {
                Task::Install => b.install(start_with_windows, add_to_path),
                Task::Uninstall => b.uninstall(),
                Task::Portable => b.run_portable(),
                Task::RunInstalled => b.run_installed(),
            }
        };
        if self.hwnd == 0 {
            // No window (tests): run right here.
            let r = job();
            self.input(Input::Finished(r), host);
            return;
        }
        let hwnd = self.hwnd;
        let handle = std::thread::spawn(move || {
            let r = job();
            wake(hwnd, TIMER_WAKE);
            r
        });
        *self.worker.borrow_mut() = Some(handle);
        host.set_timer(TIMER_ANIM, 16);
    }

    fn poll_worker(&mut self, host: &mut Host) {
        let done = self.worker.borrow().as_ref().map(|h| h.is_finished()).unwrap_or(false);
        if !done {
            return;
        }
        let Some(h) = self.worker.borrow_mut().take() else { return };
        let r = h.join().unwrap_or_else(|_| Err("Setup stopped unexpectedly.".into()));
        host.kill_timer(TIMER_ANIM);
        self.input(Input::Finished(r), host);
    }
}

impl Surface for SetupWindow {
    fn paint(&mut self, c: &mut Canvas) {
        self.scale = c.scale;
        self.layout(c);
        c.fill_rect(c.bounds(), BG);
        let items = std::mem::take(&mut self.items);
        for it in &items {
            let st = self.state_for(it);
            let r = it.rect;
            match &it.part {
                Part::Logo => c.logo(r),
                Part::Eyebrow(s) => c.spaced_text(r, s, F_EYEBROW, MUTED, c.s(3.0), Align::Left),
                Part::Headline(s) => c.text(r, s, F_HEADLINE, TEXT, Align::Left),
                Part::Body(lines) => {
                    let lh = c.line_height(F_BODY) + c.s(2.0);
                    for (i, l) in lines.iter().enumerate() {
                        c.text(Rect::new(r.x, r.y + i as i32 * lh, r.w, lh), l, F_BODY, TEXT_DIM, Align::Left);
                    }
                }
                Part::Bar => indeterminate_bar(c, r, self.anim),
                Part::OptionText { label, hint } => {
                    let lh = c.line_height(F_LABEL);
                    c.text_top(Rect::new(r.x, r.y, r.w, lh), label, F_LABEL, TEXT, Align::Left);
                    c.text_top(Rect::new(r.x, r.y + lh, r.w, c.line_height(F_HINT)), hint, F_HINT, MUTED, Align::Left);
                }
                Part::Toggle(on, _) => Toggle { on: *on }.paint(c, r, &st),
                Part::Hairline => c.fill_rect(r, BORDER),
                Part::BigButton { label, primary, .. } => paint_big_button(c, r, label, *primary, &st),
                Part::Link { label, .. } => paint_link(c, r, label, F_VALUE, MUTED, &st),
            }
        }
        self.items = items;
    }

    fn event(&mut self, ev: Event, host: &mut Host) -> bool {
        self.scale = host.scale;
        if !host.hwnd.is_null() {
            self.hwnd = host.hwnd as isize;
        }
        match ev {
            Event::Resized => host.invalidate(),
            Event::Timer(TIMER_WAKE) => self.poll_worker(host),
            Event::Timer(TIMER_ANIM) => {
                self.anim = (self.anim + 0.012).rem_euclid(1.0);
                self.poll_worker(host);
                host.invalidate();
            }
            Event::MouseMove(p) => {
                self.mouse = Some(p);
                host.set_cursor(if self.hit(p).is_some() { Cursor::Hand } else { Cursor::Arrow });
                host.invalidate();
            }
            Event::MouseLeave => {
                self.mouse = None;
                host.invalidate();
            }
            Event::MouseDown(p, MouseButton::Left) => {
                self.pressed = self.hit(p);
                self.focus = self.pressed;
                host.invalidate();
            }
            Event::MouseUp(p, MouseButton::Left) => {
                let pressed = self.pressed.take();
                if let (Some(a), Some(h)) = (pressed, self.hit(p)) {
                    if a == h {
                        self.activate(a, host);
                    }
                }
                host.invalidate();
            }
            Event::Key(vk, m) => {
                if m.alt {
                    return false;
                }
                match vk {
                    VK_ESCAPE => self.input(Input::Close, host),
                    VK_TAB => {
                        let list = self.focusables();
                        if !list.is_empty() {
                            let i = self.focus.and_then(|f| list.iter().position(|a| *a == f));
                            let n = list.len();
                            let next = match (i, m.shift) {
                                (None, false) => 0,
                                (None, true) => n - 1,
                                (Some(i), false) => (i + 1) % n,
                                (Some(i), true) => (i + n - 1) % n,
                            };
                            self.focus = Some(list[next]);
                        }
                        host.invalidate();
                    }
                    VK_RETURN | VK_SPACE => {
                        let a = self.focus.or(if vk == VK_RETURN && self.model.view().primary.is_some() { Some(Act::Primary) } else { None });
                        if let Some(a) = a {
                            self.activate(a, host);
                        }
                    }
                    _ => {}
                }
            }
            Event::Destroyed => {
                if self.exit.get().is_none() {
                    self.exit.set(Some(self.model.exit_code_on_close()));
                }
            }
            _ => return false,
        }
        true
    }
}

impl SetupWindow {
    fn activate(&mut self, a: Act, host: &mut Host) {
        let input = match a {
            Act::Primary => Input::Primary,
            Act::Secondary => Input::Secondary,
            Act::ToggleStart => Input::ToggleStart,
            Act::TogglePath => Input::TogglePath,
        };
        self.input(input, host);
    }
}

/// Outer window size (96 DPI) for a client area of `SIZE` with the setup window's style.
fn outer_size() -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: SIZE.0, bottom: SIZE.1 };
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    if unsafe { AdjustWindowRectEx(&mut r, style, 0, 0) } == 0 {
        return (SIZE.0 + 6, SIZE.1 + 32);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// Show the setup window and run its message loop until it closes. Returns the exit code:
/// 0 installed / uninstalled / started, 1 failed, 2 cancelled. If the window is closed while
/// install runs, this waits for the worker and returns its result.
pub fn run_setup(backend: Box<dyn SetupBackend>, mode: SetupMode) -> i32 {
    run_setup_with_version(backend, mode, env!("CARGO_PKG_VERSION"))
}

/// `run_setup` with another version string (the demo shows the mockup's 0.2.0).
pub fn run_setup_with_version(backend: Box<dyn SetupBackend>, mode: SetupMode, version: &str) -> i32 {
    let w = SetupWindow::new(backend, mode, version);
    let exit = w.exit.clone();
    let worker = w.worker.clone();
    let spec = WindowSpec { kind: KIND, title: TITLE.into(), size: outer_size(), min_size: outer_size(), placement: None, resizable: false };
    let hwnd = window::open(spec, Box::new(w));
    if hwnd.is_null() {
        return EXIT_FAILED;
    }
    window::run_until_closed(hwnd);
    let pending = worker.borrow_mut().take();
    if let Some(h) = pending {
        return match h.join() {
            Ok(Ok(())) => EXIT_OK,
            _ => EXIT_FAILED,
        };
    }
    exit.get().unwrap_or(EXIT_CANCELLED)
}

/// Paint a setup window at its client size (snapshots).
pub fn render(w: &mut SetupWindow, scale: Scale) -> Canvas {
    let mut c = Canvas::new(scale.s(SIZE.0 as f32), scale.s(SIZE.1 as f32), scale);
    w.paint(&mut c);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(installed: Option<&str>) -> SetupModel {
        SetupModel::new(SetupMode::Install, "0.2.0", installed, None)
    }

    #[test]
    fn offers_follow_the_installed_version() {
        assert_eq!(model(None).phase, Phase::Offer(Offer::Fresh));
        assert_eq!(model(Some("0.1.0")).phase, Phase::Offer(Offer::Update { old: "0.1.0".into() }));
        assert_eq!(model(Some("0.2")).phase, Phase::Offer(Offer::Reinstall));
        assert_eq!(model(Some("0.3.0")).phase, Phase::Offer(Offer::Downgrade { old: "0.3.0".into() }));
        let m = SetupModel::new(SetupMode::Uninstall, "0.2.0", Some("0.2.0"), None);
        assert_eq!(m.phase, Phase::ConfirmUninstall);
        // Options pre-set from the current install.
        let m = SetupModel::new(SetupMode::Install, "0.2.0", Some("0.1.0"), Some((false, true)));
        assert!(!m.start_with_windows && m.add_to_path);
        assert!(model(None).start_with_windows && model(None).add_to_path);
    }

    #[test]
    fn install_success_then_open_settings() {
        let mut m = model(None);
        assert_eq!(m.input(Input::ToggleStart), vec![]);
        assert!(!m.start_with_windows);
        let e = m.input(Input::Primary);
        assert_eq!(e, vec![Effect::Run { task: Task::Install, start_with_windows: false, add_to_path: true }]);
        assert_eq!(m.phase, Phase::Working(Task::Install));
        // Nothing but the result moves it on while working.
        assert_eq!(m.input(Input::Primary), vec![]);
        assert_eq!(m.input(Input::Close), vec![]);
        assert_eq!(m.input(Input::ToggleStart), vec![]);
        assert_eq!(m.input(Input::Finished(Ok(()))), vec![]);
        assert_eq!(m.phase, Phase::Done);
        assert_eq!(m.input(Input::Primary), vec![Effect::OpenSettings, Effect::Exit(EXIT_OK)]);
        let mut m2 = m.clone();
        assert_eq!(m2.input(Input::Secondary), vec![Effect::Exit(EXIT_OK)]);
        assert_eq!(m.exit_code_on_close(), EXIT_OK);
    }

    #[test]
    fn failure_and_retry() {
        let mut m = model(Some("0.1.0"));
        m.input(Input::Primary);
        m.input(Input::Finished(Err("The install folder is in use.".into())));
        assert_eq!(m.phase, Phase::Failed { task: Task::Install, message: "The install folder is in use.".into() });
        let v = m.view();
        assert_eq!(v.headline, "COULDN'T INSTALL");
        assert_eq!(v.body.as_deref(), Some("The install folder is in use."));
        assert_eq!(v.primary.as_deref(), Some("TRY AGAIN"));
        assert_eq!(m.exit_code_on_close(), EXIT_FAILED);
        // Try again runs the same task with the same options.
        let e = m.input(Input::Primary);
        assert_eq!(e, vec![Effect::Run { task: Task::Install, start_with_windows: true, add_to_path: true }]);
        m.input(Input::Finished(Err("again".into())));
        assert_eq!(m.input(Input::Secondary), vec![Effect::Exit(EXIT_FAILED)]);
        m.input(Input::Primary);
        m.input(Input::Finished(Ok(())));
        assert_eq!(m.phase, Phase::Done);
    }

    #[test]
    fn links_and_cancel() {
        let mut m = model(None);
        assert_eq!(m.input(Input::Secondary)[0], Effect::Run { task: Task::Portable, start_with_windows: true, add_to_path: true });
        assert_eq!(m.view().headline, "STARTING…");
        assert_eq!(m.input(Input::Finished(Ok(()))), vec![Effect::Exit(EXIT_OK)]);
        let mut m = model(None);
        m.input(Input::Secondary);
        m.input(Input::Finished(Err("no".into())));
        assert_eq!(m.view().headline, "COULDN'T START");
        let mut m = model(Some("0.2.0"));
        assert!(matches!(m.input(Input::Secondary)[0], Effect::Run { task: Task::RunInstalled, .. }));
        // Update has no link.
        let mut m = model(Some("0.1.0"));
        assert_eq!(m.input(Input::Secondary), vec![]);
        assert_eq!(m.input(Input::Close), vec![Effect::Exit(EXIT_CANCELLED)]);
        assert_eq!(m.exit_code_on_close(), EXIT_CANCELLED);
    }

    #[test]
    fn uninstall_flow() {
        let mut m = SetupModel::new(SetupMode::Uninstall, "0.2.0", None, None);
        assert_eq!(m.view().headline, "UNINSTALL?");
        assert_eq!(m.input(Input::Secondary), vec![Effect::Exit(EXIT_CANCELLED)]);
        let mut m = SetupModel::new(SetupMode::Uninstall, "0.2.0", None, None);
        assert!(matches!(m.input(Input::Primary)[0], Effect::Run { task: Task::Uninstall, .. }));
        assert_eq!(m.view().headline, "UNINSTALLING…");
        m.input(Input::Finished(Err("busy".into())));
        assert_eq!(m.view().headline, "COULDN'T UNINSTALL");
        assert!(matches!(m.input(Input::Primary)[0], Effect::Run { task: Task::Uninstall, .. }));
        m.input(Input::Finished(Ok(())));
        assert_eq!(m.phase, Phase::Uninstalled);
        assert_eq!(m.input(Input::Primary), vec![Effect::Exit(EXIT_OK)]);
    }

    #[test]
    fn wording_matches_the_spec() {
        let v = model(None).view();
        assert_eq!(v.eyebrow, "ATHER MEMGUARD · V0.2.0");
        assert_eq!(v.headline, "INSTALL FOR YOU");
        assert_eq!(v.primary.as_deref(), Some("INSTALL"));
        assert_eq!(v.secondary, Some(("Run without installing".into(), SecondaryStyle::Link)));
        assert!(v.options);
        let v = model(Some("0.1.0")).view();
        assert_eq!(v.body.as_deref(), Some("Version 0.1.0 is installed. This replaces it with 0.2.0. Your settings stay."));
        let v = model(Some("0.2.0")).view();
        assert_eq!(v.body.as_deref(), Some("Version 0.2.0 is already installed. Reinstalling repairs it. Your settings stay."));
        assert_eq!(v.secondary, Some(("Just run it".into(), SecondaryStyle::Link)));
        let mut m = model(None);
        m.input(Input::Primary);
        let v = m.view();
        assert!(v.bar && v.body.is_none() && v.primary.is_none() && v.headline == "INSTALLING…");
        m.input(Input::Finished(Ok(())));
        let v = m.view();
        assert_eq!(v.headline, "READY");
        assert_eq!(v.secondary, Some(("DONE".into(), SecondaryStyle::Button)));
    }

    #[test]
    fn window_runs_tasks_through_the_backend() {
        let fake = FakeBackend::new(None);
        let calls = fake.calls.clone();
        let mut w = SetupWindow::new(Box::new(fake), SetupMode::Install, "0.2.0");
        let mut host = Host::detached((520, 600), Scale(1.0));
        w.activate(Act::TogglePath, &mut host);
        w.activate(Act::Primary, &mut host);
        assert_eq!(w.model.phase, Phase::Done);
        w.activate(Act::Primary, &mut host);
        assert_eq!(*calls.lock().unwrap(), vec!["install true false".to_string(), "open_settings".to_string()]);
        assert_eq!(w.exit_code(), Some(EXIT_OK));
        // A failing backend ends in Failed with its message.
        let mut w = SetupWindow::new(Box::new(FakeBackend::new(None).failing("Disk full.")), SetupMode::Install, "0.2.0");
        w.activate(Act::Primary, &mut host);
        assert_eq!(w.model.phase, Phase::Failed { task: Task::Install, message: "Disk full.".into() });
    }

    fn all_phases() -> Vec<SetupModel> {
        let mut out = vec![model(None), model(Some("0.1.0")), model(Some("0.2.0")), model(Some("0.3.0"))];
        let mut m = model(None);
        m.input(Input::Primary);
        out.push(m.clone());
        m.input(Input::Finished(Ok(())));
        out.push(m);
        let mut m = model(None);
        m.input(Input::Primary);
        m.input(Input::Finished(Err("Couldn't write C:\\Users\\someone\\AppData\\Local\\Programs\\AtherMemGuard\\AtherMemGuard.exe (Access is denied. (os error 5)).".into())));
        out.push(m);
        out.push(SetupModel::new(SetupMode::Uninstall, "0.2.0", None, None));
        let mut m = SetupModel::new(SetupMode::Uninstall, "0.2.0", None, None);
        m.input(Input::Primary);
        m.input(Input::Finished(Ok(())));
        out.push(m);
        out
    }

    #[test]
    fn layout_fits_and_does_not_overlap_at_two_scales() {
        for scale in [1.0, 1.5] {
            for m in all_phases() {
                let mut w = SetupWindow::new(Box::new(FakeBackend::new(None)), SetupMode::Install, "0.2.0");
                w.model = m.clone();
                let s = Scale(scale);
                let c = Canvas::new(s.s(SIZE.0 as f32), s.s(SIZE.1 as f32), s);
                w.layout(&c);
                let items = w.items().to_vec();
                let ctx = format!("scale {scale} phase {:?}", m.phase);
                for it in &items {
                    assert!(c.bounds().encloses(&it.rect), "{ctx}: {:?} outside the window", it.part);
                }
                let solid: Vec<&Item> = items.iter().filter(|i| !matches!(i.part, Part::Hairline)).collect();
                for (i, a) in solid.iter().enumerate() {
                    for b in &solid[i + 1..] {
                        assert!(!a.rect.intersects(&b.rect), "{ctx}: {:?} overlaps {:?}", a.part, b.part);
                    }
                }
                // Leave the bottom padding free.
                let bottom = items.iter().map(|i| i.rect.bottom()).max().unwrap();
                assert!(bottom <= c.height() - s.s(20.0), "{ctx}: content reaches {bottom} of {}", c.height());
            }
        }
    }
}

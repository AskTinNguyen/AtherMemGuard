//! Paint every Part D surface off-screen with fixed sample data and save PNGs for review.
//! `cargo run --example ui_snapshots -- <dir>`. Nothing here reads the live machine or
//! writes settings: the data is `FakeStore` + `sample_status()`.

use std::path::{Path, PathBuf};

use super::canvas::Canvas;
use super::settings::{SettingsHost, SettingsWindow};
use super::settings_model::UpdateState;
use super::settings_store::FakeStore;
use super::status_source::FakeStatus;
use super::theme::Scale;
use super::toast;
use super::setup::{self, FakeBackend, Input, SetupMode, SetupWindow};
use super::history::{self, FakeHistory, HistoryWindow, Range};
use super::geom::Pt;
use super::doctor::{self, DoctorWindow, FakeDoctor};
use super::window::Surface;

/// A settings window over the mockup's data.
pub fn sample_settings() -> SettingsWindow {
    SettingsWindow::new(sample_host())
}

pub fn sample_host() -> SettingsHost {
    SettingsHost {
        store: Box::new(FakeStore::mockup()),
        status: Box::new(FakeStatus::default()),
        commands: Box::new(|_| {}),
        app_version: "0.2.0".into(),
        update_state: Box::new(|| UpdateState::UpToDate { checked: "today at 14:02".into() }),
    }
}

/// Paint the settings window at `width` x `height` (96-DPI px) or, with `height == None`,
/// the whole page in one tall image.
pub fn render_settings(w: &mut SettingsWindow, scale: Scale, width: i32, height: Option<i32>) -> Canvas {
    let pw = scale.s(width as f32);
    let ph = match height {
        Some(h) => scale.s(h as f32),
        None => {
            let probe = Canvas::new(pw, scale.s(760.0), scale);
            w.page_height(&probe)
        }
    };
    let mut c = Canvas::new(pw, ph, scale);
    w.paint(&mut c);
    c
}

/// Write every snapshot into `dir` at 100% and 150%; returns the files written.
pub fn write_all(dir: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let mut out = Vec::new();
    for (scale, suffix) in [(Scale(1.0), ""), (Scale(1.5), "@1.5x")] {
        let mut save = |name: &str, c: &Canvas| -> Result<(), String> {
            let p = dir.join(format!("{}{}.png", name, suffix));
            c.save_png(&p)?;
            out.push(p);
            Ok(())
        };
        // The whole page, groups closed.
        let mut w = sample_settings();
        save("settings", &render_settings(&mut w, scale, 900, None))?;
        // The window as it opens (900 x 760).
        let mut w = sample_settings();
        save("settings-window", &render_settings(&mut w, scale, 900, Some(760)))?;
        // Browsers open, as in the mockup.
        let mut w = sample_settings();
        w.set_open_group(Some("Browsers"));
        save("settings-group-open", &render_settings(&mut w, scale, 900, None))?;
        // Searching.
        let mut w = sample_settings();
        w.set_query("cap");
        save("settings-search", &render_settings(&mut w, scale, 900, Some(760)))?;
        // The hard-cap confirmation over the window.
        let mut w = sample_settings();
        w.set_open_group(Some("Browsers"));
        w.confirm_hard_cap("Browsers", 4096.0);
        save("settings-confirm", &render_settings(&mut w, scale, 900, Some(760)))?;
        // Minimum size.
        let mut w = sample_settings();
        save("settings-min", &render_settings(&mut w, scale, 720, Some(480)))?;
        for (name, t) in toast::catalog::samples() {
            save(&format!("toast-{}", name), &toast::render(&t, scale))?;
        }
        for (name, mut w) in sample_setups() {
            save(name, &setup::render(&mut w, scale))?;
        }
        for (name, range, empty, hover) in [
            ("history-1h", Range::H1, false, false),
            ("history-6h", Range::H6, false, false),
            ("history-24h", Range::H24, false, false),
            ("history-7d", Range::D7, false, false),
            ("history-empty", Range::H6, true, false),
            ("history-hover", Range::H6, false, true),
        ] {
            let src = if empty { FakeHistory::empty() } else { FakeHistory::mockup() };
            let mut w = HistoryWindow::new(Box::new(src));
            w.set_range(range);
            if hover {
                // Over the tight spell, 72% across the plot.
                let probe = Canvas::new(scale.s(960.0), scale.s(600.0), scale);
                let plot = w.layout(&probe).plot;
                w.set_mouse(Some(Pt::new(plot.x + plot.w * 72 / 100, plot.y + plot.h / 2)));
            }
            save(name, &history::render(&mut w, scale, 960, 600))?;
        }
        // Checkup: the whole report, all clear, after a cleanup (Spotify stayed open), and the
        // "End it anyway" confirmation.
        let mut d = DoctorWindow::new(Box::new(FakeDoctor::new()));
        d.scan(None);
        save("doctor", &doctor::render(&mut d, scale, 760, None))?;
        let mut d = DoctorWindow::new(Box::new(FakeDoctor::all_clear()));
        d.scan(None);
        save("doctor-clear", &doctor::render(&mut d, scale, 760, Some(720)))?;
        let mut d = DoctorWindow::new(Box::new(FakeDoctor::new()));
        d.scan(None);
        if let Some(m) = d.model.as_mut() {
            let spotify = m.report.findings.iter().position(|f| f.id == "app:spotify").unwrap_or(0);
            m.toggle(spotify);
        }
        d.clean_up(None);
        save("doctor-results", &doctor::render(&mut d, scale, 760, None))?;
        if let Some(i) = d.model.as_ref().and_then(|m| (0..m.rows.len()).find(|&i| m.still_open(i))) {
            d.confirm = Some(doctor::Confirm::End(i));
        }
        save("doctor-confirm", &doctor::render(&mut d, scale, 760, Some(720)))?;
    }
    Ok(out)
}

/// One setup window per state, from `FakeBackend`.
pub fn sample_setups() -> Vec<(&'static str, SetupWindow)> {
    let v = "0.2.0";
    let mk = |installed: Option<&str>, mode: SetupMode| SetupWindow::new(Box::new(FakeBackend::new(installed)), mode, v);
    let mut working = mk(None, SetupMode::Install);
    working.model.input(Input::Primary);
    let mut done = mk(None, SetupMode::Install);
    done.model.input(Input::Primary);
    done.model.input(Input::Finished(Ok(())));
    let mut failed = mk(None, SetupMode::Install);
    failed.model.input(Input::Primary);
    failed.model.input(Input::Finished(Err(
        r"Couldn't write C:\Users\you\AppData\Local\Programs\AtherMemGuard\memguard.exe (Access is denied. (os error 5)).".into(),
    )));
    vec![
        ("setup-install", mk(None, SetupMode::Install)),
        ("setup-update", mk(Some("0.1.4"), SetupMode::Install)),
        ("setup-reinstall", mk(Some("0.2.0"), SetupMode::Install)),
        ("setup-working", working),
        ("setup-done", done),
        ("setup-failed", failed),
        ("setup-uninstall", mk(Some("0.2.0"), SetupMode::Uninstall)),
    ]
}

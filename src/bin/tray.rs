// AtherMemGuard.exe: the tray app, the setup exe and the updater's second half, in one file.
// No console window.
#![windows_subsystem = "windows"]

use athermemguard::install::{self, LaunchMode};
use athermemguard::ui::setup::{run_setup, RealBackend, SetupMode};
use athermemguard::{log, paths, tray, updater, win};

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// No arguments: tray or first-run setup, by `install::launch_mode()`.
    Default,
    /// Started by the updater: finish the update, then run the tray.
    AfterUpdate(u32),
    /// From Settings > Apps: the setup window's "Uninstall?" screen.
    Uninstall,
    /// Open the settings window: in the running tray if there is one, else start the tray.
    Settings,
    /// Silent install for scripts.
    Install(install::InstallOptions),
    /// Paint every UI surface with sample data into PNG files (docs/UI.md, for review).
    UiSnapshots(String),
    /// Started elevated by the tray's "Free Windows file cache": empty it, exit 0 when done.
    FlushCache,
    /// Anything else: say so in the log and do nothing.
    Unknown(String),
}

fn parse_args(args: &[String]) -> Command {
    let Some(first) = args.first() else { return Command::Default };
    let rest = &args[1..];
    match first.to_ascii_lowercase().as_str() {
        "--after-update" => match rest {
            [pid] => pid.parse().map(Command::AfterUpdate).unwrap_or_else(|_| Command::Unknown(args.join(" "))),
            _ => Command::Unknown(args.join(" ")),
        },
        "--uninstall" if rest.is_empty() => Command::Uninstall,
        "--settings" if rest.is_empty() => Command::Settings,
        "--flush-cache" if rest.is_empty() => Command::FlushCache,
        "--ui-snapshots" => match rest {
            [dir] => Command::UiSnapshots(dir.clone()),
            _ => Command::Unknown(args.join(" ")),
        },
        "--install" => {
            let mut opts = install::InstallOptions::default();
            for a in rest {
                match a.to_ascii_lowercase().as_str() {
                    "--no-path" => opts.add_to_path = false,
                    "--no-start" => opts.start = false,
                    "--no-startup" => opts.start_with_windows = false,
                    _ => return Command::Unknown(args.join(" ")),
                }
            }
            Command::Install(opts)
        }
        _ => Command::Unknown(args.join(" ")),
    }
}

fn run_install(opts: install::InstallOptions) -> i32 {
    match install::install(opts) {
        Ok(_) => 0,
        Err(e) => {
            log::write(&format!("install failed: {}", e));
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_args(&args) {
        Command::Default => match install::launch_mode() {
            LaunchMode::Tray => {
                tray::run();
                0
            }
            LaunchMode::Setup => run_setup(Box::new(RealBackend), SetupMode::Install),
        },
        Command::AfterUpdate(pid) => {
            updater::finish_update(pid);
            tray::run_after_update();
            0
        }
        Command::Uninstall => run_setup(Box::new(RealBackend), SetupMode::Uninstall),
        Command::Settings => {
            if win::instance_running(paths::mutex()) {
                if tray::ask_running_to_open_settings(std::time::Duration::from_secs(5)) {
                    0
                } else {
                    log::write("--settings: the running copy didn't answer (an older version?)");
                    1
                }
            } else {
                match install::launch_mode() {
                    LaunchMode::Tray => {
                        tray::run_and_open_settings();
                        0
                    }
                    LaunchMode::Setup => run_setup(Box::new(RealBackend), SetupMode::Install),
                }
            }
        }
        Command::Install(opts) => run_install(opts),
        Command::FlushCache => match athermemguard::ops::flush_cache_here() {
            Ok(()) => 0,
            Err(e) => {
                log::write(&format!("--flush-cache: {}", e));
                1
            }
        },
        Command::UiSnapshots(dir) => {
            let dir = std::path::Path::new(&dir);
            match athermemguard::ui::snapshots::write_all(dir) {
                Ok(files) => {
                    log::write(&format!("wrote {} UI snapshots to {}", files.len(), dir.display()));
                    0
                }
                Err(e) => {
                    log::write(&format!("UI snapshots failed: {}", e));
                    1
                }
            }
        }
        Command::Unknown(a) => {
            log::write(&format!("AtherMemGuard.exe didn't understand \"{}\"; use memguard.exe for commands", a));
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Command {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_the_command_line() {
        assert_eq!(p(&[]), Command::Default);
        assert_eq!(p(&["--after-update", "1234"]), Command::AfterUpdate(1234));
        assert_eq!(p(&["--uninstall"]), Command::Uninstall);
        assert_eq!(p(&["--UNINSTALL"]), Command::Uninstall);
        assert_eq!(p(&["--settings"]), Command::Settings);
        assert_eq!(p(&["--flush-cache"]), Command::FlushCache);
        let opts = |add_to_path, start_with_windows, start| Command::Install(install::InstallOptions { add_to_path, start_with_windows, start });
        assert_eq!(p(&["--install"]), opts(true, true, true));
        assert_eq!(p(&["--install", "--no-path"]), opts(false, true, true));
        assert_eq!(p(&["--install", "--no-start"]), opts(true, true, false));
        assert_eq!(p(&["--install", "--no-startup"]), opts(true, false, true));
        assert_eq!(p(&["--install", "--no-start", "--no-path", "--NO-STARTUP"]), opts(false, false, false));
        assert_eq!(p(&["--ui-snapshots", r"C:\shots"]), Command::UiSnapshots(r"C:\shots".into()));
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        for bad in [
            &["--after-update"][..],
            &["--after-update", "abc"],
            &["--after-update", "-1"],
            &["--after-update", "1", "2"],
            &["--uninstall", "now"],
            &["--settings", "now"],
            &["--ui-snapshots"],
            &["--ui-snapshots", "a", "b"],
            &["--install", "--add-to-path"],
            &["status"],
            &["--no-path"],
        ] {
            assert!(matches!(p(bad), Command::Unknown(_)), "{:?}", bad);
        }
    }
}

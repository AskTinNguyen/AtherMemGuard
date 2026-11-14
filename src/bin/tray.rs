// AtherMemGuard.exe: the tray app, the setup exe and the updater's second half, in one file.
// No console window.
#![windows_subsystem = "windows"]

use athermemguard::install::{self, LaunchMode};
use athermemguard::{log, tray, updater};

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// No arguments: tray or first-run setup, by `install::launch_mode()`.
    Default,
    /// Started by the updater: finish the update, then run the tray.
    AfterUpdate(u32),
    /// From Settings > Apps.
    Uninstall,
    /// Silent install for scripts.
    Install(install::InstallOptions),
    /// Paint every UI surface with sample data into PNG files (docs/UI.md, for review).
    UiSnapshots(String),
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
            // INTEGRATION: open E2's setup window (src/ui/setup.rs) here; until then install right away.
            LaunchMode::Setup => run_install(install::InstallOptions::default()),
        },
        Command::AfterUpdate(pid) => {
            updater::finish_update(pid);
            tray::run_after_update();
            0
        }
        // INTEGRATION: E2's setup window has an "UNINSTALL?" state for this; until then uninstall right away.
        Command::Uninstall => match install::uninstall() {
            Ok(()) => 0,
            Err(e) => {
                log::write(&format!("uninstall failed: {}", e));
                1
            }
        },
        Command::Install(opts) => run_install(opts),
        Command::UiSnapshots(dir) => {
            let dir = std::path::Path::new(&dir);
            // INTEGRATION: drop the flyout call once ui::snapshots::write_all includes it.
            let result = athermemguard::ui::snapshots::write_all(dir).and_then(|mut a| {
                a.extend(athermemguard::ui::flyout::snapshots(dir)?);
                Ok(a)
            });
            match result {
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

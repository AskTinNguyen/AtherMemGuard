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
    Install { add_to_path: bool, start: bool },
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
        "--install" => {
            let (mut add_to_path, mut start) = (true, true);
            for a in rest {
                match a.to_ascii_lowercase().as_str() {
                    "--no-path" => add_to_path = false,
                    "--no-start" => start = false,
                    _ => return Command::Unknown(args.join(" ")),
                }
            }
            Command::Install { add_to_path, start }
        }
        _ => Command::Unknown(args.join(" ")),
    }
}

fn run_install(add_to_path: bool, start: bool) -> i32 {
    match install::install(add_to_path, start) {
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
            // The branded setup window plugs in here later; until then install right away.
            LaunchMode::Setup => run_install(true, true),
        },
        Command::AfterUpdate(pid) => {
            updater::finish_update(pid);
            tray::run();
            0
        }
        Command::Uninstall => match install::uninstall() {
            Ok(()) => 0,
            Err(e) => {
                log::write(&format!("uninstall failed: {}", e));
                1
            }
        },
        Command::Install { add_to_path, start } => run_install(add_to_path, start),
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
        assert_eq!(p(&["--install"]), Command::Install { add_to_path: true, start: true });
        assert_eq!(p(&["--install", "--no-path"]), Command::Install { add_to_path: false, start: true });
        assert_eq!(p(&["--install", "--no-start"]), Command::Install { add_to_path: true, start: false });
        assert_eq!(p(&["--install", "--no-start", "--no-path"]), Command::Install { add_to_path: false, start: false });
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        for bad in [
            &["--after-update"][..],
            &["--after-update", "abc"],
            &["--after-update", "-1"],
            &["--after-update", "1", "2"],
            &["--uninstall", "now"],
            &["--install", "--add-to-path"],
            &["status"],
            &["--no-path"],
        ] {
            assert!(matches!(p(bad), Command::Unknown(_)), "{:?}", bad);
        }
    }
}

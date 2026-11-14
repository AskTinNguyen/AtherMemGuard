# Ather MemGuard

Keeps a Windows PC primed for agentic Unreal Engine work: the editor, local builds and cooks get
memory first, and everything else (browsers, chat apps, agent apps, launchers) gives it up.

Two small executables, no admin rights, no runtime to install:

- `AtherMemGuard.exe`: the background app. Sits in the tray, checks every 10 seconds, enforces the rules.
- `memguard.exe`: the command line for you, your scripts and your agents.

## Install

Copy both files to the PC and run:

```
memguard install --add-to-path
```

This copies them to `%LOCALAPPDATA%\Programs\AtherMemGuard`, starts the tray app at sign-in, starts it now,
adds an entry under Settings > Apps, and (with `--add-to-path`) puts `memguard` on your user PATH. `memguard uninstall` reverses all of it;
settings and the log are kept. The files are unsigned, so SmartScreen may ask once (More info → Run anyway).

## How it decides

Every interval MemGuard works out two things:

- **Mode**: what Unreal is doing. `idle`, `editor`, `build` (UnrealBuildTool is running), `cook`
  (a `-run=cook` commandlet, UnrealPak or BuildCookRun). Several can be true at once.
- **Pressure**: `normal`, `elevated` or `critical`, from free RAM and free commit charge.

Each app group in the settings has base rules plus overrides for "Unreal is running", "elevated" and
"critical". So a browser can keep 6 GB of RAM when the PC is idle, 3 GB while the editor is open, 1 GB
when memory gets tight and 256 MB when it's critical.

| Control | What it does | Risk |
|---|---|---|
| `priority` | Windows takes RAM from lower-priority apps first | None |
| `trim_above_pct` / `trim_above_mb` (soft cap) | Over the limit, the group's memory is pushed to the compressed store / page file | A short lag when you switch back to the app |
| `cpu` (throttle) | `below` / `idle` CPU priority, or `eco` (Windows efficiency mode) | App runs slower while throttled |
| `suspend` | Freezes the group until pressure drops | App is unresponsive while frozen. Off by default |
| `hard_cap_mb`, `hard_cap_per_process_mb` | Job Object limit on committed memory | The app crashes when it hits the cap. Off by default |
| `[ue] worker_throttle` | At critical pressure, pauses the newest compile/shader workers, keeps `min_workers` running, resumes them when pressure drops | Build is slower instead of failing. Off by default |

Things worth knowing:

- Unreal's own processes are on a protect list and are never touched, even if a group lists them.
- Priority and trimming free **RAM**, not **commit charge**. Only hard caps, closing apps or a bigger
  page file change commit. `memguard status` warns when the page file is small for the RAM.
- If a group climbs straight back over its soft cap, that memory is in use; MemGuard backs off (up to 16×
  the cooldown) instead of trimming it over and over.
- Browser and Electron helper processes are sandboxed and refuse to join a Job Object, so hard caps
  only partly cover those apps. The log says how many processes a cap covers.
- Exit, Pause and `memguard stop` put every app back to normal priority and resume anything frozen.

## Commands for agents and scripts

```
memguard status [--json]            what's running, pressure, per-group use, advice
memguard prime [--for build] [--kill-orphans] [--json]
                                  trim every group now; with --for, exit code 2 if the PC still
                                  lacks the headroom that workload needs
memguard wait --for cook [--prime] [--timeout 600] [--json]
memguard wait --ram-gb 24 --commit-gb 40
                                  block until there's enough headroom; exit 0 = ready, 2 = timed out
memguard run --cap-gb 48 [--per-process-cap-gb N] [--priority low] [--cpu below] -- <command...>
                                  run a command (and its children) under a hard memory cap;
                                  the command's exit code is passed through
memguard advise [--json]            suggested -MaxParallelActions for the memory available now
memguard history [--hours 1] [--json]    memory history the background app recorded
memguard config show | set | reset | export | import | team    see Settings
memguard stop | daemon | config [--check | --reset] | install | uninstall
```

Typical agent flow before a local build or cook:

```
memguard wait --for build --prime --timeout 900 && RunUAT.bat BuildCookRun ...
```

The tray app also writes `%APPDATA%\AtherMemGuard\status.json` every interval, with the same content as
`memguard status --json`, for tools that would rather read a file.

## AI agents (MCP)

`memguard mcp` is an MCP server over stdio, so Claude Code, Codex and other coding agents can call
MemGuard as tools: `memguard_status`, `memguard_prime`, `memguard_wait` and `memguard_advise`. The agent
starts it by itself; there is no port and nothing to enable. Run `memguard agent-setup` for ready-to-paste
setup, or add it to Claude Code with:

```
claude mcp add --scope user ather-memguard -- memguard mcp
```

Setup for each agent, every tool with an example, and the recommended flow before a build or cook are in
[docs/AI-AGENTS.md](docs/AI-AGENTS.md).

## Settings

Your settings are in `%APPDATA%\AtherMemGuard\settings.toml` (tray menu → Settings). Saved changes apply
within one interval. If a change has a mistake, MemGuard keeps the previous settings and reports it in the log.

**Layers.** The settings in effect are the built-in defaults, then `team.toml` in the same folder if your
team gave you one, then your `settings.toml`. Each layer only changes the keys it sets. Tables merge key by
key, and `[[group]]` entries merge by `name`: a group with a built-in name changes only the keys you set,
and a new name adds a group. A new `settings.toml` is mostly comments: it documents every key, and only
`[general] preset` is set. A 0.1 settings file you never edited is replaced by the new template the first time
0.2 runs, and the old file is kept as `settings.toml.bak`. A 0.1 file you edited stays as it is and still works,
on top of the new defaults.

**Presets.** `[general] preset = "gentle" | "balanced" | "aggressive"` scales every soft cap: ×2, ×1 or ×0.5.
Aggressive also gives every group `cpu = "below"` while Unreal is running, unless the group already has a
stronger CPU setting (`idle`, `eco`).

**Limits that scale with RAM.** A group, or its `[group.ue]`, `[group.elevated]` and `[group.critical]`
overrides, can set its soft cap as `trim_above_pct` (percent of total RAM) or as `trim_above_mb`. If one table
sets both, MB is used. A layer that sets either replaces the soft cap from the layers below it. After the preset,
a soft cap is never below 128 MB (0 still means off). The built-in groups use percentages, so the browser cap is
4.8% of RAM: about 6 GB on a 128 GB PC and 1.5 GB on a 32 GB PC. `enabled = false` makes MemGuard leave a group
alone.

New keys in 0.2:

```toml
[general]
preset = "balanced"            # gentle | balanced | aggressive
notifications = true           # toasts for pressure changes, updates and hints
auto_prime = true              # trim every group the moment a build or cook starts
prime_hotkey = "Ctrl+Alt+M"    # empty = no hotkey

[updates]
check_automatically = true
manifest_url = ""              # empty = the built-in GitHub URL

[history]
enabled = true                 # keep a memory history in history\YYYY-MM-DD.csv
keep_days = 7                  # day files this old are deleted

[ue]
editor_growth_alert_gb_per_hour = 6   # tell you when an editor keeps growing this fast; 0 = off

[[group]]
name = "Browsers"
enabled = true
trim_above_pct = 4.8
```

**Changing settings from the command line.** You can edit the file, or use these commands, which keep your
comments:

```
memguard config                      where settings.toml is
memguard config --check              check every layer and report the first problem
memguard config --reset              replace settings.toml with the template (old file kept as .bak)
memguard config show                 print the settings in effect, all layers merged
memguard config set general.preset aggressive
memguard config set group.Browsers.trim_above_pct 3
memguard config set group.Browsers.ue.trim_above_mb 2048
memguard config reset group.Browsers.trim_above_pct   go back to the team or built-in value
memguard config export my-settings.toml      the settings in effect, as one file
memguard config import my-settings.toml      check it, keep settings.toml as .bak, replace it
memguard config team team.toml               check it and install it as team.toml
```

`config set` reads the value as TOML when it can (`3`, `true`, `["chrome", "msedge"]`) and as text otherwise
(`aggressive`). It writes nothing if the result would be invalid.

**History.** The background app records memory every interval: in memory for the last 24 hours, and in a CSV
file per day in `%APPDATA%\AtherMemGuard\history`. `memguard history [--hours N] [--json]` prints it. When an
editor's committed memory keeps growing (a least-squares slope over 30 minutes, at least 10 minutes of samples)
faster than `editor_growth_alert_gb_per_hour`, MemGuard says so once for that editor process.

The `[prime]` headroom targets and the 3 GB-per-compile-action figure in `memguard advise` are starting
points, not measurements of your project. Tune them.

## Build from source

```
cargo build --release
cargo test
```

Setting `ATHER_MEMGUARD_DIR` to a folder makes MemGuard use it for settings, log and status, with its own
instance name, so a test copy can run beside the installed one.

The exe icon is the Ather mark, drawn in code (`src/logo.rs`). To regenerate `res/app.ico` after changing it:

```
cargo run --bin memguard -- write-icon res\app.ico
```

Ather MemGuard was first prototyped as MemCap; `memguard install` replaces a MemCap install and carries its settings over. The early PowerShell and C# versions are in `prototype/`.

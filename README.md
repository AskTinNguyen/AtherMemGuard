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
settings and the log are kept. The files are unsigned, so SmartScreen may ask once (More info â†’ Run anyway).

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
| `trim_above_mb` (soft cap) | Over the limit, the group's memory is pushed to the compressed store / page file | A short lag when you switch back to the app |
| `cpu` (throttle) | `below` / `idle` CPU priority, or `eco` (Windows efficiency mode) | App runs slower while throttled |
| `suspend` | Freezes the group until pressure drops | App is unresponsive while frozen. Off by default |
| `hard_cap_mb`, `hard_cap_per_process_mb` | Job Object limit on committed memory | The app crashes when it hits the cap. Off by default |
| `[ue] worker_throttle` | At critical pressure, pauses the newest compile/shader workers, keeps `min_workers` running, resumes them when pressure drops | Build is slower instead of failing. Off by default |

Things worth knowing:

- Unreal's own processes are on a protect list and are never touched, even if a group lists them.
- Priority and trimming free **RAM**, not **commit charge**. Only hard caps, closing apps or a bigger
  page file change commit. `memguard status` warns when the page file is small for the RAM.
- If a group climbs straight back over its soft cap, that memory is in use; MemGuard backs off (up to 16Ã—
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

## Checkup (doctor)

`memguard doctor` answers one question: what is using memory on this PC that Unreal work doesn't need,
and what should be done about it? It looks and explains; it changes nothing unless you ask, and what it
changes can be undone.

```
memguard doctor [--json]                 the checkup, grouped by what to do, with the memory each item holds
memguard doctor --fix [--dry-run]        do the recommended actions (the safe ones)
memguard doctor --fix app:steam startup:hkcu:Steam:disable_startup
                                         do specific ones: <id>[:action]
memguard doctor --fix app:steam:end --force
                                         end an app that ignored "close" (anything unsaved is lost)
memguard doctor --undo [<undo id>]       reverse the last cleanup, or one action
```

What it reports:

- **Not needed for Unreal** (`close`): game launchers, Spotify, Widgets, Copilot, Phone Link, lighting and
  peripheral suites, GPU overlays and similar, from a built-in catalogue of over 150 process names.
- **Starts with Windows**: Run-key and Startup-folder entries for those apps. Yours can be switched off the
  way Task Manager does it; machine-wide ones need an administrator and are only reported.
- **Big tools you might not be using** (`heavy_tool`): Blender, Maya, Photoshop, Docker, WSL, local AI
  models, other game engines. Only ever "close if you're not using it".
- **Worth a look** (`review`): apps the catalogue doesn't know that hold more than 1 GB, with their file
  description and company so you can decide.
- **Held back, not closed** (`hold_back`): browsers, chat and agent apps you want, which aren't in a
  MemGuard group yet.
- **Unreal**: leftover helper processes from a closed editor, several editors open, an editor past its
  restart hint. **This PC**: small page file, low commit, a leaking driver, a git pile-up, MemGuard not
  running.

Actions: `close` asks the app to close (as if you clicked its X) and waits up to 10 seconds; an app with
unsaved work shows its own prompt and the result is "still open", never a forced kill. `end` needs
`--force`. `disable_startup` switches a startup entry off. `hold_back` adds the app to a MemGuard group,
`block_while_unreal` to `[doctor] close_while_unreal` (closed whenever the editor, a build or a cook runs),
`leave_it` to `[doctor] keep`. Only two things are ever recommended: switching off startup entries of apps
Unreal doesn't need, and closing such apps when they have no window open. Closing anything with a window
is always your pick.

It never touches Unreal's processes, Windows itself, security software, other users' or sessions'
processes, Ather MemGuard, or the app that started `memguard` (so an agent running `memguard doctor --fix`
can't close itself). Every applied action goes to `doctor-undo.json` in the data folder.

Settings, in `settings.toml`:

```toml
[doctor]
keep = ["MyTool"]               # never proposed
close = ["SomeLauncher*"]       # treat as not needed for Unreal
close_while_unreal = []         # closed whenever Unreal is running
ignore_below_mb = 150           # leave out apps using less than this
review_above_mb = 1024          # show unknown apps above this
nudge_above_gb = 4              # the tray suggests a checkup when Unreal starts and this much could be freed
```

Agents get the same report from the `memguard_doctor` MCP tool, which is read-only: they relay it and ask
you before anything is fixed.

## Settings

`%APPDATA%\AtherMemGuard\settings.toml` (tray menu â†’ Settings). Saved changes apply within one
interval; a mistake keeps the previous settings and is reported in the log. `memguard config --check`
validates the file. The default file documents every key.

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

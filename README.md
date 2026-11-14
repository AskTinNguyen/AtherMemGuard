# Ather MemGuard

Keeps a Windows PC primed for agentic Unreal Engine work: the editor, local builds and cooks get
memory first, and everything else (browsers, chat apps, agent apps, launchers) gives it up.

**Download:** [the latest release](https://github.com/AskTinNguyen/AtherMemGuard/releases/latest) has
`AtherMemGuard-Setup-<version>.exe`. What changed in each version is in [RELEASE_NOTES.md](RELEASE_NOTES.md).

What it does, in short:

- **Holds other apps back** so Unreal gets the memory: soft caps that tighten while the editor runs and as
  memory gets tight, with presets and team defaults. See [How it decides](#how-it-decides).
- **Frees memory when a build or cook starts**, and on demand from the tray or a shortcut.
- **Checkup:** finds apps Unreal work doesn't need and offers to close them, stop them starting with
  Windows, or keep them closed while Unreal runs. Everything it does can be undone.
- **RAM and BIOS check:** tells you if your RAM runs slower than it was sold to run, with the BIOS steps.
- **Memory history** and an alert when the editor keeps growing.
- **For agents:** a command line and an MCP server, so an agent can wait for headroom before a build or cook.

Two small programs, no admin rights, no runtime to install:

- `AtherMemGuard.exe`: the background app. Sits in the tray, checks every 10 seconds, enforces the rules.
  The same file is the setup program.
- `memguard.exe`: the command line for you, your scripts and your agents (and their MCP server).

## Install

Download **`AtherMemGuard-Setup-<version>.exe`** and run it. The setup window offers two switches, both on:
**Start with Windows** and **Add the memguard command**. Click **INSTALL**. It installs just for your Windows
account, with no admin rights:

- both programs go to `%LOCALAPPDATA%\Programs\AtherMemGuard`
- the tray app starts now (and at sign-in, if you left that on)
- a Start menu shortcut and an entry in Settings › Apps
- `memguard` on your user PATH (new terminals find it)

**Run without installing** on the same window writes an `AtherMemGuard.portable` file next to the exe and
runs it from there. Running a newer setup exe updates an installed copy; running the same version again
repairs it. Your settings are kept either way. A copy of 0.1 or MemCap is stopped and replaced, and its
settings carry over.

The exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.

For scripts, `AtherMemGuard-Setup-<version>.exe --install` installs without a window; add `--no-path`,
`--no-startup` (don't start at sign-in) or `--no-start` (don't start it now). `memguard install
[--add-to-path] [--no-startup] [--no-start]` does the same from the command line.

**Uninstall** from Settings › Apps › Ather MemGuard › Uninstall (or `memguard uninstall`). Every app goes back
to normal. Settings, history and the log stay in `%APPDATA%\AtherMemGuard` in case you come back.

[INSTALL.md](INSTALL.md) is the short version for teammates.

## The tray

The tray icon is the Ather mark: an amber dot when memory is tight, red when it's critical, faded while
paused. Its tooltip says the Unreal mode, the pressure and RAM / commit free.

- **Left-click** opens the flyout: memory is fine / tight / critical and where every GB of RAM in use is.
  Unreal (editor, compile workers), other apps with the biggest named, each app group, Windows' file cache,
  Windows and drivers, and what Windows squeezed or can't attribute add up to the total. When memory is
  critical it says which apps to close and what that gets back. When Windows' file cache holds 1 GB or more,
  "Empty it now" frees it (Windows asks for administrator permission each time). Commit free sits at the bottom. Then the buttons
  **Free memory**, **Pause 30 min** (or **Resume**) and **Settings**. "Run a checkup" opens the checkup.
- **Right-click** opens the menu: Free memory now, Empty Windows file cache, Pause for 30 minutes, Pause until I resume, Settings,
  Memory history, Run a checkup, Check for updates, Run at Windows startup, Quit.
- **Double-click** opens Settings.
- **Ctrl+Alt+M** (change it in Settings) frees memory from anywhere. If another app already uses that
  shortcut, a toast says so once and the shortcut stays off until you pick another.

Toasts appear bottom right, only when something changed that you'd want to know: memory became critical (and
fine again), memory was freed for a build or cook, the editor is big or growing fast, an update is ready, a
checkup found a lot to free. Turn them off with **Notifications** in Settings.

Pausing puts every app back to normal until you resume (or for 30 minutes). Quit and `memguard stop` do the
same and stop the app.

## The settings window

Settings (tray menu, flyout, or `AtherMemGuard.exe --settings`) shows every setting with a plain description.
Changes apply as you make them; there is no Save button. A lime dot marks what you changed, a TEAM tag what
comes from your team's file, and right-click resets a setting. Type anywhere to search.

- **General**: how hard to push other apps (Gentle / Balanced / Aggressive), free memory when a build or cook
  starts, run at Windows startup, notifications, the Free memory shortcut.
- **App groups**: one card per group (Browsers, Chat, Agents, Background, and your own) with its apps, a soft cap
  and speed for each situation (always, Unreal running, memory tight, memory critical), and the risky
  options, freeze and hard cap, labelled in red and off by default.
- **Unreal**, **Memory pressure**, **Headroom**, **History**, **Updates**.
- **AI agents**: **Copy setup** puts ready-to-paste agent setup on the clipboard.
- **Team and backup**: **Load team file…**, **Export…** / **Import…** your settings, **Open file**,
  **Reset all settings**.

## Presets and team defaults

**Presets** set how hard MemGuard pushes: Balanced is the default; Gentle doubles every soft cap; Aggressive
halves them and slows other apps while Unreal runs. Soft caps are a share of your RAM, so the same settings
suit a 32 GB and a 128 GB PC.

**Team defaults**: one person tunes the settings, exports them (or writes a `team.toml`), and everyone else
loads that file with **Load team file…** (or `memguard config team team.toml`). It sits under each person's own
settings: what you changed yourself still wins, everything else follows the team. Details under
[Settings](#settings).

## Checkup

**Run a checkup** (tray menu or flyout) opens a window that answers: what is using memory on this PC that
Unreal work doesn't need? It shows the total that could be freed, then sections: apps not needed for Unreal,
apps that start with Windows, big tools you might not be using, unknown apps worth a look, apps that should be
held back, Unreal leftovers, and notes about this PC. Tick what you want, pick what happens to each (close,
don't start with Windows, close whenever Unreal is running, hold back, leave it) and click **CLEAN UP**.
Nothing changes without a tick and that click. Apps that ask to save stay open and offer **End it anyway**,
which asks first. Ending anything always asks first: if a ticked row ends an app or Unreal's leftover
helpers, **CLEAN UP** lists them and waits for **End them**. **Undo last cleanup** reverses it.

When Unreal work starts and a checkup finds at least 4 GB to free, a toast says so (at most once a day), with a
**Review** button. Apps you put on the "close whenever Unreal is running" list are asked to close when they
start during Unreal work; the toast has **Allow this time**.

The same checkup is on the command line as `memguard doctor`; see [Checkup (doctor)](#checkup-doctor).

## RAM and BIOS check

Memory kits are sold with a fast profile (XMP on Intel, EXPO or DOCP on AMD) that stays off until someone
turns it on in the BIOS, so many PCs run for years at the slower default. For Unreal work that costs compile,
shader and cook time. Ather MemGuard reads the PC's firmware tables and tells you:

- whether the RAM runs below the speed the kit is rated for (worked out from the sticks' part numbers, or
  what the modules report), or is already as fast as it's made to run;
- whether the sticks are in the right slots (two sticks on one memory channel halve the bandwidth);
- whether the BIOS may be out of date, and the cautions for your CPU (old microcode on Intel Core 13th and
  14th gen, safe System Agent and SoC voltages).

It only reports. It never changes the BIOS, the registry or a driver, and needs no admin rights: you make
the change yourself, following steps written for your board's BIOS, with this PC's numbers filled in, what
to do if it doesn't start, and how to check it worked.

- `memguard ram` prints the report and the steps (`--json` for scripts, `--html <file>` for the page).
  Agents get the same from the read-only `memguard_ram` MCP tool.
- **Toast:** about three minutes after the tray starts, if the RAM runs below its rated speed, a toast says
  so once, with **Show me how**. It speaks again only when the situation changes (different sticks, speed
  or BIOS version). A problem that isn't about speed, such as old Intel microcode, gets a toast of its own.
- **Checkup:** the same shows as a row under **This PC**, with **Show me how**, and in `memguard doctor`.
- **Show me how** writes the report page to `%APPDATA%\AtherMemGuard\ram-report.html` and opens it in your
  browser.
- Settings › General › **Check memory speed** turns all of it off (`[general] check_ram_speed = false`).
  `memguard ram` still works.

## Disk and Dev Drive

A cook that runs out of disk fails late, and on a Dev Drive the usual folder sizes are wrong several times
over: lanes, worktrees and the git LFS store share the same clusters (block clones), so Explorer counts
them again and again. On the team PC, files on D: add up to 4.5 TB on a 1.9 TB drive. Ather MemGuard
watches free space, checks the Dev Drive is set up to be fast, and counts what could go without counting
anything twice. It only reports: it never deletes or moves files, and needs no admin rights.

- **Watching:** every 60 seconds the tray reads free space on Dev Drives, the system drive (page file) and
  drives holding projects, Unreal's DDC or Zen, or TEMP; each drive is one row however it's reached. A
  drive is **getting full** below the smaller of 150 GB and 10% free, and **almost full** below the smaller
  of 50 GB and 5%. A toast says so once per change ("Your Dev Drive is almost full. D: has 38 GB free, and
  a cook usually needs 100 GB."), with 5 GB of slack before it changes back. The flyout shows one line,
  "D: Dev Drive · 469 GB free".
- **Dev Drive health:** whether the Dev Drive is trusted, whether Defender's performance mode is on,
  whether antivirus filters are allowed on Dev Drives, whether big caches sit on C:, and another antivirus.
  Fixes that need an administrator are shown with the exact command and how to check it worked; Ather
  MemGuard doesn't run them. These rows also show under **This PC** in the checkup.
- **Before a cook:** `memguard wait --for cook --disk-path D:\s2` returns at once with exit code 2 when the
  project's drive has less than `disk_gb` free (or the DDC, Zen or TEMP drive less than
  `cache_min_free_gb`), because disk space doesn't free itself.
- **Disk checkup:** `memguard disk checkup [--path D:\s2]` lists build files, caches, logs, lanes and
  worktrees, each with what it holds and what removing it would free, counted from the drive's own
  reference counts, with how far to trust each number (exact, at least, about, at most). Totals exist only
  for a named set, counted once. `memguard disk plan d7 d9` counts exactly those again, fresh. Counting
  runs in a separate low-priority process, a few threads at a time, for at most a minute by default, and
  stops if a build or cook starts.
- **"Frees" is predicted.** It comes from the drive's reference counts and has not been checked against a
  real delete. On ReFS (Dev Drives), deleted space has been seen to come back much later, and in bulk:
  **deleting on this drive may take a long time to show up as free space.**

## Memory history

**Memory history** (tray menu) charts the last 1 hour, 6 hours, 24 hours or 7 days: Unreal's committed
memory, RAM free and commit free, with tight and critical spells shaded, dots where memory was freed and a line
where an editor started. Cards under it show the editor and each group, now and at its peak. The data is a CSV
file per day in `%APPDATA%\AtherMemGuard\history`, kept 7 days by default.

## Updates

The app checks for a new version a minute after it starts and then once a day (Settings › Updates; **Check
now** checks at once). When there is one, a toast offers it; clicking it downloads the new version, checks its
size and SHA-256, swaps it in and restarts. Your settings stay. Nothing is installed if the download doesn't
match.

Updates download from this repository's public GitHub releases (the manifest is
`releases/latest/download/latest.json`), so any PC that can reach github.com gets them. Only the version
check is sent. If GitHub can't be reached the check fails quietly and the app keeps running the version it
has. Teams that host builds themselves can point `[updates] manifest_url` at their own `latest.json`.

## Known limits

- **Not code-signed.** SmartScreen warns once per download.
- **Updates need github.com.** A PC that can't reach it keeps the version it has. See [Updates](#updates).
- **Soft caps free RAM, not commit.** If Windows runs out of commit (apps fail with RAM still free), the fix
  is a larger page file; the checkup says when yours is small.
- **Checkup:** startup tasks of Microsoft Store apps aren't listed. A background app with no window may
  ignore the request to close; the result then says "still open". An app on the "close whenever Unreal is
  running" list is closed even if it is a console program.
- **RAM and BIOS check:** it recognises the common kits from G.Skill, Corsair, Kingston, Crucial, TeamGroup,
  ADATA, Patriot, Samsung, SK hynix and Micron; anything else is reported as "couldn't tell", never guessed.
  It can't know the newest BIOS version, only how old yours is. BIOS menu names differ between board
  models, so treat the steps as a guide to where the setting lives.
- **Silent install from scripts:** `--install` starts the tray app, which keeps running. A script that
  waits for the whole process tree (PowerShell's `Start-Process -Wait`) will wait forever; wait on the
  installer process itself, or pass `--no-start`.

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
memguard wait --for cook --disk-path D:\s2 [--disk-gb 100]
                                  also check the project's drive (and the DDC, Zen, TEMP drives);
                                  too little disk returns at once with exit 2, blocked_by ["disk"]
memguard disk [--json] [--refresh]  drives, how full they are, Dev Drive health (changes nothing)
memguard disk checkup [--path D:\s2] [--refresh] [--budget-sec 60] [--json]
                                  what could go and what removing it frees (predicted; deletes nothing)
memguard disk plan d7 d9 [--json]   fresh numbers for exactly these checkup items
memguard run --cap-gb 48 [--per-process-cap-gb N] [--priority low] [--cpu below] -- <command...>
                                  run a command (and its children) under a hard memory cap;
                                  the command's exit code is passed through
memguard advise [--json]            suggested -MaxParallelActions for the memory available now
memguard flush-cache [--json]       empty Windows' file cache (asks for admin permission; exit 3 = declined)
memguard history [--hours 1] [--json]    memory history the background app recorded
memguard doctor [--json] [--fix ...] [--undo]   the checkup; see Checkup (doctor)
memguard ram [--json] [--html <file>]    is the RAM running at its rated speed, and the BIOS steps if not
memguard mcp | agent-setup          MCP server for agents, and setup text to paste; see AI agents
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
MemGuard as tools: `memguard_status`, `memguard_prime`, `memguard_wait`, `memguard_advise`,
`memguard_history`, `memguard_doctor`, `memguard_ram`, `memguard_disk`, `memguard_disk_checkup` and
`memguard_disk_plan`. Only `memguard_prime` and `memguard_wait` with `prime` change anything (they free
memory); the rest are read-only, and no tool deletes files. Agents are told to quote disk "frees" numbers
as predicted and never to promise space right after a delete. The agent
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
  peripheral suites, GPU overlays and similar, from a built-in catalogue of 142 apps.
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
  running, RAM running below its rated speed (see [RAM and BIOS check](#ram-and-bios-check); run
  `memguard ram` for the steps).

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

The settings window writes `%APPDATA%\AtherMemGuard\settings.toml` (Settings › Team and backup › Open file).
You can also edit it by hand; saved changes apply within one interval. If a change has a mistake, MemGuard keeps
the previous settings and says so in a toast and in the log.

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
check_ram_speed = true         # say once if the RAM runs below its rated speed; see RAM and BIOS check

[updates]
check_automatically = true
manifest_url = ""              # empty = the built-in GitHub URL

[history]
enabled = true                 # keep a memory history in history\YYYY-MM-DD.csv
keep_days = 7                  # day files this old are deleted

[ue]
editor_growth_alert_gb_per_hour = 6   # tell you when an editor keeps growing this fast; 0 = off

[prime]
cook = { ram_gb = 32, commit_gb = 48, disk_gb = 100 }   # disk_gb: free disk `memguard wait` wants

[disk]
enabled = true                 # watch free space; see Disk and Dev Drive
watch = ["auto"]               # Dev Drives, system drive, drives with projects, DDC/Zen, TEMP
elevated_free_gb = 150         # a level applies below the smaller of the GB and the percent figure
elevated_free_pct = 10
critical_free_gb = 50
critical_free_pct = 5
cache_min_free_gb = 20         # what `memguard wait` wants free where the DDC, Zen and TEMP are
project_dirs = []              # where the disk checkup looks for Unreal projects
lane_dirs = []                 # extra folders holding lanes or worktrees
scan_budget_sec = 60           # how long a disk checkup may count
scan_threads = 4
fresh_sec = 300                # older counts are reported as "about"
check_dev_drive = true         # Dev Drive health rows

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

Rust stable with the MSVC toolchain.

```
cargo build --release
cargo test
```

The build writes an `AtherMemGuard.portable` marker into `target\<profile>`, so `target\release\AtherMemGuard.exe`
runs as the tray app from there instead of offering to install. A copy of the exe anywhere else opens the setup
window.

Setting `ATHER_MEMGUARD_DIR` to a folder makes MemGuard use it for settings, log, status and history, with its
own instance name, so a test copy can run beside the installed one. `ATHER_MEMGUARD_UPDATE_URL` points the update
check somewhere else (tests).

One doctor test writes a throwaway value to your HKCU Run key and removes it again; it only runs with
`MEMGUARD_TEST_HKCU=1` (CI sets it).

**UI snapshots.** `cargo run --example ui_snapshots -- docs/design/snapshots` (or `AtherMemGuard.exe
--ui-snapshots <dir>`) paints every window, the flyout and every toast with fixed sample data into PNG files, at
100% and 150%. Look at them after changing any UI. `cargo run --example ui_demo` opens the settings window and
some toasts on fake data.

## Package a release

```
.\package.ps1
```

builds the CLI, then `AtherMemGuard.exe` with the CLI embedded (feature `embed-cli`), and writes `dist\`:

- `AtherMemGuard-Setup-<version>.exe`: the one file teammates download
- `AtherMemGuard-<version>-portable.zip`: both programs, the portable marker, README and INSTALL
- `SHA256SUMS.txt` and `latest.json` (the update manifest: version, URL, SHA-256, size, `UPDATE_NOTES`)

The version comes from `Cargo.toml`. `.\package.ps1 -Publish` also creates or updates the GitHub release
`v<version>` with `RELEASE_NOTES.md` as its text and copies `latest.json` to `downloads\latest.json`; running
copies find the update on their next check. Set `SIGN_PFX` and `SIGN_PASSWORD` to code-sign both programs.

The exe icon is the Ather mark, drawn in code (`src/logo.rs`). To regenerate `res/app.ico` after changing it:

```
cargo run --bin memguard -- write-icon res\app.ico
```

Ather MemGuard was first prototyped as MemCap; `memguard install` replaces a MemCap install and carries its settings over. The early PowerShell and C# versions are in `prototype/`.

# Ather MemGuard: design for 0.2

This is the contract for the 0.2 work. Several people build parts of it in parallel, each in
their own git worktree, so **file ownership matters**: only edit the files your part owns, plus
the small shared edits listed for it. Everything else is merged by the integrator.

Read `README.md` first for what the app does today, then `src/lib.rs` for the module map.

## Ground rules for every builder

- Rust stable, MSVC. Cargo is at `%USERPROFILE%\.cargo\bin` and may not be on PATH:
  in PowerShell run `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"` first.
- `cargo build` and `cargo test` must pass with no warnings before you finish.
- All `unsafe` Win32 code stays in `win.rs`, `tray.rs`, `logo.rs`, `net.rs`, `doctor_win.rs`,
  `smbios.rs`, `startup.rs`, `disk_win.rs` and `ui/`. (`disk_win.rs`: volume and file reads, the
  FSCTLs, the read-only scanner, and since Phase 2 the `WM_SETTINGCHANGE` broadcast after a cache
  relocation. `disk_clean.rs` and `disk_relocate.rs` have no `unsafe`: deletes and moves use `std::fs`,
  registry writes use `winreg`.)
  Use `windows-sys` 0.59 (add features to `Cargo.toml` as needed). New crates need a reason;
  the ones already agreed are listed per part.
- **A copy of the app is installed and running on this PC. Do not disturb it.** Never run
  `memguard install`, `uninstall` or `stop` without `ATHER_MEMGUARD_DIR` set, never write to
  `HKCU`, the user PATH, `%APPDATA%\AtherMemGuard` or `%LOCALAPPDATA%\Programs\AtherMemGuard`.
  For anything that runs the engine or reads settings, set `ATHER_MEMGUARD_DIR` to a temp
  folder: that gives the test copy its own data folder and instance name. Code that writes to
  the registry or installs must be unit-tested on its pure parts only.
- Never touch the user's real processes in tests (no trimming, suspending or capping of
  anything you didn't start yourself). Start `ping -t 127.0.0.1` or similar as a test target.
- UI wording follows the other Ather apps: short, plain, second person, sentence case.
  Say what happens and what stays safe. Product name in text is "Ather MemGuard".
- Commit your work on your worktree's branch with a clear message. Do not push, do not merge.
  Finish with a short report: branch name, what works, what you verified and how, what is
  not done, and any contract in this document you had to change.

## What exists (0.1)

| Module | Role |
|---|---|
| `win.rs` | Safe wrappers: process list, memory info, priority, trim, suspend, Job Objects, instance mutex, events |
| `config.rs` | `Config` from `settings.toml` (serde, `deny_unknown_fields`), `DEFAULT_TOML`, `name_matches` |
| `engine.rs` | `snapshot()` -> `analyze()` (pure, returns `Status`) -> `Engine::apply`. `Engine::tick()` does all three and writes `status.json` |
| `ops.rs` | `prime`, `wait_for`, `run`, `advise` |
| `install.rs` | Per-user install, autostart, PATH, Settings > Apps entry, MemCap migration |
| `logo.rs` | The Ather mark drawn with GDI; `create_icon`, `write_ico` |
| `tray.rs` | Tray icon, menu, 1-second timer driving the engine, balloons |
| `paths.rs` | Names and folders. `ATHER_MEMGUARD_DIR` overrides the data folder |
| `bin/memguard.rs` | Console CLI (clap) |
| `bin/tray.rs` | GUI exe `AtherMemGuard.exe`, calls `tray::run()` |

## 0.2 at a glance

1. **Settings people can use**: presets, limits that scale with RAM, team defaults, import and
   export, and a settings window (Part B for the data, Part D for the window).
2. **Auto-update and one setup exe**, modelled on AtherScreenshot (Part A).
3. **Auto-prime** when a build or cook starts, **memory history** and an **editor growth
   alert**, **pause for 30 minutes** (Part B).
4. **MCP server** so agents can call MemGuard as tools (Part C).
5. **Branded toasts, settings window** (Part D); **setup window, history window, prime hotkey,
   tray wiring** (Part E).
6. **Release pipeline**: package script, release notes, CI (Part A).

---

## Part A: updater, single setup exe, release pipeline

**Owns:** `src/updater.rs` (new), `src/net.rs` (new), `src/install.rs`, `src/bin/tray.rs`,
`build.rs`, `package.ps1` (new), `.github/workflows/check.yml` (new), `RELEASE_NOTES.md` (new),
`INSTALL.md` (new), `downloads/latest.json` (new).
**Shared edits:** `Cargo.toml` (deps, features), `src/lib.rs` (`pub mod`), `src/paths.rs`.
**Agreed crates:** `sha2`. HTTP is WinHTTP through `windows-sys` (system proxy, no TLS crate).

### Single setup exe

Teammates download one file, `AtherMemGuard-Setup-<ver>.exe`, which **is** the app.

- `AtherMemGuard.exe` (GUI subsystem) embeds the console CLI: cargo feature `embed-cli` makes
  `src/install.rs` do `include_bytes!(env!("MEMGUARD_CLI"))`. Without the feature (dev builds)
  install copies `memguard.exe` from next to the running exe, as today.
- `install::install` writes `AtherMemGuard.exe` (a copy of the running exe) and `memguard.exe`
  (embedded or copied) into the install folder. Keep everything else it does today.
- `src/bin/tray.rs` becomes the entry point for every GUI-exe mode. Parse `std::env::args`:
  - no args, running from the install folder or a folder containing a file named
    `AtherMemGuard.portable`: run the tray (`tray::run()`).
  - no args, running from anywhere else: first-run setup, the branded setup window
    (`ui::setup::run_setup`). The decision is `install::launch_mode() -> LaunchMode { Tray, Setup }`.
  - `--after-update <pid>`: finish an update (below), then run the tray.
  - `--uninstall`: the setup window's "Uninstall?" screen (Settings > Apps calls this;
    `UninstallString` is `"<AtherMemGuard.exe>" --uninstall`).
  - `--install [--no-path] [--no-startup] [--no-start]`: silent install, for scripts
    (`install::install(InstallOptions { add_to_path, start_with_windows, start })`).
  - `--settings`: open the settings window in the running tray (a named event it polls), or
    start the tray with it open.
- Add a Start menu shortcut `Ather MemGuard.lnk` on install (IShellLink through `windows-sys`
  COM, or a small PowerShell-free implementation) and remove it on uninstall.

### Updater (`src/updater.rs`)

Mirror AtherScreenshot's design (`C:\AtherScreenshot\src\updater.cpp` is the reference; read it).

```rust
pub struct Manifest { pub version: String, pub url: String, pub sha256: String, pub size: u64, pub notes: String }
pub enum CheckResult { UpToDate, Available(Manifest), Failed(String) }
pub fn manifest_url() -> String;                       // setting, else default, else ATHER_MEMGUARD_UPDATE_URL
pub fn parse_manifest(json: &str) -> Result<Manifest, String>;   // the "windows" entry; validation below
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering; // dotted numbers, "1.0" == "1.0.0"
pub fn check() -> CheckResult;                          // blocking; call from a worker thread
pub fn download(m: &Manifest, progress: &dyn Fn(u64, u64)) -> Result<std::path::PathBuf, String>; // to <exe>.update, verified
pub fn install_staged(staged: &std::path::Path) -> Result<(), String>; // swap + relaunch with --after-update <pid>
pub fn finish_update(old_pid: u32);                     // wait for old pid, clean up .old/.update, re-extract CLI, refresh Apps entry
```

- Default manifest URL:
  `https://github.com/AskTinNguyen/AtherMemGuard/releases/latest/download/latest.json`.
  `[updates] manifest_url` in settings overrides it (Part B adds the key; read it through
  `Config`). `ATHER_MEMGUARD_UPDATE_URL` overrides both, and only then is `http://localhost`
  or `http://127.0.0.1` allowed (for tests).
- Validation: manifest at most 64 KB; `version` digits and dots, at most 20 chars; `sha256`
  exactly 64 hex chars; `size` between 1 byte and 64 MB; `url` HTTPS on `github.com` or
  `objects.githubusercontent.com` unless the test override is active; `notes` cut to 300 chars.
- Download to `<exe>.update` beside the running exe; verify exact size and SHA-256.
- Swap: rename the running exe to `<exe>.old`, move `.update` into place, roll back on failure.
  Start `"<exe>" --after-update <pid>`, then the caller quits the tray.
- The updater has no UI. Part E shows the toasts and menu items and calls these functions.
- Unit tests for `parse_manifest`, `compare_versions`, URL validation. An integration test
  with a local HTTP server (PowerShell `HttpListener` or a tiny Rust test server) serving a
  manifest and a fake exe, run against a **temp copy** of the built exe.

### Release pipeline

- `package.ps1`: build the CLI (release), then the GUI with `--features embed-cli` and
  `MEMGUARD_CLI` set; optional `signtool` when `$env:SIGN_PFX` is set (sign the CLI before
  embedding, then the setup exe); write to `dist\`: `AtherMemGuard-Setup-<ver>.exe`,
  `AtherMemGuard-<ver>-portable.zip` (exe, `memguard.exe`, `AtherMemGuard.portable`, README,
  INSTALL), `SHA256SUMS.txt`, `latest.json` (schema below, `notes` from `$env:UPDATE_NOTES`).
  `package.ps1 -Publish` runs `gh release create v<ver> ... --notes-file RELEASE_NOTES.md
  --latest` (or upload `--clobber` when the release exists). Do not run `-Publish`.
- `latest.json`: `{ "windows": { "version", "url", "sha256", "size", "notes" } }`.
- `.github/workflows/check.yml`: on push and pull request, `windows-latest`, `cargo build` and
  `cargo test`.
- `RELEASE_NOTES.md` and `INSTALL.md` in AtherScreenshot's format (read theirs).

---

## Part B: settings data, engine behaviour, history

**Owns:** `src/config.rs`, `src/engine.rs`, `src/ops.rs`, `src/history.rs` (new).
**Shared edits:** `Cargo.toml`, `src/lib.rs`, `src/bin/memguard.rs` (the `config` subcommands
and a `history` subcommand only), `README.md` (settings section).
**Agreed crates:** `toml_edit` (already in the tree through `toml`).

### Settings schema v2

Keep every existing key working. Add:

```toml
[general]
preset = "balanced"            # gentle | balanced | aggressive
notifications = true           # toasts for pressure changes, updates, hints
auto_prime = true              # free RAM the moment a build or cook starts
prime_hotkey = "Ctrl+Alt+M"    # empty string = no hotkey

[updates]
check_automatically = true
manifest_url = ""              # empty = the built-in GitHub URL

[history]
enabled = true
keep_days = 7

[ue]
editor_growth_alert_gb_per_hour = 6   # 0 = off
```

- **Limits that scale with RAM.** Groups and their overrides accept `trim_above_pct`
  (percent of total RAM, float) as an alternative to `trim_above_mb`. If both are set, MB
  wins. Change the built-in defaults to percentages so a 32 GB PC and a 128 GB PC both get
  sensible limits (today's MB values were tuned on 128 GB: 6144 MB is 4.8%). Resolved limits
  have a floor of 128 MB. `Effective.trim_above_mb` stays the resolved MB value, so nothing
  downstream changes.
- **Presets** scale every resolved soft cap: gentle x2, balanced x1, aggressive x0.5.
  Aggressive also applies `cpu = "below"` to every group while Unreal is running.
- **Layering.** Effective settings = built-in defaults, then `team.toml` in the data folder if
  it exists, then `settings.toml`. Merge at the TOML level (tables deep-merge; `[[group]]`
  entries merge by `name`, new names append). A fresh `settings.toml` is therefore mostly
  comments: write a new template that documents every key but leaves them commented out,
  except `[general] preset`.
  Migrate existing 0.1 files: if `settings.toml` parses and is byte-identical to a known 0.1
  default (keep the two 0.1 default texts as test fixtures), replace it with the new template
  and keep the old one as `settings.toml.bak`. Otherwise leave it alone; it layers on top.
- **Editing API for the settings window** (comment-preserving, via `toml_edit`):

```rust
pub enum Layer { Builtin, Team, User }
pub struct Resolved<T> { pub value: T, pub from: Layer }      // which layer a value came from
pub fn load_layered() -> Result<(Config, /*warnings*/ Vec<String>), String>;
pub fn set_user_value(path: &str, value: toml_edit::Item) -> Result<(), String>;   // e.g. "general.preset", "group.Browsers.trim_above_pct", "group.Browsers.ue.trim_above_pct"
pub fn reset_user_value(path: &str) -> Result<(), String>;    // remove the override, falling back to team/builtin
pub fn value_layer(path: &str) -> Layer;
pub fn add_group(name: &str, processes: &[String]) -> Result<(), String>;
pub fn remove_group(name: &str) -> Result<(), String>;        // user-layer groups only; built-in groups get `enabled = false`
pub fn export_to(path: &Path) -> Result<(), String>;          // the merged, effective settings as one TOML file
pub fn import_from(path: &Path) -> Result<(), String>;        // validate, back up settings.toml, replace it
pub fn install_team_file(path: &Path) -> Result<(), String>;  // validate and copy to team.toml
```

  Add `enabled = true` to groups (false = ignored). Every setter validates by re-parsing the
  result before writing, and writes atomically (temp file + rename).
- CLI: `memguard config show` (effective settings as TOML), `config export <file>`,
  `config import <file>`, `config team <file>`, `config set <path> <value>`,
  `config reset <path>`, alongside the existing `--check` / `--reset` flags.

### Engine

- **Auto-prime.** When `activities.build` or `activities.cook` goes from false to true and
  `general.auto_prime` is on, trim every group once (reuse the logic in `ops::prime`, without
  its 1.5 s sleep) and log it.
- **Pause for a while.** `Engine::pause_for(Duration)` and `Engine::pause_until() ->
  Option<Instant>`; resumes by itself. `set_paused(true)` stays "until resumed".
- **Events for the UI.** `Engine::tick()` keeps returning `Status`; add
  `pub fn take_events(&mut self) -> Vec<Event>` with

```rust
pub enum Event {
    PressureChanged { from: Level, to: Level },
    AutoPrimed { reason: &'static str /* "build" | "cook" */, trimmed: usize, groups_ram_before_gb: f64 },
    EditorRestartHint { pid: u32, committed_gb: f64 },
    EditorGrowing { pid: u32, gb_per_hour: f64, committed_gb: f64 },
    SettingsReloaded, SettingsError(String),
}
```

  Each hint fires once per editor process until it clears. `tray.rs` currently derives
  pressure changes itself; leave `tray.rs` alone, Part E will switch it to events.

### History (`src/history.rs`)

- `History::record(&Status)` on every tick: an in-memory ring of the last 24 hours and an
  append to `history\YYYY-MM-DD.csv` in the data folder (time, mode, pressure, ram_free_gb,
  commit_free_gb, ue_committed_gb, editor_committed_gb, workers, then one RAM column per
  group). Delete files older than `history.keep_days` once a day.
- `History::series(since: Duration) -> Vec<Sample>` for the chart, `History::load_day(date)`.
- `editor_growth_gb_per_hour(pid)`: least-squares slope of that editor's committed memory
  over the last 30 minutes; `None` with under 10 minutes of samples.
- CLI: `memguard history [--hours N] [--json]` prints a compact table or JSON.
- The engine owns a `History` and exposes `engine.history()`.

---

## Part C: MCP server for agents

**Owns:** `src/mcp.rs` (new), `docs/AI-AGENTS.md` (new).
**Shared edits:** `Cargo.toml`, `src/lib.rs`, `src/bin/memguard.rs` (the `mcp` and
`agent-setup` subcommands only), `README.md` (one short section).

- `memguard mcp` is an MCP server over **stdio** (JSON-RPC 2.0, newline-delimited, protocol
  version `2025-06-18`; answer `initialize`, `notifications/initialized`, `ping`,
  `tools/list`, `tools/call`). No HTTP server, no port, no token: the agent starts the
  process, so there is nothing to secure and nothing to enable.
- Tools, each returning the same JSON the CLI's `--json` gives, as text content plus
  `structuredContent`:
  - `memguard_status`
  - `memguard_prime` (`workload?: "editor"|"build"|"cook"`, `kill_orphans?: bool`)
  - `memguard_wait` (`workload?`, `ram_gb?`, `commit_gb?`, `timeout_sec?` default 600, max 3600, `prime?`)
  - `memguard_advise`
  - `memguard_history` (`hours?` default 1) if Part B's `history` module is present when
    you merge; otherwise leave a clearly marked TODO and skip the tool.
  Tool descriptions must tell an agent when to use each one (for example: call
  `memguard_wait` before starting a local build or cook).
- `memguard agent-setup` prints ready-to-paste setup for Claude Code
  (`claude mcp add ather-memguard -- memguard mcp`) and Codex (TOML snippet).
- Write nothing to stdout except protocol messages; diagnostics go to stderr.
- Tests: drive the server through an in-memory reader/writer: initialize, list tools, call
  `memguard_status`, malformed input, unknown method.

---

## Part D: UI kit, toasts, settings window

Specified in `docs/UI.md` (written by the designer). **Owns:** `src/ui/` (new).

## Part E: setup window, history window, prime hotkey, tray wiring

Specified in `docs/UI.md`. **Owns:** `src/tray.rs`, `src/ui/setup.rs`, `src/ui/history.rs`,
`src/ui/doctor.rs`.

---

## Part F: Doctor

**Owns:** `src/doctor.rs` (new), `src/doctor_catalog.toml` (new, embedded with `include_str!`),
`src/startup.rs` (new: reading and switching startup entries).
**Shared edits:** `Cargo.toml`, `src/lib.rs`, `src/bin/memguard.rs` (the `doctor` subcommand
only, under a `// ---- Part F` marker), `src/mcp.rs` (register `memguard_doctor`, report only),
`README.md` (one section), `docs/AI-AGENTS.md` (the new tool).
Do not edit `config.rs` or `engine.rs`; Part B owns them. Read your `[doctor]` settings
straight from the settings TOML with the `toml` crate (missing file or key = defaults), and
list the keys in your report so the integrator can add them to the schema.

The doctor answers one question: **what is using memory on this PC that Unreal work doesn't
need, and what should be done about it?** It looks, explains, and proposes. It changes
nothing unless asked, and everything it changes can be undone.

### What it checks

1. **Apps.** Every process of the current user that isn't protected (`engine::is_protected`),
   grouped into apps by executable name, with process count, RAM and committed memory.
   Each app is matched against the catalogue and gets a verdict:

| Verdict | Meaning | Examples (the catalogue has many more) |
|---|---|---|
| `close` | Not needed for Unreal work; closing it is safe and frees real memory | Game launchers and games (Steam, Epic Games Launcher, Battle.net, Riot, Xbox app), media (Spotify), Widgets, Copilot, Phone Link, peripheral suites (iCUE, Razer Synapse, Armoury Crate) |
| `hold_back` | Wanted, but shouldn't compete with Unreal: put it in a MemGuard group | Browsers, chat apps, agent apps (Claude, Codex, ChatGPT), cloud sync (OneDrive, Dropbox) |
| `keep` | Part of Unreal work or the system; never proposed | Visual Studio, Rider, VS Code, Perforce, git, Windows Defender, drivers, terminals, remote-desktop tools, MemGuard |
| `review` | Unknown to the catalogue and large (over `review_above_mb`, default 1024 MB committed) | Anything unrecognised: shown with what it is (file description and company from the exe's version info) so the person can decide |
| `heavy_tool` | A big creative or virtualisation tool that is probably deliberate | Blender, Maya, Photoshop, Substance, Docker Desktop, WSL (`vmmem`), another game engine. Proposed only as "close if you're not using it" |

   Apps under `ignore_below_mb` (default 150 MB committed) are left out unless they have a
   startup entry worth switching off.
2. **Startup entries.** `HKCU\...\Run`, the user's Startup folder, `HKLM\...\Run` and the
   common Startup folder (those two read-only: changing them needs admin, so they are
   reported with "needs an administrator"), each with its enabled state from
   `Explorer\StartupApproved`. An entry whose app has verdict `close` or `heavy_tool` gets
   the proposal "don't start with Windows".
3. **Unreal leftovers.** Helper processes with no parent (reuse the orphan logic in
   `ops.rs`; copy it rather than editing `ops.rs`), more than one editor open, editors past
   the restart hint.
4. **System.** Page file small for the RAM, commit headroom low, non-paged pool above 4 GB,
   a git process pile-up (20 or more), MemGuard not running or paused.

### Actions

Every finding carries zero or more proposed actions. Nothing runs unless the person picks it.

| Action | What it does | Undo |
|---|---|---|
| `close` | Asks the app to close: `WM_CLOSE` to its top-level windows, wait up to 10 s. Apps with unsaved work show their own prompt; if it's still running the result is "still open", never a forced kill | Start the app again |
| `end` | Ends the processes (`TerminateProcess`). Offered only for windowless leftovers and for apps that ignored `close`, and only with `--force` / an explicit second confirmation | None; say so |
| `disable_startup` | Switches the startup entry off through `StartupApproved` (the same switch Task Manager uses), for HKCU and the user's Startup folder | Switch it back on |
| `hold_back` | Adds the app to the matching MemGuard group in the settings file | Remove it from the group |
| `block_while_unreal` | Adds the app to `[doctor] close_while_unreal`: while the editor, a build or a cook is running, MemGuard asks it to close when it appears. Opt-in per app, never offered for `keep`, `hold_back` or `review` apps | Remove it from the list |

- Every applied action is appended to `doctor-undo.json` in the data folder; `undo` reverses
  the most recent run, or one action by id.
- Never act on: protected processes, anything not owned by the current user, anything in
  another session, security software, MemGuard itself, and the process tree MemGuard was
  launched from (so `memguard doctor --fix` run by an agent never closes that agent).

### API

```rust
pub struct Report { pub time: String, pub system: SystemStatus, pub findings: Vec<Finding>,
                    pub reclaimable_ram_gb: f64, pub reclaimable_commit_gb: f64 }
pub struct Finding { pub id: String,            // stable: "app:steam", "startup:hkcu:Steam", "ue:orphans", "sys:pagefile"
                     pub kind: FindingKind,     // App | Startup | Unreal | System
                     pub verdict: Verdict, pub title: String, pub detail: String,  // plain words, app voice
                     pub processes: usize, pub ram_mb: u64, pub committed_mb: u64,
                     pub actions: Vec<Action>,  // proposed, most recommended first
                     pub recommended: bool }    // true = included in "fix the safe ones"
pub fn scan(cfg: &Config) -> Report;                       // read-only
pub fn apply(report: &Report, picks: &[(String /*finding id*/, ActionKind)], force: bool) -> Vec<Outcome>;
pub fn undo(last_run: bool, action_id: Option<&str>) -> Vec<Outcome>;
pub struct Guard { .. }                                    // close_while_unreal watcher
impl Guard { pub fn on_tick(&mut self, procs: &[win::Proc], unreal_active: bool) -> Vec<GuardEvent>; }
```

`recommended` is true only for `disable_startup` on `close` apps and for `close` on apps that
have no visible window (background launchers). Closing anything with a window is always the
person's explicit pick.

### CLI and MCP

- `memguard doctor` prints the report grouped by verdict with the memory each item would
  free and a total, then says how to act. `--json` for machines.
- `memguard doctor --fix` applies the `recommended` actions after printing them; `--fix
  <id>[:action] ...` applies specific ones; `--force` allows `end`; `--dry-run` shows what
  would happen. `memguard doctor --undo` reverses the last run.
- MCP tool `memguard_doctor`: the report only. Its description tells agents to relay the
  findings and ask the person before running `--fix`.

### Tests

The catalogue matcher, verdict rules, reclaimable totals, `StartupApproved` encode/decode,
undo log round trip, and "never act on" rules are pure functions over data: unit-test them.
Closing and startup switching are tested only against things the test creates itself: a
process it starts (`notepad` or `ping`), and a throwaway `HKCU\...\Run` value named
`AtherMemGuardDoctorTest` that the test deletes afterwards.

---

## Part H: disk and Dev Drive (Phase 1: look, warn, gate, count)

**Owns:** `src/disk.rs` (volumes, levels, roles, Dev Drive findings, the wait gate; pure parts
tested), `src/disk_space.rs` (the accounting, pure), `src/disk_scan.rs` (catalogue, discovery,
checkup, plan, cache), `src/disk_win.rs` (unsafe: volumes, FSCTLs, the read-only scanner, the
Defender probe), `src/disk_catalog.toml`.
**Shared edits:** `config.rs` (`[disk]`, `disk_gb` on prime targets), `ops.rs` (wait gate),
`engine.rs` (`Status.disk`, `Engine::set_disk`; the engine never reads disks), `mcp.rs`,
`doctor.rs` (`add_disk_findings`), `tray.rs` (60 s poll, toasts), `ui/flyout.rs` (disk line),
`ui/toast.rs`, `bin/memguard.rs`. The spec is the disk proposal, sections 1-8.

- Volumes are keyed by GUID path. Levels use space available to the caller and the smaller of
  the GB and percent settings, with 5 GB hysteresis; one toast per level change.
- The tray polls volumes every 60 s on a worker thread. Counting folders never happens in the
  tray: `memguard disk scan-child` does it at background priority (CPU, I/O and memory),
  `[disk] scan_threads` threads, a time budget, stopping if a build or cook starts. It opens
  files with `FILE_READ_ATTRIBUTES` only and never follows reparse points.
- Every "frees" number is **predicted** from ReFS reference counts (`basis: "predicted"`). After acting,
  the measured result is reported separately and never merged with it (Phase 2, below).
- Caches: the last checkup (`disk-checkup.json`, per item root file id and newest write) and
  the day's Defender facts (`disk-defender.json`) in the data folder. Only the CLI, the MCP
  server and the tray write them; library tests never do.

### Open risk: ReFS returns deleted space late (still unexplained)

The controlled delete test on the team PC (2026-11-14, Dev Drive D:, ReFS 3.14, 4 KB
clusters) deleted two throwaway folders: one with 0.50 GB predicted to free (unique data plus
block clones of existing files), one with 1.00 GB of unique, never-cloned data. In the next
4 to 30 minutes `GetDiskFreeSpaceEx` showed no increase at all. The same 1 GB delete on NTFS
(C:) showed at once. Later, free space on D: rose from about 341 GB to 504 GB within roughly
20 minutes; whether that was a batch of delayed ReFS frees or an unrelated large delete is not
known. An elevated `fsutil fsinfo refsinfo D:` and `GetDiskFreeSpaceEx` agreed (504 GB free,
8.68 GB reserved) and there are no shadow copies on D:. What was observed: on this ReFS
volume, deleted space can come back much later, and in bulk. **The cause is still not
established**; Phase 2 does not explain it, it measures it honestly:

- The cleanup reports the prediction at once, labelled predicted, and never says space is free.
- A watcher (`memguard disk watch-child`, detached, below-normal priority) reads the volume's free
  space every second for 2 minutes, then every 5 seconds, for at most 30 minutes from the delete.
  It stops early only when 90% of the prediction has shown up **and** three readings agree within
  16 MB. It does not stop because the free space is stable: on ReFS a stable reading at zero is the
  delay itself. At 30 minutes it reports what came back, says it hasn't all come back yet, and
  notes that this drive can return space much later.
- `other_activity` is set when free space moved more than 64 MB in the 5 seconds before acting,
  dropped more than 64 MB after rising, went below the starting point, or overshot the prediction
  by more than 10% (256 MB at least): the result then says "about" and that other writes made it
  unclear. A quiet, stable result under 80% of a prediction over 256 MB writes an
  `accounting_mismatch` log line with the file system version. No telemetry.
- Live test on this PC (2026-11-14, Phase 2 build): a throwaway project on D: with 2.5 GB predicted
  to free (exact; its 1 GB block clone in `Content` and a hard-linked file correctly predicted to stay).
  For 5 minutes nothing came back (free space even fell 0.3 GB from other writes); at 320 s free space
  rose by 105.2 GB at once, to about 498 GB, far more than anything the test deleted. The watcher
  flagged `other_activity` and now says the rise can't be told apart from this cleanup's 2.5 GB.
  The same deletes on C: (NTFS) measured 3.52 GB against 3.52 GB predicted, at once. So the bulk,
  late return is reproducible on this ReFS volume; what it releases (earlier deletes of other trees,
  ReFS metadata, something else) is still unknown. A second run (emptying 0.2 GB from the bin on
  D:) watched the full 30 minutes while other programs wrote to the drive: free space fell 53.8 GB, the
  prediction never showed up, and the result was reported as not back, with other writes noted.
- Detached watchers clear the inherit flag on the caller's standard handles first
  (`disk_win::stop_std_handle_inheritance`), so a script, the tray or an MCP client reading
  `memguard disk clean` output isn't held for the 30-minute watch.

## Part H, Phase 2: cleanup with undo, the measured result, cache relocation

**Owns:** `src/disk_clean.rs` (actions, safety rules, plans, delete, bin, tool cleans, undo log,
measured watch, tray housekeeping; pure parts tested, no `unsafe`), `src/disk_relocate.rs`
(relocation plan, `EnvStore`, apply and undo; no `unsafe`).
**Shared edits:** `disk_scan.rs` (catalogue `tool`, `actions` per item, the bin and old caches as
items, `plan_record`), `disk_win.rs` (`broadcast_environment_change`), `disk.rs`
(`recent_cleanups`), `mcp.rs` (`memguard_disk_clean`, plans recorded, INSTRUCTIONS),
`bin/memguard.rs` (`disk clean|undo|bin|relocate|watch-child`), `tray.rs` (housekeeping on the disk
poll, drive rows in the checkup window, `EmptyDiskBin`), `ui/doctor.rs` (ON YOUR DRIVES section,
the drive confirmation), `ui/toast.rs`, `ui/commands.rs`, `config.rs` (comments only). The spec is
proposal section 4 E/F with the product owner's decisions, which override it where they differ.

- **Actions.** `now` (permanent delete; the default for `regenerable`), `bin` (same-volume move into
  `<vol>:\.AtherMemGuard-bin\<run-id>\<n>\<name>`; the default for `disposable` and `rebuild_cost`;
  frees nothing until emptied and every output says so), `tool` (`npm cache clean --force`,
  `pip cache purge`, `dotnet nuget locals http-cache --clear`, found on PATH with PATHEXT; permanent,
  "can't be undone"). Zen, lanes, worktrees, the git LFS store, `review` items and the bin itself
  offer nothing. `bin` is refused at apply time when the volume root isn't writable.
- **Safety rules** are `disk_clean::refusal(&ItemCheck, &[PathFacts])`, pure and tested one by one;
  `clean` reads every fact again at apply time (`path_facts`, `newest_write`, `busy_state`,
  `sync_roots`, `git_check`). Allow-list: project items only at their catalogue path under the
  nearest folder with a `.uproject` or an `Engine` folder with `Build\Build.version`; shared caches
  only at their catalogue path inside `%USERPROFILE%` (TEMP entries: direct children of TEMP only;
  old caches: the paths in the relocation record). Never-list on the path relative to the project.
  A reparse point at the item root or any folder up to the project root (or the profile) refuses it;
  inside an item, links are removed as links (`remove_dir`/`remove_file` on the link, never followed).
  Cloud attributes or a registered sync root refuse it. `git ls-files` with a 5 s limit; a timeout or
  a failure inside a checkout refuses it. Anything written in the last `min_age_hours`, or after the
  plan's newest write, refuses the whole item, as does an Unreal process working in that project (or
  one whose command line can't be read). The path's file id must match the plan's.
- **Plans** are recorded (`disk-plans.json`, last 10) with each path's file id and newest write.
  The CLI makes a fresh plan for exactly the picks (its `frees_together` covers only the `now` and
  `tool` items) and acts on it after the confirmation. The MCP tool acts only on a plan under 10
  minutes old. The prediction after acting is the plan's `frees_together` when exactly that set went,
  else the sum of the items' own numbers labelled `lower_bound` (it can only be lower); a partial
  delete makes it `estimate`.
- **Undo:** `disk-undo.json`, the shape of `doctor-undo.json` (`runs[].entries[]` with `id`,
  `finding_id`, `title`, `action`, `step`, `undone`). Bin moves are `from_bin` steps; deletes and tool
  cleans are `nothing`. Undo refuses to overwrite a folder that is back (rebuilt) and says where the
  moved copy is. Emptying a bin run turns its entries into `nothing`.
- **The bin** empties runs older than `hold_hours` from the tray's disk poll, through a hidden
  `memguard disk bin --expired --yes` (at most once an hour). At `critical` the tray asks
  ("Empty the bin on D:?", `Command::EmptyDiskBin`) and never empties it unasked.
- **Measured result:** one `disk-watch.json` record per volume acted on, with the free space read
  just before acting (5 readings a second apart) and the prediction. The cleaner starts the detached
  watcher (`--wait` watches in-process instead); the tray restarts one whose process is gone, shows
  each result once as a toast, and the watcher writes the log line. `memguard_disk` and
  `memguard disk` list the last five (`recent_cleanups`). No disk history CSV or chart (out of scope).
- **The tray never walks or deletes:** the checkup window's drive rows come from the cached checkup,
  and CLEAN UP on them runs `memguard disk clean --yes --json` in a hidden child. A drive action can
  never map onto a memory action (`action_to_real` returns `None`).
- **Relocation:** `UE-LocalDataCachePath`, `NUGET_PACKAGES`, `npm_config_cache`, `PIP_CACHE_DIR`,
  `VCPKG_DEFAULT_BINARY_CACHE`; never `CARGO_HOME`, `GRADLE_USER_HOME`, `TEMP` or Zen (its variable is
  unconfirmed). The destination is the fixed, local, non-rotating drive with most room that stays
  `normal` after taking the caches' current size; a named destination that would drop below that is
  refused. `--apply` saves the previous values first (`disk-relocate.json`), makes the new folders
  empty, sets `REG_SZ` user variables and broadcasts `WM_SETTINGCHANGE`; `--undo` restores only
  variables still holding the value it set. Registry writes go through `EnvStore`;
  `ATHER_MEMGUARD_ENV_KEY` (must be under `HKCU\Software\AtherMemGuard-test\`) points the CLI at a
  test key and turns the broadcast off. Tests never open the real `HKCU\Environment`.
- **Tests:** every rule; tool command construction and a fake `npm.cmd` on a test PATH; delete with a
  junction, a read-only file and a file held open; bin move, undo, refusal to overwrite, emptying; the
  measured watch on quiet NTFS, late ReFS frees, never-back and busy drives; relocation plan, apply and
  undo on an in-memory store and on a throwaway key under `HKCU\Software\AtherMemGuard-test`; the
  MCP tool over stdio on a tree the test makes (no plan, disposable item, recent write, written since
  the plan, stale plan, forged protected path, success).

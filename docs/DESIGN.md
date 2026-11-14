# Ather MemGuard: design for 0.2

This is the contract for the 0.2 work. Several people build parts of it in parallel, each in
their own git worktree, so **file ownership matters**: only edit the files your part owns, plus
the small shared edits listed for it. Everything else is merged by the integrator.

Read `README.md` first for what the app does today, then `src/lib.rs` for the module map.

## Ground rules for every builder

- Rust stable, MSVC. Cargo is at `%USERPROFILE%\.cargo\bin` and may not be on PATH:
  in PowerShell run `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"` first.
- `cargo build` and `cargo test` must pass with no warnings before you finish.
- All `unsafe` Win32 code stays in `win.rs`, `tray.rs`, `logo.rs`, `net.rs` and `ui/`.
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
  - no args, running from anywhere else: first-run setup. Part E builds the branded window;
    until then call `install::install(true, true)` directly and exit. Expose the decision as
    `pub fn install::launch_mode() -> LaunchMode { Tray, Setup }` so Part E can plug in.
  - `--after-update <pid>`: finish an update (below), then run the tray.
  - `--uninstall`: same as `memguard uninstall` (Settings > Apps will call this; change
    `UninstallString` to `"<AtherMemGuard.exe>" --uninstall`).
  - `--install [--no-path] [--no-start]`: silent install, for scripts.
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

Specified in `docs/UI.md`. **Owns:** `src/tray.rs`, `src/ui/setup.rs`, `src/ui/history.rs`.

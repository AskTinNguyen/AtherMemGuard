# Ather MemGuard: UI specification

The visual reference is `docs/design/mockup.html` (open it in a browser). This document is the
behaviour, the wording and the build contract. Where the two disagree, this document wins on
behaviour and wording, the mockup wins on looks.

All UI is custom drawn Win32 in Rust (`windows-sys`), the same approach as AtherScreenshot
(`C:\AtherScreenshot\src\settingsui.cpp`, `toast.cpp`, `installer.cpp`, `common.cpp` are the
reference implementations; read them before building the matching surface). No web view, no
native dialog controls except a temporary `EDIT` while a text or number field is being edited,
`IFileOpenDialog` / `IFileSaveDialog` for files, and popup menus.

## Principles

1. **Glanceable first.** The tray icon, its tooltip and the flyout answer "is memory fine?"
   without reading numbers. Numbers are there for the people who want them.
2. **Safe by default, danger is labelled.** Controls that can freeze or crash an app (freeze,
   hard cap, pausing workers) are off by default, red-labelled, and say what happens.
3. **Changes apply as you make them.** No Save button. Right-click resets. The footer says so.
4. **Plain words.** "Soft cap", "Memory is tight", "Free memory". Never "working set",
   "commit charge", "EcoQoS" or "trim" in the UI. Mapping: elevated = "tight"; trim/prime =
   "free memory"; CPU `below`/`idle`/`eco` = "Slower" / "Slowest" / "Efficiency mode";
   suspend = "freeze"; memory priority `normal/belownormal/medium/low/verylow` = "Normal",
   "Slightly low", "Medium", "Low", "Very low". "Commit" is allowed only as "Commit free" in
   the status readouts, with the hint "Memory Windows has promised to apps. When this runs
   out, apps fail even if RAM is free."
5. **Quiet.** A toast only when something changed that the person would want to know.
   Never more than three on screen. Nothing steals focus.

## Theme (`src/ui/theme.rs`)

Dark only. Every window sets `DWMWA_USE_IMMERSIVE_DARK_MODE` and `DWMWA_CAPTION_COLOR = BG`.

| Token | RGB | Use |
|---|---|---|
| `BG` | 14,14,13 | Window background, logo tile |
| `SURFACE` | 26,27,25 | Cards, search bar, footer, toasts, menu |
| `RAISED` | 37,38,35 | Inputs, pills, toggle off track, slider track |
| `SELECTED` | 42,45,33 | Hovered / selected row |
| `BORDER` | 50,52,46 | Hairlines, control borders |
| `TEXT` | 242,239,232 | Main text |
| `TEXT_DIM` | 208,206,199 | Toast and setup body text |
| `MUTED` | 150,150,142 | Hints, captions, placeholders, toggle knob when off |
| `ACCENT` | 212,255,0 | Section headers, toggle on, primary button, caret, bars, toast edge |
| `ACCENT_SOFT` | 226,255,110 | Primary button hover |
| `ON_ACCENT` | 14,14,13 | Text and knobs on lime |
| `WARN` | 255,184,48 | "Tight" pressure, over-cap bars, warning toast edge |
| `ERROR` | 255,120,110 | "Critical" pressure, danger labels, error toast edge |

Fonts: body "Segoe UI Variable Text" (fallback "Segoe UI"); display "Bahnschrift SemiBold
Condensed" for UPPERCASE headlines; eyebrow "Bahnschrift SemiBold", uppercase, letter-spaced
3 px; glyph icons from "Segoe Fluent Icons". Sizes in the mockup are at 96 DPI; scale
everything with the window's DPI (`S(n)` helper) and handle `WM_DPICHANGED`.

Shapes: 6 px radius on controls, 8 px on cards and windows' inner panels, anti-aliased
(GDI+ flat API through `windows-sys` is fine for shapes; draw text with GDI `DrawTextW` for
ClearType). Everything is double buffered.

## UI kit (`src/ui/kit.rs`, `src/ui/window.rs`)

One small retained model shared by every surface, not a general toolkit:

- `Canvas`: double-buffered paint target with `fill_round_rect`, `stroke_round_rect`, `text`
  (font, colour, alignment, ellipsis), `spaced_text`, `icon`, `line`, `logo(rect)`.
- Widgets are plain structs with `rect`, `paint(&mut Canvas, &State)` and
  `hit(point) -> Option<Action>`: `Toggle`, `Segmented`, `Pill` (opens a popup menu),
  `NumberField`, `Slider`, `Chips`, `Button` (primary / normal / link), `HotkeyField`,
  `Bar` (meter with an optional marker).
- `Window` trait + a generic window proc: paint, mouse (hover, press, capture for sliders),
  wheel scrolling with a thin lime scrollbar thumb, keyboard (Tab order, Space/Enter, Esc),
  DPI, single instance per window kind (opening again brings it to the front).
- **Snapshots for review and tests:** every surface can paint itself into a bitmap without
  being shown. `AtherMemGuard.exe --ui-snapshots <dir>` writes PNG files (via GDI+ or WIC):
  `settings.png`, `settings-group-open.png`, `settings-search.png`, `flyout-normal.png`,
  `flyout-critical.png`, `toast-*.png` (one per catalogue entry), `setup-install.png`,
  `setup-done.png`, `setup-uninstall.png`, `history-6h.png`. Snapshots use fixed sample data
  (the numbers in the mockup), never the live machine, so they are stable. This is how the
  work gets reviewed: generate them and look at them before calling a surface done.

## 1. Settings window (`src/ui/settings.rs`) — Part D

900 x 760 at 96 DPI, resizable, minimum 720 x 480, remembers size and position in
`ui-state.toml` in the data folder. Title "Ather MemGuard — Settings".

Layout, top to bottom (see mockup):

- **Search bar**, 60 px, `SURFACE`. Typing anywhere goes to search. Every word must match a
  row's label, hint, section or current value. While searching the hero is hidden and
  non-matching rows and empty sections disappear; group cards open if a row inside matches.
  Esc clears the search, then closes the window. Ctrl+W closes.
- **Hero**: logo 64 px, eyebrow "ATHER MEMGUARD", headline "SETTINGS", and on the right the
  live strip: Unreal mode, Pressure (lime / amber / red; words "NORMAL", "TIGHT", "CRITICAL"),
  RAM free, Commit free. It refreshes from the engine's latest `Status` once a second.
- **Sections**, each an uppercase lime letter-spaced header with a hairline:

| Section | Rows |
|---|---|
| GENERAL | How hard to push other apps (segmented: Gentle / Balanced / Aggressive) · Free memory when a build or cook starts (toggle) · Run at Windows startup (toggle, registry not settings file) · Notifications (toggle) · Free memory now (hotkey field) · Check memory speed (toggle, `general.check_ram_speed`) |
| APP GROUPS | One card per group, then "Add a group" (button "Add group…") |
| UNREAL | Never touched (chips) · Suggest restarting the editor above (GB) · Warn when the editor grows faster than (GB / hour) · Pause compile workers when memory is critical (toggle, danger hint) · Keep at least this many workers running (number, only visible when the toggle is on) |
| MEMORY PRESSURE | "Memory is tight when RAM free drops below" (%) · "…or commit free drops below" (%) · "Memory is critical when RAM free drops below" (%) · "…or commit free drops below" (%). Each shows the GB it means on this PC as its unit text |
| HEADROOM | Editor, Build, Cook: each row has two number fields, "RAM" and "Commit" in GB. Hint: "What `memguard wait` and agents wait for before starting this kind of work." |
| HISTORY | Record memory history (toggle) · Keep for (days) · Open memory history (button) |
| UPDATES | Check for updates automatically (toggle) · "Version X" with status hint and button "Check now" (becomes "Update now" when one is available) |
| AI AGENTS | "Let agents check and free memory" hint "Agents use the `memguard` command or its MCP server. Paste this into your agent's setup." button "Copy setup" · button "Open guide" (docs/AI-AGENTS.md on GitHub) |
| TEAM AND BACKUP | Team defaults (status hint: "None loaded" or "Loaded <date>, N settings"; buttons "Load team file…", and "Remove" when loaded) · Your settings (buttons "Export…", "Import…", "Open file") · Start over (button "Reset all settings", confirms first) |

- **Rows**: 66 px, label 14 px semibold, hint 12 px `MUTED`, control right-aligned in a
  280 px column (wider where the mockup shows). Hover tints the row `SELECTED`.
- **Where a value comes from**: a lime dot after the label when the user has changed it; a
  small outlined "TEAM" tag when it comes from the team file. Right-click a row: "Reset to
  default (value)" (disabled when already default). For team values the item reads "Reset to
  team default (value)".
- **Footer**, `SURFACE`: "Changes are saved automatically" left, "Right-click a setting to
  reset it" right.

**Group card.** Collapsed: chevron, name, "N processes", a usage bar (RAM now as a share of
`max(cap x 1.5, RAM now)`, lime, amber when over the cap, with a white marker at the cap; no
bar fill colour change for groups with no cap, bar is `MUTED`), "X GB / cap Y GB" (or "no
cap"), and the memory-priority pill. Click the header to open. Open (one at a time):

- **Apps**: chips; click a chip's x (shown on hover) to remove; "+ Add app…" opens a popup
  list of running processes not already in a group, sorted by RAM, each line "name · N
  processes · X GB"; last item "Type a name…" opens an inline text field.
- **The four tiers** as a grid: Always (hint "Unreal not running") / Unreal is running /
  Memory is tight / Memory is critical, each with a soft-cap slider (0 to 25% of RAM, snapping
  to 0.1%, left end = "Off"), a readout "4.8%" over "6.1 GB", and a Speed pill (Normal /
  Slower / Slowest / Efficiency mode). A tier that isn't overridden shows its inherited value
  at 50% opacity; dragging it creates the override; right-click "Use the value above" removes
  it. Sliders write `trim_above_pct`.
- **Freeze when memory is critical** (toggle, label in `ERROR`): "The apps stop responding
  until memory recovers."
- **Hard cap** (number, MB, label in `ERROR`): "Apps that hit a hard cap usually crash.
  Browser helper processes can't be capped." Turning it on from Off asks once: "Set a hard
  cap? If <group> reaches <N> MB its apps will fail to get more memory and will probably
  crash." buttons "Set hard cap" / "Cancel".
- Right-click the card header: "Turn off this group" / "Turn on", "Rename…", "Remove group"
  (built-in groups can only be turned off).
- The Agents group never shows Freeze or Hard cap: instead a hint "These run your agents, so
  they are only ever slowed down and held back, never frozen or capped."

All writes go through the settings API from Part B (`config::set_user_value`,
`reset_user_value`, `value_layer`, `add_group`, `remove_group`, `export_to`, `import_from`,
`install_team_file`). A failed write shows an error toast with the message and leaves the
control as it was. The window re-reads settings when the file changes on disk.

## 2. Toasts (`src/ui/toast.rs`) — Part D

Custom, non-activating, layered, topmost popups in the bottom-right of the primary work
area, 16 px from the edges, 360 px wide, stacking upwards with 12 px gaps, at most three
(the oldest leaves early). `SURFACE` background, 3 px left edge (lime / amber for warnings /
red for errors), condensed uppercase headline 17 px, body 13 px `TEXT_DIM`, optional buttons.
Slide and fade in 150 ms, out 200 ms. Default 6 s; 15 s with buttons; hover pauses the timer.
Clicking the body runs the toast's default action if it has one, then closes it. A body can
be updated in place (download progress). Skipped entirely when `general.notifications` is
off, except errors caused by something the person just did.

```rust
pub enum ToastKind { Info, Warning, Error }
pub struct Toast { pub kind: ToastKind, pub title: String, pub body: String,
                   pub actions: Vec<(String, Command)>, pub on_click: Option<Command>, pub key: Option<&'static str> }
pub fn show(toast: Toast) -> ToastId;        // same `key` replaces the toast already showing
pub fn update_body(id: ToastId, body: &str);
```

`Command` is the app-wide command enum also used by the tray menu and flyout (Part E owns it
in `src/ui/commands.rs`): `FreeMemory, Pause30, PauseUntilResumed, Resume, OpenSettings,
OpenHistory, CheckUpdates, InstallUpdate, ToggleStartup, CopyAgentSetup, Quit`.

Catalogue (exact wording; `{}` are values):

| When | Kind | Title | Body | Buttons |
|---|---|---|---|---|
| First run after install | Info | Ather MemGuard is running | It keeps memory free for Unreal. {hotkey} frees memory now. | Open settings |
| Pressure becomes critical | Error | Memory is critical | {ram} GB RAM free, {commit} GB commit free. Background apps are being held back. Don't start a build yet. | Free memory |
| Pressure back to normal after critical | Info | Memory is fine again | {ram} GB RAM free. | |
| Auto-primed | Info | Freed memory for the {build\|cook} | Held back {n} background processes using {x} GB. | |
| Manual free memory | Info | Freed memory for Unreal | Held back {n} background processes. RAM free went from {a} to {b} GB. | |
| Emptied the file cache | Info | Emptied Windows' file cache | It held {x} GB. RAM free went from {a} to {b} GB. | |
| File cache: permission declined | Info | Windows' file cache is unchanged | Emptying it needs administrator permission, and that was declined. | |
| File cache: failed | Error | Couldn't empty Windows' file cache | {why}. | |
| Editor restart hint | Warning | Unreal Editor is at {n} GB | It has been open {h} h {m} min. Restarting it gets that memory back. | |
| Editor growing | Warning | Unreal Editor is growing {n} GB an hour | At this rate memory runs out in about {t}. | Show history |
| Paused | Info | Paused for 30 minutes | Other apps are back to normal until {hh:mm}. | Resume now |
| Paused until resumed | Info | Paused | Other apps are back to normal until you resume. | Resume |
| Resumed | Info | Ather MemGuard resumed | | |
| Update available | Info | Ather MemGuard {v} is available | {notes} Click to update ({size}). Your settings stay. | (click = InstallUpdate) |
| Downloading | Info | Updating Ather MemGuard | Downloading… {pct}% | |
| Updated | Info | Updated to Ather MemGuard {v} | Your settings are just as you left them. | |
| Up to date (manual check) | Info | Ather MemGuard is up to date | Version {v} | |
| Check failed (manual check) | Error | Couldn't check for updates | {reason} | |
| Update failed | Error | The update didn't install | Click to download it from GitHub instead. | (click opens the releases page) |
| Startup toggled | Info | Run at startup: On \| Off | | |
| Settings problem | Error | There's a problem in your settings | {message} The last good settings are still in use. | Open file |
| Setup copied | Info | Agent setup copied | Paste it into your agent's terminal. | |
| About three minutes after start, the RAM runs below its rated speed; once per report `fingerprint` (`[tray] last_ram_fingerprint` in ui-state.toml); never with notifications or `general.check_ram_speed` off | Warning | Your RAM is running below its rated speed | {current} MHz now, rated {rated} MHz. Turning on {XMP\|EXPO} in the BIOS takes about five minutes. | Show me how |
| Same, but the RAM check found another problem (old Intel microcode, both sticks on one channel) and the speed is fine | Warning | {the finding's title} | {the finding's detail} | Show me how |

Pressure going from normal to tight shows no toast (the icon dot and flyout carry it).

## 3. Tray icon, menu and flyout — Part E

- **Icon**: the Ather mark; amber dot when tight, red when critical (exists in `logo.rs`).
  While paused, draw the mark at 45% opacity.
- **Tooltip**: `Ather MemGuard\n{Mode} · memory is {fine|tight|critical}\nRAM free {x} GB · commit free {y} GB`
  (or `Paused until {hh:mm}`).
- **Left-click** opens the flyout. **Right-click** opens the menu. Double-click opens Settings.
- **Menu** (dark owner-drawn popup is a nice-to-have; a standard popup menu is acceptable):

```
{Mode} · memory is {fine|tight|critical}        (disabled status line)
────────
Free memory now                    {hotkey}
Empty Windows file cache…                      (Windows asks for administrator permission)
Pause for 30 minutes
Pause until I resume                           (both replaced by "Resume" while paused)
────────
Settings
Memory history
────────
Update to version {v}…                         (or "Check for updates…")
✓ Run at Windows startup
Quit Ather MemGuard
```

- **Flyout** (`src/ui/flyout.rs`): 340 px wide borderless popup anchored above the tray icon
  (use `Shell_NotifyIconGetRect`), closes on focus loss or Esc. Its job is to say what is
  using the RAM, so every part it lists adds up to the RAM in use (`Status.ram`, built by
  `engine::ram_breakdown`).
  - Eyebrow = mode ("EDITOR + BUILD", "IDLE"); headline "MEMORY IS FINE" / "MEMORY IS TIGHT"
    (amber) / "MEMORY IS CRITICAL" (red) / "PAUSED".
  - "RAM in use 92.6 of 96 GB" with "3.4 GB free" on the right (lime / amber / red by the
    pressure thresholds), over one stacked bar: a segment per part as a share of total RAM,
    the empty track is free. A critical bar ends in a red notch.
  - The Unreal band: "Unreal · 52.9 GB" (private RAM of everything on the protect list and the
    workers), "Editor 41.6 GB · 14 compile workers 9.8 GB · other 1.5 GB", then "Up 5 min ·
    growing 2.1 GB an hour · Windows promised it 47.0 GB" (the committed total), or "Unreal
    isn't running".
  - Rows, largest first by the size shown, each with a swatch matching its bar segment:
    "Other apps" (apps in no group; its three biggest named, then "N smaller apps", when it is
    1 GB or more or memory is tight), each group of 1 GB or more, the smaller groups on one
    row ("Chat, Background"), "Windows file cache", "Windows and drivers", "Squeezed by
    Windows" (the Memory Compression process), "Shared or not itemized" (the rest: memory
    processes share, memory drivers lock). From 1 GB, "Windows file cache" has the link "Empty
    it now (needs admin)" under it (`Command::FreeWindowsCache`, see below).
  - Critical: a red-edged box "Closing {apps} gets about {x} GB back. Don't start a build or
    cook yet." (apps of 0.5 GB and up, at most three), led by "Emptying Windows' file cache gets
    about {c} GB back" when the cache holds 2 GB or more with the link "Review and close" (opens
    the checkup; nothing closes from the flyout). Otherwise the link "Run a checkup".
  - Advice from `Status.advice` as one muted line, at most two lines (the critical line gives
    way to the box).
  - "Commit free" meter under a hairline, with the hint "Room left for apps to ask for more
    memory: RAM plus the page file. When it runs out, apps crash even if RAM is free."
  - Buttons "Free memory" (primary), "Pause 30 min" (or "Resume"), "Settings".
  - App names are only in memory: `RamBreakdown::top_apps` is left out of status.json.
- **Emptying Windows' file cache** (`Command::FreeWindowsCache`, `ops::flush_cache`): Windows
  only lets an elevated process do it, and the tray runs as the user. So the tray starts its
  own exe as `AtherMemGuard.exe --flush-cache` with the `runas` verb: Windows shows its
  administrator prompt, the elevated copy empties the cache (`SetSystemFileCacheSize(-1, -1)`)
  and writes out the modified list, then exits. The tray measures RAM free and the cache before
  and after and shows a toast. The pages go to the standby list, which counts as available;
  Windows reads the files again when they are needed. Nothing does this on its own: only a
  click (or `memguard flush-cache`), because each run asks for permission.
- **Hotkey**: `general.prime_hotkey` registered with `RegisterHotKey`; runs FreeMemory. If
  registration fails, an error toast "Shortcut {keys} is taken" with "Pick another one in
  Settings.", once per start.
- **Engine events** (`Engine::take_events()`) drive the toasts in the catalogue; replace the
  balloon code in `tray.rs` and the `MessageBox` status.
- **Updates**: first check one minute after start, then every 24 hours, on a worker thread,
  when `updates.check_automatically` is on. Each new version is announced once.

## 4. Setup window (`src/ui/setup.rs`) — Part E

520 x 600, fixed, centred, title "Ather MemGuard Setup". Shown when the exe runs from
outside the install folder with no portable marker (`install::launch_mode() == Setup`).

| State | Eyebrow | Headline | Body | Options | Buttons |
|---|---|---|---|---|---|
| Not installed | ATHER MEMGUARD · V{v} | INSTALL FOR YOU | Keeps memory free for Unreal Editor, builds and cooks by holding back other apps. Installs just for your Windows account — no admin rights needed. | Start with Windows (on) · Add the memguard command (on) | INSTALL · link "Run without installing" |
| Older version installed | same | UPDATE | Version {old} is installed. This replaces it with {v}. Your settings stay. | same, pre-set from the current install | UPDATE |
| Same version installed | same | REINSTALL | Version {v} is already installed. Reinstalling repairs it. Your settings stay. | same | REINSTALL · link "Just run it" |
| Working | same | INSTALLING… | (thin lime indeterminate bar) | | |
| Done | same | READY | Ather MemGuard is in your tray. It starts holding back other apps when Unreal needs the memory. | | OPEN SETTINGS · DONE |
| Failed | same | COULDN'T INSTALL | {message} | | TRY AGAIN · CLOSE |
| `--uninstall` | same | UNINSTALL? | This removes Ather MemGuard and puts every app back to normal. Your settings and history are kept in case you come back. | | UNINSTALL · CANCEL |

"Run without installing" writes the portable marker next to the exe and starts the tray.
Install runs on a worker thread. The window owns no install logic: it calls `install::*`.

## 5. Memory history window (`src/ui/history.rs`) — Part E

960 x 600, resizable. Title "Ather MemGuard — Memory history". Eyebrow + headline
"MEMORY HISTORY"; range pills 1 h / 6 h / 24 h / 7 d (7 d reads the daily CSV files,
downsampled to at most 1500 points).

- **Chart** on `SURFACE`: y axis in GB from 0 to total RAM (gridlines every quarter), x axis
  time. Lines: Unreal committed (lime, 2.5 px), RAM free (off-white, 1.5 px), Commit free
  (muted, dashed). Background bands: amber where pressure was tight, red where critical.
  Lime dots on the x axis where memory was freed. A dashed vertical line with a label where
  an editor process started ("Editor started"). Hover: crosshair plus a small `RAISED`
  readout with the time and the three values.
- **Legend** under the chart, as in the mockup.
- **Cards**: Unreal Editor ("Now X GB · peak Y GB · growing Z GB an hour" or "steady"), then
  one per group ("Now X GB · peak Y GB").
- **Footer**: "Recorded every {n} seconds, kept for {d} days" and a link "Open the CSV folder".
- Empty state: "No history yet. It starts filling in as soon as Ather MemGuard has been
  running for a minute."

## 6. Checkup window, the doctor (`src/ui/doctor.rs`) — Part E

760 x 720, resizable. Title "Ather MemGuard — Checkup". Opened from the tray menu
("Run a checkup", between "Memory history" and the update item), the flyout (a link
"Run a checkup" under the group bars) and the checkup toast. Data and actions come from
`doctor::scan`, `doctor::apply` and `doctor::undo` (docs/DESIGN.md, Part F).

- **Hero**: eyebrow "ATHER MEMGUARD", headline "CHECKUP". On the right the result in one
  number: "{x} GB" over the caption "COULD BE FREED" (lime), or "ALL CLEAR" when nothing is
  proposed. A link "Check again" re-scans.
- **Sections**, in this order, each a lime header with the memory it covers on the right;
  empty sections are hidden:

| Section | Findings | Row |
|---|---|---|
| NOT NEEDED FOR UNREAL | verdict `close` | Checkbox · app name · "N processes · X GB" · hint saying what it is ("Game launcher. Not used by Unreal.") · action pill: "Close" / "Close and don't start with Windows" / "Close whenever Unreal is running" |
| STARTS WITH WINDOWS | startup entries | Checkbox · entry name · hint "Starts every time you sign in. Uses about X GB." · pill "Don't start with Windows" (or the muted text "Needs an administrator" and no checkbox) |
| BIG TOOLS YOU MIGHT NOT BE USING | verdict `heavy_tool` | Unchecked by default · "Close if you're not using it" |
| WORTH A LOOK | verdict `review` | Unchecked · app name, company, "X GB" · pill: "Close" / "Hold back" / "Leave it" (Leave it adds it to `[doctor] keep`) |
| HELD BACK, NOT CLOSED | `hold_back` apps not yet in a group | Checked · "Hold back" (adds the app to the matching group) |
| UNREAL | leftovers | "3 leftover Unreal helpers from a closed editor · 1.2 GB" · "End them" |
| THIS PC | system | Text rows, each with a link: "The page file is small for this much RAM." → "How to fix". The RAM check's rows (`sys:ram`: the report's headline over its summary, and `sys:ram:<what>` for each problem such as old Intel microcode; only with `general.check_ram_speed` on) → "Show me how", which writes `ram-report.html` to the data folder and opens it in the browser (`Command::ShowRamReport`) |

- Rows whose finding is `recommended` start checked. The footer shows the running total of
  what is checked, "Selected: frees about {x} GB", on the left; on the right the primary
  button "CLEAN UP" (disabled with nothing checked) and a link "Undo last cleanup" when
  there is one to undo.
- **Clean up** runs the picks on a worker thread; each row turns into its result in place:
  "Closed", "Still open — it may be asking you to save", "Switched off", "Added to
  Browsers". An app that stayed open gets a red link "End it anyway", which asks first:
  "End {app}? Anything unsaved in it is lost." buttons "End it" / "Cancel".
- **Ending always asks.** When any checked row's pick ends processes ("End it" on a
  background app, "End them" on Unreal leftovers), CLEAN UP first shows the in-window
  confirmation listing them: "End {n} apps?" · "Anything unsaved in them is lost." and the
  names · buttons "End them" / "Cancel" (Cancel is the focused default; Enter and Esc
  cancel). Nothing runs until "End them"; Cancel leaves the picks as they were. A single
  app reads "End {app}? Anything unsaved in it is lost." with "End it".
- Nothing in this window changes anything without a visible checkbox and the CLEAN UP click.

Toasts added to the catalogue:

| When | Kind | Title | Body | Buttons |
|---|---|---|---|---|
| Unreal work starts and the checkup finds at least `nudge_above_gb` (default 4) to free; at most once a day | Info | {x} GB is in use by apps Unreal doesn't need | {top three app names}. | Review |
| Cleanup finished | Info | Checkup done | Freed about {x} GB. {n} apps won't start with Windows any more. | Undo |
| A `close_while_unreal` app was closed | Info | Closed {app} | It's on your list to stay closed while Unreal is running. | Allow this time |

`Command` gains `OpenDoctor` and `UndoDoctor` (and `ShowRamReport` for the RAM check).
Snapshots: `doctor.png`, `doctor-clear.png`, `doctor-results.png`, `doctor-confirm.png`,
`doctor-confirm-end.png`.

## Accessibility and robustness

- Every interactive element is reachable with Tab, shows a 1 px `ACCENT` focus ring, and
  works with Space / Enter; sliders with arrow keys (0.1%) and PgUp/PgDn (1%).
- Text never clips: hints wrap to two lines, then ellipsis; windows have minimum sizes.
- No UI code calls the engine's enforcement functions directly: UI sends `Command`s and edits
  settings; the tray loop owns the engine.

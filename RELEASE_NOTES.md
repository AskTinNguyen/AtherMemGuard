# Ather MemGuard 0.2.0 (Windows)

**Get it:** download `AtherMemGuard-Setup-0.2.0.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **0.1 (or MemCap):** run the setup exe once. It stops the old copy, replaces it and keeps your settings. After that, updates come in the app.
- **Agents and scripts:** `memguard` is still on your PATH, and the setup exe writes it for you, so there's only one file to download.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.0-portable.zip` runs from any folder without installing.

## What's new

### One file to install, and updates in the app
- **One setup exe:** `AtherMemGuard-Setup-<version>.exe` is the app itself. It opens a small setup window with two switches, **Start with Windows** and **Add the memguard command**, installs both programs, adds a **Start menu** shortcut and an entry in **Settings › Apps**, and starts. **Run without installing** runs it from where it is.
- **Updates itself:** the app checks GitHub once a day for a newer version, downloads it next to itself, checks its size and SHA-256, swaps it in and restarts. Nothing is installed if the download doesn't match. The release files have to be reachable from your PC: while the repository is private, only teammates with access get automatic updates.
- **Your own update server:** `[updates] manifest_url` in the settings points the app at another `latest.json`, for teams that host their own builds.
- **Uninstall the usual way:** Settings › Apps › Ather MemGuard › Uninstall asks first, then puts every app back to normal. Settings, history and the log are kept.
- **Silent install for scripts:** `AtherMemGuard-Setup-<version>.exe --install [--no-path] [--no-startup] [--no-start]`, or `memguard install --no-startup`.

### A tray app you can read at a glance
- **Flyout:** click the tray icon to see whether memory is fine, tight or critical, RAM and commit free, what Unreal is doing and how each app group sits against its cap, with **Free memory**, **Pause 30 min** and **Settings**.
- **Toasts** in Ather style, only when something changed: memory critical and fine again, memory freed for a build or cook, the editor is big or growing fast, an update is ready.
- **Free memory shortcut:** **Ctrl+Alt+M** from anywhere (change it in Settings).
- **Pause** for 30 minutes or until you resume; every app goes back to normal meanwhile.

### Settings people can use
- **Settings window:** every setting with a plain description, search, changes saved as you make them, right-click to reset. Risky options (freeze, hard cap) are red and off by default.
- **Presets:** Gentle, Balanced or Aggressive scales every limit at once.
- **Limits that scale with RAM:** soft caps are a share of your RAM, so one settings file suits a 32 GB and a 128 GB PC.
- **Team defaults:** load a team file and it sits under your own settings; what you changed still wins. **Export** and **Import** your settings.
- **0.1 settings:** an untouched 0.1 file is replaced by the new template (kept as `.bak`); one you edited keeps working.

### Checkup
- **Run a checkup** from the tray: what is using memory that Unreal doesn't need (launchers, music, overlays, apps that start with Windows, big tools you might not be using, unknown apps, Unreal leftovers), how much could be freed, and one click to clean up what you tick. Apps are asked to close, never forced; anything unsaved stays. Ending an app is only ever your pick, and always asks first. **Undo last cleanup** reverses it.
- **A nudge when Unreal starts:** if 4 GB or more could be freed, a toast says so, at most once a day.
- **Keep apps closed while Unreal runs:** apps on that list are asked to close when they start during Unreal work, with **Allow this time** on the toast.
- `memguard doctor` is the same checkup for the command line and agents.

### RAM and BIOS check
- **Is your RAM as fast as you paid for?** Memory kits ship with their fast profile (XMP or EXPO) switched off, and many PCs run for years at the slower default. Ather MemGuard reads the PC's firmware tables and says whether the RAM runs below its rated speed, whether the sticks are in the right slots, whether the BIOS may be out of date, and which cautions apply to your CPU.
- **It only reports.** Nothing in the BIOS, the registry or a driver is changed, and no admin rights are needed. **Show me how** opens a page with steps for your board's BIOS, your PC's numbers filled in, what to do if it doesn't start, and how to check it worked.
- **Told once:** a toast a few minutes after start if the RAM runs below its rated speed (or a problem such as old Intel microcode), and again only when the situation changes. The checkup shows it under **This PC**.
- `memguard ram` prints the report and the steps; agents get it from the read-only `memguard_ram` MCP tool. Turn it off in Settings › General › **Check memory speed**.

### Memory history and smarter freeing
- **Memory history** window: Unreal's memory, RAM free and commit free over 1 hour to 7 days, with tight and critical spells, memory freed and editor starts marked. Recorded to a CSV file per day.
- **Free memory when a build or cook starts**, automatically.
- **Editor alerts:** a toast when the editor passes its restart hint or keeps growing fast, with how long until memory runs out.

### For agents
- **MCP server:** `memguard mcp` gives Claude Code, Codex and other agents MemGuard as tools (status, prime, wait, advise, history, and a read-only checkup and RAM check). **Copy setup** in Settings, or `memguard agent-setup`, gives the text to paste. See `docs/AI-AGENTS.md`.

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

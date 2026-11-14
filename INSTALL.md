# Ather MemGuard: install guide

## Install (a few seconds, no admin rights)

1. Download **`AtherMemGuard-Setup-<version>.exe`**.
2. Double-click it.
   - If Windows SmartScreen says *"Windows protected your PC"*, click **More info → Run anyway**. The app isn't code-signed yet, so Windows doesn't recognize the publisher.
3. Look for the Ather icon in the tray. That's it: Ather MemGuard is running and starts again when you sign in.

The installer:
- installs `AtherMemGuard.exe` and `memguard.exe` to `%LOCALAPPDATA%\Programs\AtherMemGuard` (just for you)
- starts the app when you sign in, and now
- adds a **Start menu** shortcut
- adds an uninstall entry in **Settings › Apps**
- puts `memguard` on your user PATH, so agents and scripts can call it (open a new terminal to pick it up)

If you had MemCap or Ather MemGuard 0.1, the installer stops it, replaces it and keeps your settings.

**For scripts:** `AtherMemGuard-Setup-<version>.exe --install` installs without asking. Add `--no-path` to leave PATH alone and `--no-start` to not start it now.

## Use it

| Where | What it does |
|---|---|
| Tray icon | Status, **Prime now** (free RAM for Unreal), Pause, Settings, log |
| `memguard status` | What's running, memory pressure, per-group use, advice |
| `memguard wait --for build --prime` | Waits until there's room for a build; agents call this before a local build or cook |
| `memguard advise` | Suggested `-MaxParallelActions` for the memory available now |

Settings are in `%APPDATA%\AtherMemGuard\settings.toml` (tray menu → Settings). The README explains every rule.

## Update

From 0.2 the app updates itself. It checks GitHub for a newer version, downloads it, checks its size and SHA-256, swaps it in and restarts. Your settings and log are kept.

Running a newer `AtherMemGuard-Setup-<version>.exe` works too.

To use your team's own update server, set `manifest_url` under `[updates]` in the settings to the address of its `latest.json`.

## Uninstall

Go to **Settings › Apps › Installed apps › Ather MemGuard › Uninstall**, or run `memguard uninstall`. Apps go back to normal priority, and the program, the Start menu shortcut and the PATH entry are removed. Your settings and log (`%APPDATA%\AtherMemGuard`) are kept.

## Portable use

Unzip `AtherMemGuard-<version>-portable.zip` anywhere and run `AtherMemGuard.exe`. The `AtherMemGuard.portable` file next to it makes it run from that folder instead of installing. Delete that file if you'd rather it install.

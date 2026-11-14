# Ather MemGuard: install guide

## Install (a few seconds, no admin rights)

1. Download **`AtherMemGuard-Setup-<version>.exe`**.
2. Double-click it.
   - If Windows SmartScreen says *"Windows protected your PC"*, click **More info → Run anyway**. The app isn't code-signed yet, so Windows doesn't recognize the publisher.
3. The setup window has two switches, both on: **Start with Windows** and **Add the memguard command**. Click **INSTALL**.
4. Look for the Ather icon in the tray. That's it: Ather MemGuard is running (and starts again when you sign in, if you left that on). **OPEN SETTINGS** on the last screen shows every setting.

The installer:
- installs `AtherMemGuard.exe` and `memguard.exe` to `%LOCALAPPDATA%\Programs\AtherMemGuard` (just for you)
- starts the app now, and when you sign in (unless you switched that off)
- adds a **Start menu** shortcut
- adds an uninstall entry in **Settings › Apps**
- puts `memguard` on your user PATH, so agents and scripts can call it (open a new terminal to pick it up), unless you switched that off

If you had MemCap or Ather MemGuard 0.1, the installer stops it, replaces it and keeps your settings. Running the same version again repairs it.

**Run without installing** on the setup window runs it from where it is (it leaves an `AtherMemGuard.portable` file next to the exe).

**For scripts:** `AtherMemGuard-Setup-<version>.exe --install` installs without asking. Add `--no-path` to leave PATH alone, `--no-startup` to not start it at sign-in and `--no-start` to not start it now.

## Use it

| Where | What it does |
|---|---|
| Tray icon | Click: how memory is doing, **Free memory**, **Pause 30 min**, **Settings**. Right-click: the menu, with **Memory history** and **Run a checkup**. Double-click: Settings |
| **Ctrl+Alt+M** | Frees memory for Unreal from anywhere |
| `memguard status` | What's running, memory pressure, per-group use, advice |
| `memguard wait --for build --prime` | Waits until there's room for a build; agents call this before a local build or cook |
| `memguard advise` | Suggested `-MaxParallelActions` for the memory available now |

Settings has a window (tray menu → Settings); changes apply as you make them. The file behind it is `%APPDATA%\AtherMemGuard\settings.toml`. If your team has a settings file, load it with **Load team file…** under Team and backup. The README explains every rule.

## Update

From 0.2 the app updates itself. Once a day it checks this repository's GitHub releases for a newer version, downloads it, checks its size and SHA-256, swaps it in and restarts. Your settings and log are kept. If your PC can't reach github.com, it keeps the version it has.

Running a newer `AtherMemGuard-Setup-<version>.exe` works too.

To use your team's own update server, set `manifest_url` under `[updates]` in the settings to the address of its `latest.json`.

## Uninstall

Go to **Settings › Apps › Installed apps › Ather MemGuard › Uninstall** and confirm, or run `memguard uninstall`. Apps go back to normal priority, and the program, the Start menu shortcut and the PATH entry are removed. Your settings, history and log (`%APPDATA%\AtherMemGuard`) are kept.

## Portable use

Unzip `AtherMemGuard-<version>-portable.zip` anywhere and run `AtherMemGuard.exe`. The `AtherMemGuard.portable` file next to it makes it run from that folder instead of installing. Delete that file if you'd rather it install.

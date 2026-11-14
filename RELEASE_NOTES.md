# Ather MemGuard 0.2.0 (Windows)

**Get it:** download `AtherMemGuard-Setup-0.2.0.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **0.1 (or MemCap):** run the setup exe once. It stops the old copy, replaces it and keeps your settings. After that, updates come in the app.
- **Agents and scripts:** `memguard` is still on your PATH, and the setup exe writes it for you, so there's only one file to download.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.0-portable.zip` runs from any folder without installing.

## What's new

### One file to install, and updates in the app
- **One setup exe:** `AtherMemGuard-Setup-<version>.exe` is the app itself. Run it and it installs both programs, adds a **Start menu** shortcut and an entry in **Settings › Apps**, and starts.
- **Updates itself:** the app checks GitHub for a newer version, downloads it next to itself, checks its size and SHA-256, swaps it in and restarts. Nothing is installed if the download doesn't match.
- **Your own update server:** `[updates] manifest_url` in the settings points the app at another `latest.json`, for teams that host their own builds.
- **Uninstall the usual way:** Settings › Apps › Ather MemGuard › Uninstall. Settings and the log are kept.
- **Silent install for scripts:** `AtherMemGuard-Setup-<version>.exe --install [--no-path] [--no-start]`.

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

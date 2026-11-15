# Ather MemGuard 0.2.4 (Windows)

**Get it:** if you have 0.2.x, it updates itself within a day (or right away from the tray menu: **Check for updates**). New to it? Download `AtherMemGuard-Setup-0.2.4.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.4-portable.zip` runs from any folder without installing.

## What's new

### Free disk space, with undo
- **Clean what the disk checkup found:** `memguard disk clean <id>` (or the checkup rows in the Doctor window). Regenerable build output and caches (Unreal Intermediate and DDC, crash and shader-debug files, npm, pip, uv, NuGet and GPU shader caches) are deleted after you say yes; anything you might want back goes to Ather MemGuard's bin on the same drive instead, so it costs no copy and no time.
- **Undo:** `memguard disk undo` puts back what the last cleanup moved to the bin. `memguard disk bin` shows what's in it and `--empty` empties it for good.
- **What it frees, measured:** after a cleanup MemGuard watches the drive's free space for up to 30 minutes and tells you how much it actually rose, next to the prediction. On a Dev Drive the space can take a while to come back.
- **Safe by default:** it never touches a folder in `[disk] keep`, a folder git tracks files in, a project an editor, build or cook is using, or anything that changed since the checkup. Links are never followed, and what's shown is exactly what goes.

### Move caches off C:
- `memguard disk relocate` shows where your Unreal DDC, NuGet, npm, pip and vcpkg caches live and how much they hold, and suggests a drive with room. `--apply` points their future writes there with user environment variables (no admin), and `--undo` points them back. Nothing is copied: the old folders stay until you clean them. Restart terminals and the editor afterwards.

### For agents
- **`memguard_disk_clean`** deletes regenerable items from a plan the same MCP server just made with `memguard_disk_plan`, and nothing else. Moving to the bin, relocating and anything to review stay with the person. `memguard_disk` lists recent cleanups with their measured results. See `docs/AI-AGENTS.md`.

### Fixes
- A folder too large to count in time (a 113 GB DDC) shows as "at least" instead of a much smaller number.
- Low-disk toasts for different drives no longer replace each other.

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

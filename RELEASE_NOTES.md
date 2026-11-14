# Ather MemGuard 0.2.3 (Windows)

**Get it:** if you have 0.2.x, it updates itself within a day (or right away from the tray menu: **Check for updates**). New to it? Download `AtherMemGuard-Setup-0.2.3.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.3-portable.zip` runs from any folder without installing.

## What's new

### Disk space and your Dev Drive
- **Watches free space** on your Dev Drive, the system drive and every drive that holds a project, the Unreal cache (DDC, Zen) or TEMP, once a minute. The flyout shows one line, "D: Dev Drive · 446 GB free", in amber or red when a drive gets low, and counts any other low drive.
- **A toast when a drive gets low**, one per drive and once per change: "Your Dev Drive is almost full. D: has 38 GB free, and a cook usually needs 100 GB." When C: is low it also says why it matters for memory: Windows may not be able to grow the page file.
- **Dev Drive health** in the checkup and `memguard disk`: whether the drive is trusted, whether Defender's performance mode is on, and whether your Unreal and toolchain caches sit on C:. Fixes that need an administrator are shown as the exact command to run; MemGuard changes none of these itself.
- **Disk checkup:** `memguard disk checkup --path D:\lanes\my-lane` tells you what a project, lane or cache holds and how much deleting it would free. On a Dev Drive many files share space through block cloning, so a 659 GB lane can free 91 GB: MemGuard counts shared and hard-linked data once and says how sure each number is. It only reports; it never deletes or moves anything.
- **Builds and cooks wait for disk space too:** `memguard wait --for cook` also checks free space on the project's drive (100 GB for a cook, 50 for a build, 20 for the editor by default; change it with `disk_gb` under `[prime]`). Disk space doesn't free itself, so a shortfall answers at once instead of waiting.
- **For agents:** `memguard_disk`, `memguard_disk_checkup` and `memguard_disk_plan` are new read-only MCP tools, and `memguard_wait` and `memguard_status` include disk. See `docs/AI-AGENTS.md`.
- **Good to know:** on a Dev Drive, space from deleted files can take a long time to show up as free, and can come back in bulk later. "Frees" numbers are predictions, and MemGuard labels them that way.

### Act on what holds your RAM
- **Click a row in the flyout** for what can be done about it: end leftover Unreal helpers, free one app group now, close an app or hold it back with a group (with Undo), or empty Windows' caches. Results come back as toasts.
- **Which drivers use it?** `memguard drivers` and a new report page break "Windows and drivers" into kernel pools, driver code and Windows processes, with the drivers that likely own the biggest pool tags. No admin rights needed.

### Fixes
- The flyout's advice line wraps to four lines instead of being cut off.

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

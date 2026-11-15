# Ather MemGuard 0.2.5 (Windows)

**Get it:** if you have 0.2.x, it updates itself within a day (or right away from the tray menu: **Check for updates**). New to it? Download `AtherMemGuard-Setup-0.2.5.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.5-portable.zip` runs from any folder without installing.

## What's new

### Memory and disk space, apart
- **The Checkup window has two tabs: Memory and Disk space.** Each has its own list, its own total ("frees about 0.3 GB of RAM" or "frees about 16.4 GB on D:"), its own button (**FREE MEMORY** or **FREE DISK SPACE**) and its own Undo, so freeing RAM and freeing disk space never mix in one click.
- The Disk space tab groups items by drive ("ON C:", "ON D:") and holds the drive notices (Dev Drive almost full, Defender performance mode, caches on C:). With no disk checkup yet, it offers to run one.
- **The tray menu** has **Free up memory…** and **Free up disk space…** instead of "Run a checkup". The low-disk notification gets a **Free up disk space** button that opens the Disk space tab.
- Each tab's count is what you can act on, and matches the number at the top.

### Fixes
- **The disk checkup no longer says "frees 0" for what it didn't get to count.** A big cache on one drive could use up the checkup's time before another drive was reached, and every item there read "at least 0 MB". It now counts project build files before large shared caches, across all drives. An item it didn't reach says "not counted". One it only sized says about its size on NTFS, or "up to" its size on a Dev Drive, where files can share space. `memguard disk plan` gives the exact number.
- An older checkup result no longer turns "up to 16 GB" into "about 0 MB".

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

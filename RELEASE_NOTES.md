# Ather MemGuard 0.2.1 (Windows)

**Get it:** if you have 0.2.0, it updates itself within a day (or right away from the tray menu: **Check for updates**). New to it? Download `AtherMemGuard-Setup-0.2.1.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.1-portable.zip` runs from any folder without installing.

## What's new

### Lighter in the background
- **About half the CPU** in the tray: each check reads every process in one call instead of opening each one, and only sets an app's priority when it needs to change (and again every minute, for apps that reset their own).
- **Settings window:** the blinking cursor no longer repaints the whole window, so keeping Settings open costs much less.
- **Memory history window:** reads only the new samples instead of the whole day again, and past days once.

### Fewer false alarms
- **Editor growth alert:** no longer fires while the editor is loading a project. It waits until the editor has run for 10 minutes, needs 20 minutes of samples, and only warns when memory keeps growing, not after a single jump. A restarted editor gets its own count.

### Fixes
- Apps MemGuard can't control (ones running as administrator) are left out of app groups, their totals and the checkup's suggestions, instead of being counted and then failing.
- Moving a group from **Low** or **Very low** CPU to **Efficiency mode** now gives its apps their normal priority back.
- An app that matches two groups follows the first one instead of switching back and forth.
- The log says how many apps were actually trimmed ("trimmed 18 of 22 processes").
- Settings and team files that use the `[disk]` settings of the next version load in this one.

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

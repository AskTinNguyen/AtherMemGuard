# Ather MemGuard 0.2.2 (Windows)

**Get it:** if you have 0.2.0, it updates itself within a day (or right away from the tray menu: **Check for updates**). New to it? Download `AtherMemGuard-Setup-0.2.2.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.2.2-portable.zip` runs from any folder without installing.

## What's new

### See where all your RAM goes
- **The tray flyout accounts for every GB in use.** "RAM in use 92.6 of 96 GB" sits over one bar split by what holds it, and the list under it adds up to the total: Unreal (editor, compile workers, other), **Other apps** with the biggest ones named, each app group, **Windows file cache**, **Windows and drivers**, memory Windows squeezed, and what Windows can't say whose it is.
- **Unreal is shown as RAM in use**, like everything else, with what Windows has promised it on the line below.
- **When memory is critical**, a red box says what to do and what it gets back: "Emptying Windows' file cache gets about 31.5 GB back, closing Rider and Visual Studio about 3.7 GB."
- **Commit free** moved to the bottom, with what it means: room left for apps to ask for more memory (RAM plus the page file). When it runs out, apps crash even if RAM is free.
- `memguard status` prints the same breakdown, and `memguard status --json` has it under `ram` (without app names).

### Empty Windows' file cache
- Windows keeps file data it has read in RAM, and that doesn't count as free. On a busy Unreal PC it can hold tens of GB: on one 96 GB machine it held 44 GB.
- **Empty it now** under "Windows file cache" in the flyout, **Empty Windows file cache…** in the tray menu, or `memguard flush-cache` empties it. On that machine RAM free went from 31 to 73 GB.
- Windows asks for administrator permission each time, and nothing does this on its own. Nothing is lost: Windows reads the files again when they're needed.

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

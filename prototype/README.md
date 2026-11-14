# MemCap

A small tray app (25 KB, no installer framework, no admin rights) that keeps background apps from
crowding Unreal Editor out of RAM. Runs on any Windows 10/11 PC (uses the built-in .NET Framework 4.8).

## Install on a PC

1. Copy `MemCap.exe` to the PC (anywhere, e.g. Downloads).
2. Open a terminal in that folder and run:

   ```
   MemCap.exe install
   ```

   This copies it to `%LOCALAPPDATA%\Programs\MemCap\`, adds it to your sign-in startup
   (`HKCU\...\Run`, current user only), and starts it. A blue icon appears in the tray.

Unsigned exe: the first run may show a SmartScreen "Windows protected your PC" prompt
(More info → Run anyway).

## Use

Tray icon, right-click:
- **Status**: RAM and committed memory per group.
- **Pause / Resume**: pausing puts every app back to normal memory priority.
- **Edit settings**: opens `%LOCALAPPDATA%\MemCap\memcap.ini` in Notepad. Saved changes apply within 15 s.
- **Open log**: `%LOCALAPPDATA%\MemCap\memcap.log` (trims and errors).
- **Exit**: stops and restores normal memory priority.

Command line: `MemCap.exe stop` stops a running copy. `MemCap.exe uninstall` stops it, removes it
from startup and deletes the program (settings and log stay).

## What it does

Every 15 s, for each group in `memcap.ini`:

| Setting | Effect | Risk |
|---|---|---|
| `priority = Low / VeryLow` | Windows evicts these apps' pages from RAM before Unreal's | None |
| `trimAboveMB = N` | When the group's RAM is over N MB, its memory is pushed to the compressed store / page file | Brief lag when you switch back to the app |
| `jobMemoryMB = N` | Hard cap on the group's committed memory (Job Object) | App crashes when it hits the cap. Sandboxed Chrome/Electron helpers can't be capped. Off by default |
| `processMemoryMB = N` | Hard cap per process | Same as above. Off by default |

Default groups: Chrome + Edge (Low, trim above 4 GB); Slack, ChatGPT, Wispr Flow, Discord, Teams
(VeryLow, trim above 1.5 GB); Claude (Low, no trim). Never put Unreal tools in a group
(UnrealEditor, dotnet/UnrealBuildTool, cl, link, git used by builds).

Note: priority and trimming free physical RAM, not commit charge. If a PC runs out of commit
("out of memory" with RAM free), make the page file bigger.

## Rebuild from source

```
%WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe /nologo /target:winexe /optimize+ /out:MemCap.exe /r:System.Windows.Forms.dll /r:System.Drawing.dll MemCap.cs
```

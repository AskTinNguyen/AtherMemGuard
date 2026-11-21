# Ather MemGuard 0.3.0 (Windows)

**Get it:** if you have 0.2.x, it updates itself within a day (or right away from the tray menu: **Check for updates**). New to it? Download `AtherMemGuard-Setup-0.3.0.exe` below and run it. It installs for you only, in a few seconds, with no admin rights, and starts in the tray.
- **SmartScreen:** the exe isn't code-signed yet, so SmartScreen asks once: **More info › Run anyway**.
- **Portable:** `AtherMemGuard-0.3.0-portable.zip` runs from any folder without installing.

## What's new

### Half the size, a fraction of the idle footprint
- **Setup exe cut by >50%** (from ~8.4 MB to ~4.0 MB) and **portable zip cut by >50%** (from ~4.7 MB to ~2.3 MB) via native Windows LZNT1 compression of the embedded CLI, feature-gated builds, `panic = "abort"`, and removing duplicate dependencies.
- **Zero 1 Hz idle polling:** the tray now sleeps on an event-driven `MsgWaitForMultipleObjects` + coalescable timer loop, caches startup/tooltip state, buffers CSV writes, compacts the 24-hour memory history ring to under 400 KB, and shuts down GDI+ + trims itself when idle.
- **Smooth UI rendering:** eliminated per-mousemove full-window DIB allocations in Checkup, added dirty-hover gating across every window, batched History chart drawing with `GdipDrawLines`, cached the supersampled logo, and stopped 10 Hz static toast repaints.

### Smarter, gentler Windows memory management
- **Foreground awareness:** the app you are actively using (and its process group) gets a grace period before trimming or throttling, eliminating stutters when alt-tabbing to a browser or chat app during Unreal work.
- **RAM pressure vs. commit pressure, separated with hysteresis:** low commit no longer triggers futile working-set trims, and hysteresis stops Rapid Tight/Critical flapping.
- **Surgical private-RAM trimming & kernel primitives:** group caps measure private working set instead of double-counting shared DLLs, trim only the largest private-RAM holders, lower background I/O priority (`IoPriorityVeryLow`), apply proper Windows 11 EcoQoS, trim suspended compile workers, react in milliseconds via `LowMemoryResourceNotification`, and support optional working-set caps, Job CPU rate control, low-priority standby purges, memory page combining, and an opt-in elevated helper mode (zero-admin remains the default).
- **UE 5.4+ workload detection:** recognizes `UbaHost`, `UbaAgent`, `LiveCodingConsole`, `UnrealHeaderTool`, `UnrealLightmass`, `UnrealPak`, `dxc`/`fxc`/`ispc`, and heavy commandlets (`DerivedDataCache`, `ResavePackages`, `BuildLighting`).

### Faster ReFS / Dev Drive & first-class Git disk cleaner
- **Order-of-magnitude faster scans & cleanups:** single-pass NTFS/ReFS traversal with relative `NtCreateFile` opens, packed 16-byte zero-allocation extent sweep, persistent cross-volume worker pool, O(1) `.AtherMemGuard-bin` size manifests (eliminating 60-second tray bin walks), batched `git ls-files -z`, and multi-threaded `\\?\` deletion with Windows POSIX delete semantics.
- **First-class Git & Dev Drive management:** discovers all Git repos (not just `.uproject`), adds `git lfs prune --verify-remote --dry-run` + `tool` pruning, Git object DB / maintenance / `git worktree prune` diagnostics and actions, Git 2.48+ relative `gitdir:` support, VHD/VHDX Dev Drive backing & compaction guidance, `FSCTL_GET_REFS_VOLUME_DATA` + ReTrim insights, per-volume minifilter audits, 64 KB cluster slack alerts, and daily disk burn-rate trends (`disk-trend.json`).
- **Expanded developer & Windows cache catalog + relocator:** adds Unreal UBA, VaultCache, SwarmCache, Engine Intermediate/DDC, PDB bloat, Zen GC/relocation, Cargo, pnpm/yarn/bun, `uv`, VS `ipch`, Rider, NuGet, vcpkg, `sccache`, WSL2/Docker VHDX diagnostics, AMD/Intel/NVIDIA shader caches, loose `%TEMP%` files, and Windows crash dumps/WER/WinSxS reporting.

### Settings, CLI & MCP upgrades
- **Disk space & Checkup in Settings:** full `[disk]` and `[doctor]` configuration right inside the Settings window.
- **Daemon IPC:** `memguard pause`, `memguard resume`, and `memguard reload` talk directly to the running tray over a scoped named pipe, alongside a shared-memory live status channel.
- **Upgraded MCP server:** cancellable via `notifications/cancelled`, cached schemas, and 5 new read-only tools (`memguard_drivers`, `memguard_disk_relocate`, `memguard_disk_bin`, `memguard_config`, `memguard_session_summary`).

### Fixes
- Fixed ReFS `sharing_groups` threshold using bytes instead of clusters (restoring sharing-group detection on Dev Drives).
- Cleared Job Object hard caps properly on Pause, Reload, and Quit; fixed `Add group…` and `Add app…` in Settings; fixed hard-link + block-clone RefCount math; fixed `planned_last_write` vs. `newest_write` refusal loops; keyed tracked processes by `(pid, created)` to eliminate PID-reuse bugs; fixed `team.toml` `manifest_url` & custom team HTTPS updater hosts; synchronized logging and atomic file writes across processes.

## Checksums

`SHA256SUMS.txt` lists the SHA-256 of the setup exe and the portable zip.

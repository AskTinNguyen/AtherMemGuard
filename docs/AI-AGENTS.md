# Ather MemGuard for AI agents

`memguard mcp` lets coding agents (Claude Code, Codex and any other MCP client) call Ather MemGuard
as tools. It is an MCP server over **stdio**: the agent starts `memguard mcp` itself when it needs it
and talks to it over stdin and stdout. There is no port, no token and nothing to turn on in the app.

The tools do the same things as the `memguard` commands in the README (`memguard_doctor` is the
read-only checkup, `memguard doctor --json`; `memguard_ram` the read-only RAM and BIOS check,
`memguard ram --json`; `memguard_disk`, `memguard_disk_checkup` and `memguard_disk_plan` the read-only
drive and Dev Drive reports, `memguard disk ... --json`) and return the same JSON as
their `--json` output. Without the background app running, `memguard_status` still works (it reports
`daemon: false`), and priming and waiting still work; only the continuous enforcement is missing.

## Set up

Run this to get the exact commands for this PC, with the full path to `memguard.exe` filled in:

```
memguard agent-setup
```

**Copy setup** in the app's settings (AI agents) puts the same text on the clipboard.

### Claude Code

For all your projects:

```
claude mcp add --scope user ather-memguard -- memguard mcp
```

If `memguard` is not on your PATH (it is when you installed with `--add-to-path`), use the full path:

```
claude mcp add --scope user ather-memguard -- "%LOCALAPPDATA%\Programs\AtherMemGuard\memguard.exe" mcp
```

To share it with everyone working in a repository, put this in `.mcp.json` at the repo root instead:

```json
{ "mcpServers": { "ather-memguard": { "command": "memguard", "args": ["mcp"] } } }
```

Check it with `claude mcp list`, or `/mcp` inside Claude Code.

### Codex

Add this to `%USERPROFILE%\.codex\config.toml`:

```toml
[mcp_servers.ather-memguard]
command = "memguard"            # or the full path, in single quotes: 'C:\...\memguard.exe'
args = ["mcp"]
tool_timeout_sec = 3700         # memguard_wait can block for up to an hour
```

### Other clients

Any client that speaks MCP over stdio works: command `memguard`, argument `mcp`. The server speaks
protocol version `2025-06-18` and also accepts `2025-03-26` and `2024-11-05`.

### Timeouts

`memguard_wait` blocks until memory is ready or its `timeout_sec` passes (at most 3600). Give the
client a tool timeout longer than the waits you ask for: in Codex set `tool_timeout_sec` as above; in
Claude Code set the `MCP_TOOL_TIMEOUT` environment variable (milliseconds) if waits get cut off. When
the client sends a progress token, the server reports progress every 2 seconds while it waits.

## Before a local build or cook

Call `memguard_wait` with `prime: true` and the workload, and start the job only when it returns
`ready: true`:

```json
{ "name": "memguard_wait", "arguments": { "workload": "build", "prime": true, "timeout_sec": 900 } }
```

Then, for a build, call `memguard_advise` and pass its `suggested_max_parallel_actions` to
UnrealBuildTool as `-MaxParallelActions=N`.

If `ready` is `false`, don't start anyway: tell the user, and show `memguard_status` so they can see
what is holding memory (often the editor, or many browser tabs). This is the same as the shell flow
`memguard wait --for build --prime --timeout 900 && RunUAT.bat BuildCookRun ...`.

Before a cook, also pass the project folder, so the drive it cooks to is checked:

```json
{ "name": "memguard_wait", "arguments": { "workload": "cook", "prime": true, "disk_path": "D:\\s2" } }
```

If `blocked_by` contains `disk`, the call returned at once (disk space doesn't free itself): don't
start. Call `memguard_disk_checkup` with the same path and relay it in two lines, quoting only
predicted totals (see "Quoting disk numbers" below). Removing anything stays with the person.

A line like this in your project's `CLAUDE.md` or `AGENTS.md` makes agents follow it:

> Before any local Unreal build, cook or package, call the `memguard_wait` tool with `prime: true`
> and the workload, and only start when it returns `ready: true`. Use `memguard_advise` for
> `-MaxParallelActions`.

## Tools

Each tool returns its result twice: as pretty-printed JSON in a text block, and as
`structuredContent`. A bad argument returns a result with `isError: true` and a message saying what
to fix; an unknown tool name is a protocol error (-32602).

### `memguard_status`

Read-only. Free RAM and commit, pressure (`normal`, `elevated`, `critical`), what Unreal is doing,
what each app group holds and what MemGuard does to it, whether the background app is running, and
advice. Same as `memguard status --json`. No arguments.

```json
{ "name": "memguard_status", "arguments": {} }
```

Example result (shortened):

```json
{
  "daemon": true,
  "paused": false,
  "mode": "build",
  "pressure": "normal",
  "activities": { "editor": false, "build": true, "cook": false },
  "system": { "ram_total_gb": 127.4, "ram_free_gb": 31.3, "commit_free_gb": 30.6, "...": "..." },
  "ue": { "workers": 18, "workers_committed_gb": 55.7, "total_committed_gb": 59.2, "...": "..." },
  "groups": [ { "name": "Browsers", "processes": 44, "ram_mb": 1287, "policy": { "priority": "low", "trim_above_mb": 3072, "...": "..." } } ],
  "advice": [ "..." ],
  "disk": { "level": "normal", "volumes": [ { "volume": "D:", "free_gb": 497.2, "free_pct": 26.1, "level": "normal", "roles": ["dev_drive", "projects"] },
                                            { "volume": "C:", "free_gb": 293.1, "free_pct": 15.4, "level": "normal", "roles": ["system", "pagefile", "ddc", "zen", "temp"] } ] }
}
```

`daemon: false` means the background app is not running, so nothing is being enforced between calls.
`disk` lists the watched drives with free space available to this user and their level; it is left
out when `[disk] enabled = false`. `memguard_disk` has the details.

### `memguard_prime`

Frees RAM for Unreal now: background app groups (browsers, chat, agent apps) get low memory priority
and their memory is pushed to the page file. Nothing is closed and no data is lost; those apps may lag
briefly when next used. Unreal's own processes are never touched. Same as `memguard prime --json`.

| Argument | Type | Meaning |
|---|---|---|
| `workload` | `"editor"`, `"build"` or `"cook"` | Also report whether the PC now has the headroom this workload needs (`readiness`) |
| `kill_orphans` | boolean, default false | End leftover Unreal helper processes whose parent has exited |

```json
{ "name": "memguard_prime", "arguments": { "workload": "cook" } }
```

Example result (shortened):

```json
{
  "trimmed_processes": 61,
  "ram_freed_gb": 4.2,
  "groups_ram_before_gb": 9.8,
  "groups_ram_after_gb": 2.1,
  "before": { "ram_free_gb": 27.1, "...": "..." },
  "after": { "ram_free_gb": 31.3, "...": "..." },
  "orphans": [],
  "readiness": { "target": "cook", "need_ram_gb": 24.0, "need_commit_gb": 40.0, "ready": false },
  "advice": [ "..." ]
}
```

Before a build or cook, prefer `memguard_wait` with `prime: true`, which primes and then waits.

### `memguard_wait`

Blocks until the PC has enough free RAM and commit, or the timeout passes. Same as
`memguard wait --json`. Give `workload`, or `ram_gb` / `commit_gb`, or both (the numbers override the
workload's figures from `settings.toml` `[prime]`).

| Argument | Type | Meaning |
|---|---|---|
| `workload` | `"editor"`, `"build"` or `"cook"` | Use the headroom configured for this workload |
| `ram_gb` | number | Free RAM needed, in GB |
| `commit_gb` | number | Free commit needed, in GB |
| `timeout_sec` | integer, default 600 | Give up after this many seconds; values over 3600 are cut to 3600 |
| `prime` | boolean, default false | Prime once before waiting |
| `disk_gb` | number | Free disk space needed on the project's drive, in GB. Default: the workload's `disk_gb` (editor 20, build 50, cook 100) |
| `disk_path` | string | The project folder whose drive is checked. Default: the server's current folder |

When there is a disk need, the drives holding Unreal's DDC, Zen and TEMP are also checked against
`[disk] cache_min_free_gb` (20 GB), each drive once. A shortfall returns at once, before priming.

```json
{ "name": "memguard_wait", "arguments": { "workload": "build", "prime": true, "timeout_sec": 900 } }
```

Example result:

```json
{ "ready": true, "waited_sec": 4, "ram_free_gb": 31.3, "commit_free_gb": 30.6, "need_ram_gb": 16.0, "need_commit_gb": 24.0,
  "blocked_by": [], "disk_volume": "D:", "disk_free_gb": 497.2, "need_disk_gb": 50.0, "disk_checks": [ "..." ] }
```

Blocked by the disk (this PC, `disk_gb: 5000`, so it can't be met):

```json
{ "ready": false, "waited_sec": 0, "blocked_by": ["disk"], "disk_volume": "D:", "disk_free_gb": 497.2, "need_disk_gb": 5000.0,
  "disk_checks": [ { "volume": "D:", "free_gb": 497.2, "need_gb": 5000.0, "roles": ["project"], "ok": false },
                   { "volume": "C:", "free_gb": 293.1, "need_gb": 20.0, "roles": ["ddc", "zen", "temp"], "ok": true } ], "...": "..." }
```

A timeout is not a tool error: the result has `ready: false` and `blocked_by: ["memory"]`.

### `memguard_advise`

Read-only. Suggests compile parallelism for the memory available now, with notes. Same as
`memguard advise --json`. No arguments.

```json
{ "name": "memguard_advise", "arguments": {} }
```

Example result:

```json
{
  "mode": "build",
  "pressure": "normal",
  "logical_cores": 24,
  "ram_budget_gb": 38.5,
  "commit_free_gb": 30.9,
  "suggested_max_parallel_actions": 10,
  "notes": [ "A 24-wide compile could need about 72 GB at 3 GB per action, and 31 GB is available. Limit UnrealBuildTool with -MaxParallelActions=10 ..." ]
}
```

The 3 GB per compile action is a starting estimate, not a measurement of your project.

### `memguard_doctor`

Read-only checkup: which running apps use memory that Unreal work doesn't need, startup entries worth
switching off, leftover Unreal helpers and PC-level problems, and how much could be freed. Same as
`memguard doctor --json`. No arguments. **It changes nothing, and the tool can't fix anything**: relay the
findings to the person in a few lines and ask before acting. The person runs `memguard doctor --fix` (the
recommended ones) or `memguard doctor --fix <id>[:action]`; `memguard doctor --undo` reverses it. Don't
close or end apps yourself.

```json
{ "name": "memguard_doctor", "arguments": {} }
```

Example result (shortened):

```json
{
  "time": "2026-11-14 15:57:27",
  "unreal_active": true,
  "system": { "ram_total_gb": 127.4, "ram_free_gb": 48.6, "commit_free_gb": 43.4, "...": "..." },
  "findings": [
    {
      "id": "app:steam", "kind": "app", "verdict": "close", "title": "Steam",
      "detail": "Game launcher. Not used by Unreal. It's running in the background with no window open.",
      "processes": 8, "ram_mb": 98, "committed_mb": 1433,
      "actions": [ { "kind": "close", "label": "Close", "undo": "Start the app again", "needs_force": false },
                   { "kind": "block_while_unreal", "label": "Close whenever Unreal is running", "...": "..." } ],
      "recommended": true, "category": "Game launcher", "names": ["steam", "steamwebhelper"], "pids": [1234, 5678]
    },
    { "id": "startup:hkcu:Steam", "kind": "startup", "verdict": "close", "title": "Steam",
      "actions": [ { "kind": "disable_startup", "label": "Don't start with Windows", "...": "..." } ], "recommended": true, "...": "..." },
    { "id": "sys:pagefile", "kind": "system", "verdict": "review", "title": "The page file is small for this much RAM", "actions": [], "...": "..." }
  ],
  "reclaimable_ram_gb": 0.2,
  "reclaimable_commit_gb": 2.0
}
```

Verdicts: `close` (not needed for Unreal; closing it is safe), `hold_back` (wanted, but should give way:
put it in a MemGuard group), `review` (unknown and large, or it depends), `heavy_tool` (a big tool that is
probably deliberate). `keep` items are never listed. Findings with no actions are information only.
`reclaimable_*` counts the `close` apps and Unreal leftovers, not heavy tools or unknown apps.

A good relay is two or three lines: what could be freed, the recommended actions, and the command to run
them, for example: "About 2 GB is held by Steam and Copilot running in the background, and Steam starts
with Windows. `memguard doctor --fix` closes them and switches Steam's startup off; `memguard doctor
--undo` reverses it. Want me to run it?" Only run `--fix` after the person says yes, and never with
`--force` unless they asked for that app to be ended.

### `memguard_history`

Read-only. What memory did over the last hours, as the background app recorded it: one sample per
interval (every 10 seconds by default). Use it to explain a slow or failed build after the fact, to see
whether the editor keeps growing, or to check how long memory has been tight. The samples are the same
as `memguard history --json` gives.

| Argument | Type | Meaning |
|---|---|---|
| `hours` | number, default 1 | How far back to look; values over 168 (a week) are cut to 168 |

```json
{ "name": "memguard_history", "arguments": { "hours": 6 } }
```

Example result (shortened):

```json
{
  "hours": 6.0,
  "total_samples": 2160,
  "step": 3,
  "samples": [
    {
      "unix": 1794744000, "time": "2026-11-14 12:00:00", "mode": "editor+build", "pressure": "elevated",
      "ram_free_gb": 11.3, "commit_free_gb": 21.0, "ue_committed_gb": 61.4, "editor_committed_gb": 58.2,
      "workers": 18, "groups": [ { "name": "Browsers", "ram_gb": 4.1 } ]
    }
  ]
}
```

More than 720 samples are thinned evenly: `step` says every how many samples one is kept, and the
newest sample is always included. An empty `samples` list means nothing was recorded for that span
(the background app wasn't running, or history is off in settings).

### `memguard_ram`

Read-only RAM and BIOS check: whether this PC's memory runs below the speed its kit is rated for (the
XMP / EXPO profile is switched off in the BIOS), whether two sticks are in the right slots, whether the
BIOS may be out of date, and CPU-specific cautions. Same as `memguard ram --json`. No arguments. It reads
the firmware's SMBIOS tables and the CPU's microcode version, and **changes nothing**: only the person can
change BIOS settings.

```json
{ "name": "memguard_ram", "arguments": {} }
```

Example result (shortened; a PC whose kit runs below its rated speed):

```json
{
  "verdict": "below_rated",
  "headline": "Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz.",
  "summary": "The kit's fast profile (EXPO) is switched off in the BIOS, so the memory runs at its slower default. ...",
  "sticks": [
    { "slot": "DIMM_A2", "populated": true, "size_gb": 16.0, "kind": "DDR5", "form": "DIMM", "manufacturer": "G.Skill",
      "part_number": "F5-6000J3038F16G", "module_mhz": 4800, "configured_mhz": 4800, "voltage": 1.1, "rated_mhz": 6000, "...": "..." },
    { "slot": "DIMM_A1", "populated": false, "...": "..." }
  ],
  "slots_total": 4,
  "rated": { "speed_mhz": 6000, "cl": 30, "timings": "30-38-38", "voltage": 1.35, "voltage_is_typical": true,
             "profile_kind": "EXPO", "confidence": "part_number" },
  "current_mhz": 4800,
  "board": { "maker": "ASUS", "model": "ROG STRIX B650E-F GAMING WIFI", "bios_version": "1813", "bios_date": "2023-10-05", "bios_age_months": 37 },
  "cpu": { "name": "AMD Ryzen 7 7800X3D 8-Core Processor", "vendor": "AMD", "microcode": null },
  "findings": [
    { "id": "ram:speed", "severity": "problem", "title": "Your RAM is running below its rated speed", "detail": "..." },
    { "id": "ram:bios", "severity": "caution", "title": "The BIOS may be out of date", "detail": "..." },
    { "id": "ram:cpu", "severity": "caution", "title": "Keep SoC voltage at or under 1.3 V", "detail": "..." }
  ],
  "steps": [ "Before you start: save your work ...", "Restart the PC and press Del or F2 ...", "..." ],
  "fallback_mhz": 5600,
  "fingerprint": "..."
}
```

Verdicts: `below_rated` (at least 200 MHz under the rated speed: worth acting on), `at_rated`, `no_profile`
(standard-speed or soldered memory: nothing to turn on), `mixed_kit` (sticks don't match), `unknown` (the kit
couldn't be identified; the finding says what to look up), `not_applicable` (virtual machine, or no memory
reported). `rated.confidence` says where the rated speed came from: `part_number` (decoded) or
`module_reported` (what the modules tell the BIOS). A voltage with `voltage_is_typical: true` is the usual value
for kits like this, not this kit's specification. Findings: `ram:speed`, `ram:mixed`, `ram:slots`, `ram:bios`,
`ram:microcode`, `ram:cpu`, each `problem`, `caution` or `info`.

A good relay is the headline plus one line: "Your RAM is running at 4800 MHz; the kit is rated for 6000.
Turning on EXPO in the BIOS takes about five minutes. Want the steps?" Then give `steps` as they are (they
name this board's menus and this kit's values). Never say you changed anything, and don't invent BIOS settings
the steps don't mention.

### `memguard_disk`

Read-only and fast (no folder is counted). Every drive once, keyed by volume id however it is reached:
free space available to this user, the level (`normal`, `elevated`, `critical`: free space below the
smaller of the GB and percent settings in `[disk]`), roles (`dev_drive`, `system`, `pagefile`,
`projects`, `ddc`, `zen`, `temp`), Dev Drive and trusted state, block cloning, and Dev Drive health
findings with the exact administrator command and how to check it. Same as `memguard disk --json`.

| Argument | Type | Meaning |
|---|---|---|
| `refresh` | boolean, default false | Read the Defender and Dev Drive facts again (they are kept for a day) |

```json
{ "level": "normal",
  "volumes": [ { "volume_id": "\\\\?\\Volume{29b74db7-...}\\", "paths": ["D:\\"], "label": "S2DevDrive", "fs": "ReFS",
                 "dev_drive": true, "trusted": true, "block_cloning": true, "backing": "partition", "backed_by": null,
                 "total_gb": 1907.69, "free_gb": 469.52, "free_basis": "available_to_caller", "reserved_gb": 8.68,
                 "free_pct": 24.6, "level": "normal", "trend_gb_per_day": null, "roles": ["dev_drive", "projects"],
                 "notes": ["Deleting on this drive may take a long time to show up as free space."] } ],
  "dev_drive": { "enabled": true, "av_filters": "antivirus_allowed", "defender_performance_mode": "off",
                 "realtime_protection": true, "third_party_av": 0, "defender_checked": "2026-11-14 20:15:02" },
  "findings": [ { "id": "dev:perf_mode_off", "severity": "caution", "title": "Defender's performance mode is off",
                  "detail": "Defender's performance mode is off, so files on your Dev Drive are scanned before they open.",
                  "fix": { "needs_admin": true, "command": "Set-MpPreference -PerformanceModeStatus Enabled",
                           "check": "(Get-MpPreference).PerformanceModeStatus reads 0" } } ],
  "advice": [] }
```

Findings: `dev:not_trusted`, `dev:perf_mode_off`, `dev:av_disallowed` (info), `dev:caches_on_c` (info; it
names another drive only if that drive would still have enough free afterwards), `dev:vhd_host_low` (info),
`dev:third_party_av` (info). Mention them once; only the person can apply the fixes (they need an
administrator), and never claim to have fixed one. Text that can't be read (Windows in another language)
gives `unknown`, never a guess.

### `memguard_disk_checkup`

Read-only and report-only: what could go from a drive and what removing it would really free, counted
without double counting. On a Dev Drive the usual folder sizes are wrong several times over, because lanes,
worktrees and the git LFS store share the same clusters (block clones) and hard links repeat files; the
checkup counts each cluster once, from the drive's own reference counts. Same as
`memguard disk checkup --json`. Lanes and worktrees are listed (report only): removing one stays with the
person (`git worktree remove`, or their own delete).

| Argument | Type | Meaning |
|---|---|---|
| `path` | string | Only this folder: a project, a lane, or a folder of them. Default: `[disk] project_dirs`, projects on Dev Drives, and the shared caches (DDC, Zen, TEMP, npm, pip, NuGet, ...) |
| `refresh` | boolean, default false | Count again instead of answering from the saved result (its age is in `scan.age_sec`) |
| `budget_sec` | integer, default 60 | How long counting may take; at most 300. Progress is reported like `memguard_wait` |

Counting runs in a separate low-priority process, a few threads at a time, and stops early if a build or
cook starts (`scan.stopped_for_build`). Example (shortened; this PC, `path: "D:\\lanes\\sched-cleanup"`,
with the editor open in that lane, which is why everything is `estimate`):

```json
{ "scan": { "time": "2026-11-14 20:22:14", "age_sec": 0, "from_cache": false, "partial": false, "took_sec": 22.4, "files": 845236 },
  "volumes": [ { "paths": ["D:\\"], "fs": "ReFS", "free_gb": 469.52, "basis": "refs_refcount",
                 "notes": ["Deleting on this drive may take a long time to show up as free space.", "..."] } ],
  "items": [
    { "id": "d1", "key": "ue:intermediate", "class": "regenerable", "title": "Build files (Intermediate) for sched-cleanup",
      "logical_gb": 14.1, "footprint_gb": 14.1, "frees_alone_gb": 14.1, "frees_basis": "predicted",
      "frees_confidence": "estimate", "in_use": true, "in_flux": true, "notes": ["Unreal is working in this project"], "...": "..." },
    { "id": "d7", "key": "lane", "class": "review", "title": "Lane sched-cleanup", "excludes": ["D:\\lanes\\sched-cleanup\\Intermediate", "..."],
      "logical_gb": 619.94, "footprint_gb": 621.09, "frees_alone_gb": 51.74, "frees_basis": "predicted", "frees_confidence": "estimate",
      "stays_shared_gb": 569.35, "shared_outside_scan_gb": 569.35, "notes": ["3 links to other folders not followed", "73 hard links counted once", "..."] } ],
  "sharing_groups": [],
  "totals": [ { "set": "Lane sched-cleanup with everything in it", "volume": "D:", "item_ids": ["d7", "d1", "d2", "d3", "d4", "d5", "d6"],
                "frees_together_gb": 91.26, "frees_confidence": "estimate", "frees_basis": "predicted" } ],
  "basis": "predicted",
  "do_not_sum": "..." }
```

The numbers, never added to each other or across items: `logical_gb` (sum of file sizes: "holds"),
`allocated_gb` (Explorer's "size on disk", for comparison only), `footprint_gb` (distinct clusters:
"occupies"), `frees_alone_gb` (clusters whose every reference is inside this item), `stays_shared_gb`,
`shared_with` and `shared_outside_scan_gb` (who keeps the rest), and, for a named set,
`totals[].frees_together_gb`. A folder's numbers exclude the items inside it (`excludes`): each file
belongs to the deepest item.

`frees_confidence`: `exact` (quote as is), `lower_bound` (at least; `frees_max_gb` is the ceiling),
`estimate` (about: from a saved result older than `[disk] fresh_sec`, the tree changed during the count,
or Unreal is working in it), `upper_bound` (at most; never use it to decide there is enough), `unknown`
(could not be measured; the number is null). A total's confidence is its weakest member's; a set with an
`upper_bound` or `unknown` member has no total.

### `memguard_disk_plan`

Read-only. Counts exactly the items you name, fresh, and returns one entry per drive (never a sum across
drives). Same as `memguard disk plan <ids> --json`.

| Argument | Type | Meaning |
|---|---|---|
| `ids` | array of strings | Item ids from the latest `memguard_disk_checkup`, e.g. `["d7"]`. Empty = all of them |


### Quoting disk numbers

- Every "frees" number is **predicted** from the drive's reference counts. It has not been checked against a
  real delete, and on ReFS (Dev Drives) deleted space has been seen to come back much later, and in bulk.
  Say "predicted", and **never promise the space right after a delete**.
- Quote only `totals[].frees_together_gb`, a plan's `frees_together_gb`, or one item's `frees_alone_gb`,
  with its confidence word: `exact` as is, `lower_bound` as "at least", `estimate` as "about",
  `upper_bound` as "at most". Never add numbers yourself. Never present `logical_gb` as space that can be freed.
- To decide whether a set frees enough for a job, call `memguard_disk_plan` and use `free_after_gb`; rely
  only on `exact` or `lower_bound`.
- Nothing here deletes files; the person removes them.

A good relay is two lines: "D: has 38 GB free and a cook needs 100. Removing lanes ready-03 and
mocap-stellar-girl together would free about 94 GB (predicted; on this drive the space can take a while to
show up). Want the plan for those two?"

## Troubleshooting

- **The agent doesn't see the tools.** Run `memguard mcp` in a terminal: it should print one line to
  stderr and wait. Press Ctrl+C to quit. If `memguard` isn't found, use the full path to
  `memguard.exe` in the setup, or reinstall with `memguard install --add-to-path`.
- **`memguard_wait` gets cut off.** Raise the client's tool timeout (see Timeouts).
- **Settings.** The server reads `settings.toml` on every call, so changes apply at once. A broken
  file falls back to the built-in defaults, with a warning on stderr.

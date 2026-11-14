# Ather MemGuard for AI agents

`memguard mcp` lets coding agents (Claude Code, Codex and any other MCP client) call Ather MemGuard
as tools. It is an MCP server over **stdio**: the agent starts `memguard mcp` itself when it needs it
and talks to it over stdin and stdout. There is no port, no token and nothing to turn on in the app.

The tools do the same things as the `memguard` commands in the README and return the same JSON as
their `--json` output. Without the background app running, `memguard_status` still works (it reports
`daemon: false`), and priming and waiting still work; only the continuous enforcement is missing.

## Set up

Run this to get the exact commands for this PC, with the full path to `memguard.exe` filled in:

```
memguard agent-setup
```

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
  "advice": [ "..." ]
}
```

`daemon: false` means the background app is not running, so nothing is being enforced between calls.

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

```json
{ "name": "memguard_wait", "arguments": { "workload": "build", "prime": true, "timeout_sec": 900 } }
```

Example result:

```json
{ "ready": true, "waited_sec": 4, "ram_free_gb": 31.3, "commit_free_gb": 30.6, "need_ram_gb": 16.0, "need_commit_gb": 24.0 }
```

A timeout is not a tool error: the result has `ready: false`.

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

## Troubleshooting

- **The agent doesn't see the tools.** Run `memguard mcp` in a terminal: it should print one line to
  stderr and wait. Press Ctrl+C to quit. If `memguard` isn't found, use the full path to
  `memguard.exe` in the setup, or reinstall with `memguard install --add-to-path`.
- **`memguard_wait` gets cut off.** Raise the client's tool timeout (see Timeouts).
- **Settings.** The server reads `settings.toml` on every call, so changes apply at once. A broken
  file falls back to the built-in defaults, with a warning on stderr.

# Tools and capabilities

Everything yolop can do in one place: the full tool inventory, how the agent
stays oriented in large repos, and how to turn capabilities on or off. For the
short version, see the project README.

## Filesystem

Real workspace disk, with every write checkpointed for `/undo` and `/rewind`:

- `read_file`, `write_file`, `edit_file`
- `list_directory`, `stat_file`, `delete_file`
- `grep_files`, line-oriented search with before/after context

## Orientation

- `repo_map` and `repo_symbols` build an on-demand multi-language symbol
  overview, so the agent orients before targeted grep and read. See
  [Repo map](repo-map/repo-map.md).
- `ast_grep` runs read-only structural pattern search across Rust, Python,
  TypeScript/TSX, JavaScript/JSX, C#, Go, CSS, HTML, and Bash.
- `ast_edit` (optional, off by default) applies pattern rewrites with a
  preview-first `dry_run` flow. Enable with `[[capabilities]] ref = "ast_edit"`
  in `settings.toml`.

## Language servers

Real language servers (rust-analyzer, typescript-language-server, pyright,
gopls, clangd, or any configured binary) behind `lsp_diagnostics`,
`lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_rename`
(workspace-wide), `lsp_symbols`, and `lsp_code_actions`. Optional and off by
default; enable with `[[capabilities]] ref = "lsp"` in `settings.toml`.
Rename a symbol and references follow across files.

## Shell

`bash -lc` from the workspace root, with a 120 s timeout and a 1 MiB
per-stream output cap (overflow spills to the session folder and stays
readable for later tool calls). Run a command directly with
`/shell <command>` or `!<command>`.

## Background tasks

`spawn_background` runs a shell command detached from the current turn, for
example watching CI. It streams to a log, writes a `result.json`, and tracks
a session task you inspect with `list_tasks`, `get_task`, and `cancel_task`,
or through the `/background` command and the `Ctrl+B` activity rail.
Detached commands may run up to 24 hours and their results survive a restart.
`spawn_background` can also schedule one-shot or recurring monitors; when a
schedule fires or a task finishes while the session is idle, yolop proactively
wakes the agent (disable with `proactive_wake`).

## Subagents

`spawn_agent` delegates independent work into child context windows. A
two-level hierarchy supports broad swarms without exceeding the per-session
fan-out limit; `Ctrl+B` shows the live activity rail with branch token and
cost rollups. See [Parallel sub-agents](subagents/subagents.md).

## Session coordination

A purpose-built coordinator profile can discover live, opt-in Yolop workers
in separate Git worktrees, dispatch durable tasks, and receive explicit
completion wakes through the attached `yolop coordination` CLI, without
adding model-tool schemas. Presence and delivery are local and restart-safe;
ordinary sessions start drained. See
[Session coordination](../session-coordination.md).

## Web

`free_web_search`, `web_fetch` (HTTP GET/HEAD with markdown and text
conversion plus DNS-pinned SSRF protection), and `duckduckgo_instant_answer`,
all working without an API key. Set
`EVERRUNS_SYSTEM_ALLOWLIST_ENABLED=true` to restrict `web_fetch` to a curated
allowlist of well-known public resources.

## Plans and goals

- `write_todos` keeps multi-step work on track, with loop detection stopping
  repeated failing calls.
- `/goal` hands the agent a standing objective; it loops under a token budget
  until a separate evaluator confirms the completion condition
  (`/goal clear` stops early, `/goal resume` picks a previous one back up).
- `/btw` asks a side question without touching history.

## Conversation control plane

Broad configuration and administration actions live behind `yolop <route>`
commands instead of model tool schemas, so nested agents and detached shell
descendants connect back to the live session and blocking requests survive
side restarts. This is how the agent installs MCP servers, manages skills,
and coordinates workers when you ask in plain words. See
[Conversation Control Plane](conversation-control-plane/conversation-control-plane.md).

## Turning capabilities on and off

Optional tools stay off until enabled so the model sees a small, relevant
toolbox. Enable a set in `settings.toml`:

```toml
[[capabilities]]
ref = "lsp"
```

or say it in plain words: "turn on LSP for this repo." The same goes for
approvals (`/setup approval <protective|normal|off>`), providers (`/setup`,
`/model`), and editor wiring (`yolop into paseo|zed|buzz`).

## Further reading

- [Approvals](approvals.md) and [Sandboxing](sandboxing/sandboxing.md)
- [Repo map](repo-map/repo-map.md)
- [Parallel sub-agents](subagents/subagents.md)
- [Hooks](hooks.md)
- [Show me](show-me/show-me.md)
- [OKF knowledge](okf/okf.md)
- [Session coordination](../session-coordination.md)
- [Conversation Control Plane](conversation-control-plane/conversation-control-plane.md)
- [Extensions](../extensions.md)

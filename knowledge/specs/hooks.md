---
type: Product Specification
title: Hooks Specification
description: Defines the hooks specification contract for Yolop.
---

# Hooks Specification

Status: v1 implemented for scoped config loading, detached `yolop config hooks`
management, and upstream `user_hooks` registration.

## Why

Hooks let users run deterministic automation at well-defined points in an
agent session without turning that automation into model instructions. Common
uses include rejecting dangerous shell commands, formatting after edits,
recording audit events, or enforcing project-specific checks before a turn
continues.

yolop should not invent a second hook engine. `everruns-core` already ships the
`user_hooks` capability, `UserHookSpec`, and adapters for tool and lifecycle
events. yolop owns only the local config discovery and merge policy for global
and workspace files.

The implementation must therefore route through the upstream mechanism:
`hooks.json` is parsed into the upstream `user_hooks` capability config and
registered with `AgentCapabilityConfig::with_config("user_hooks", ...)`.

## Scope

Two user-authored scopes are supported:

1. **Global**: `<config_dir>/yolop/hooks.json`.
   Personal automation shared across every workspace.
2. **Workspace**: `<workspace>/.agents/hooks.json`.
   Project-owned automation that can be reviewed and committed with the repo.
   It loads only while the user trusts its current contents; see
   [Trust Model](#trust-model).

The config format is JSON because the upstream `user_hooks` capability already
owns a JSON schema for `UserHookSpec`. yolop reads the two files, merges them
into one effective `user_hooks` capability config, then registers
`UserHooksCapability` in the session harness only when the effective config is
non-empty.

Missing files are silent. A malformed file is warned and skipped; it must not
prevent the other scope from loading or prevent yolop from starting.

## Config Shape

Each file uses the same envelope:

```json
{
  "hooks": [
    {
      "id": "format-after-edit",
      "event": "post_tool_use",
      "matcher": { "tool_name_glob": "edit_file|write_file" },
      "executor": { "type": "bash", "command": "cargo fmt --check" },
      "timeout_ms": 10000,
      "on_error": "warn",
      "description": "check formatting after file edits"
    }
  ],
  "disabled": ["global-hook-id"],
  "disabled_contributions": ["capability_id:hook_id"]
}
```

`hooks` is the upstream `Vec<UserHookSpec>`.

`disabled` is yolop-owned. It removes hook entries from lower-precedence
scopes by explicit `id`. It is intentionally scoped to user-authored global and
workspace hooks, not capability-bundled hooks.

`disabled_contributions` is passed through to upstream `user_hooks` unchanged.
It mutes capability-contributed hook ids such as `capability_id:hook_id`.

Hooks without `id` are legal but append-only: they cannot be overridden or
disabled by another scope. Project and global hooks that need stable merge
behavior should always set `id`.

## Merge

Merge order is global first, workspace second:

1. Load global hooks.
2. Load workspace hooks.
3. Apply each higher-precedence file's `disabled` list to lower-precedence
   hooks.
4. For hooks with the same explicit `id`, the higher-precedence hook replaces
   the lower-precedence hook.
5. Preserve file order within each scope for hooks that remain.
6. Concatenate `disabled_contributions` from both scopes and pass the list to
   the upstream capability.

This matches yolop's existing extension model: workspace scope is most
specific, global scope is user-wide, and absent or broken optional extension
files do not sink startup.

## Management CLI

Hook management is detached host administration under `yolop config hooks`.
The `list`, `get`, `set`, and `remove` commands reuse `HooksStore`, including
its global/workspace merge and atomic write behavior. Hook management is not a
model tool surface. The model can still explain hook configuration, but it does
not receive hook CRUD tools in an assembled session.

## Events

Yolop accepts the six upstream hook event names in config. Firing behavior is
owned by the upstream `everruns-core` runtime; the primary supported Yolop use
case in v1 is `pre_tool_use` / `post_tool_use` policy around tool calls.

| Event | Blocking | Mutation | Purpose |
|-------|----------|----------|---------|
| `session_start` | no | no | Advisory setup/audit at session creation. |
| `user_prompt_submit` | yes | yes | Reject or rewrite the inbound user prompt. |
| `pre_tool_use` | yes | yes | Inspect, block, or rewrite a model-authored tool call. |
| `post_tool_use` | no | yes | Inspect or rewrite a completed tool result. |
| `turn_end` | no | no | Advisory reporting after a turn completes. |
| `session_end` | no | no | Advisory cleanup/audit at session close. |

Built-in capabilities can contribute hooks the same way config does. The
`rex` capability contributes one `session_start` hook that runs
`rex __internal session-start --agent yolop` when a `rex` binary resolves on
`PATH`, and nothing when it does not. The command is best effort (stderr
dropped, failure tolerated), so it never blocks startup. Mute it with
`disabled_contributions` like any other contributed hook.

Tool-event matchers use the upstream restricted matcher shape:

- `tool_name` for exact match.
- `tool_name_glob` for alternation (`a|b`) or trailing-prefix (`mcp_*`).
- `args_jsonpath` for simple dot-path extraction from tool arguments.
- `match_regex` or `deny_regex`, but never both.

## Executor Contract

v1 supports only `executor.type = "bash"`, matching upstream. The runtime
passes the hook payload through environment variables:

- `EVERRUNS_HOOK_PAYLOAD_JSON`
- `EVERRUNS_HOOK_PAYLOAD_PATH`
- `EVERRUNS_HOOK_EVENT`
- `EVERRUNS_HOOK_ID`
- `EVERRUNS_HOOK_SESSION_ID`
- `EVERRUNS_HOOK_TURN_ID` when available
- `EVERRUNS_HOOK_TOOL_NAME` and `EVERRUNS_HOOK_TOOL_CALL_ID` on tool events

The hook prints a JSON decision on stdout:

```json
{ "decision": "allow" }
```

```json
{ "decision": "block", "reason": "dangerous command", "user_message": "Blocked by hook." }
```

```json
{ "decision": "mutate", "patch": { "arguments": { "command": "cargo fmt --check" } } }
```

If stdout is empty, exit code `0` means allow and non-zero means error. The
default timeout is 5 seconds. Valid timeout range is 100 ms through 30 seconds.
Hook output is capped by the upstream executor.

## Trust Model

Hooks are code execution with no model involvement: `session_start` hooks run
at process start in every mode, before any user interaction, through the host
shell with the full environment (provider keys, tokens) and no sandbox. A
workspace hooks file arrives with a clone, so loading it unasked would let a
tracked repository file execute a command. Workspace stdio MCP servers are
refused for the same reason ([MCP](mcp.md)).

Workspace hooks therefore require explicit trust:

- `yolop config hooks trust`, run in the workspace, records the canonical
  workspace path and the SHA-256 of the file bytes in
  `<config_dir>/yolop/hook-trust.json`, beside the global `hooks.json`.
  `--revoke` drops the record.
- Until the stored digest matches the file on disk, the whole workspace file
  is ignored: its hooks for every event, its `disabled` markers and its
  `disabled_contributions`. Any byte change, from a pull or from the agent,
  revokes trust.
- The digest is computed over the same bytes that are parsed, so the file
  cannot change between the check and the load.
- Trust is granted only from the user's own terminal. The session control
  endpoint, reachable from the agent's shell, refuses `trust`, and a workspace
  write made through it leaves the file untrusted. A write from the terminal
  (`config hooks set|remove --scope workspace`) keeps the file trusted only if
  it was trusted, or absent, before, so an edit cannot launder unreviewed
  repository content.
- When a file is skipped the user is told once: a system line in the TUI, a
  stderr line in `--print`, a warning in the log for ACP and serve. The notice
  names the file and the trust command. `config hooks list` reports the skip.
- The record lives outside every workspace: no repository can ship one, and a
  sandboxed session cannot write it.

A CLI command was chosen over a startup prompt: trust must come from outside
the session the hooks would run in, `-p` and ACP runs have no one to ask, and a
prompt shown on every changed file trains users to accept it.

Global hooks are personal, live under the user's config dir, and need no trust
record.

v1 behavior:

- Keep hooks opt-in by file presence; no generated default hooks.
- Show configured hook counts and scopes in `--print` startup diagnostics.
- Run hooks through the upstream `user_hooks` capability instead of the model.
- Never hide hook failures: warn on skipped config and record runtime hook
  errors through existing event/log surfaces.
- Keep environment expansion out of `hooks.json`; users can read env vars from
  their bash script instead of storing secrets in config.

## Implementation Plan

Implemented:

1. `src/config/hooks.rs` parses the two scope files into a yolop-owned
   envelope, validates each hook through `UserHookSpec::validate`, and produces
   an effective upstream `UserHooksConfig`.
2. `src/runtime/mod.rs` registers `everruns_core::capabilities::UserHooksCapability`
   and enables it with `AgentCapabilityConfig::with_config("user_hooks", ...)`
   only when hooks are configured.
3. `StartupInfo` includes effective hook counts; `--print` shows loaded hooks.
4. `yolop config hooks` manages hooks through the startup `HooksStore`; hook
   management is not exposed as model tools.
5. Tests cover merge behavior, hook CLI management, and a scripted llmsim
   `pre_tool_use` hook that blocks a matching `bash` tool call.
6. `src/config/hook_trust.rs` stores workspace trust records; `HooksStore`
   applies them. Tests cover an untrusted workspace hook not running (tool-use
   and `session_start`), a trusted one running, an edit revoking trust, global
   hooks being unaffected, and the session endpoint refusing `trust`.

Follow-up:

- Add more embedded hook recipes once the minimal tool
  surface has settled.
- Run `session_start` hooks under the configured sandbox with a scrubbed
  environment; trusted hooks still get the full host environment today.

## Non-goals

- No new hook events beyond upstream's six-event contract.
- No per-hook approval prompt. Trusting a workspace file approves all of it;
  writing the global file is consent.
- No secret storage in hook config.
- No TOML format until there is a concrete reason to diverge from upstream's
  JSON schema.
- No separate yolop-only hook executor.

## Management surface

Hook management is explicit CLI configuration, not a model tool surface. Use `yolop config hooks list`, `get`, `set`, `remove`, and `trust`. Runtime hook execution remains model-independent.

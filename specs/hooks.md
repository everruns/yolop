# Hooks Specification

Status: v1 implemented for scoped config loading, `hooks` self-configuration,
and upstream `user_hooks` registration.

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

1. **Global** — `<config_dir>/yolop/hooks.json`.
   Personal automation shared across every workspace.
2. **Workspace** — `<workspace>/.agents/hooks.json`.
   Project-owned automation that can be reviewed and committed with the repo.

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

## Natural-Language Setup

Hook setup is part of yolop self-configuration. A user should be able to say:

> yolop setup a hook to prevent calls to git

and have yolop create a real hook config entry rather than merely remembering a
preference. The natural-language layer belongs in `YourCapability`, because
that capability is the place users address yolop itself.

The reliable design is **prompt/skill for interpretation, tools for writes**:

- The `your` prompt teaches the model that requests like "configure yolop",
  "set up your hooks", or "prevent yourself from calling X" are
  self-configuration requests.
- The embedded `yolop-hooks` skill holds examples that map common intents to
  hook specs, so the prompt stays short and examples are loaded on demand.
- Structured `hooks` capability tools perform the actual mutation. They read,
  validate, merge, and atomically write `hooks.json`, then return the effective
  hook id and scope. The model should not hand-edit hook JSON through generic
  file tools for global self-configuration.

Initial tool surface:

- `list_hooks` — show effective hooks, their scope, event, matcher, and
  source file.
- `upsert_hook` — create or replace one hook by `id`.
- `remove_hook` — remove or disable one hook by `id`.
- `validate_hook` — validate a candidate spec without writing it.

`upsert_hook` accepts an explicit `scope`:

- `global` writes `<config_dir>/yolop/hooks.json`.
- `workspace` writes `<workspace>/.agents/hooks.json`.

If the user says "yolop", "your", or otherwise frames the request as a
personal preference, default to `global`. If the user says "this repo",
"workspace", or "project", use `workspace`. If the requested hook would block a
broad class of normal coding actions and the scope is ambiguous, ask once for
scope instead of guessing.

Example generated hook for "prevent calls to git":

```json
{
  "id": "block-git",
  "event": "pre_tool_use",
  "matcher": {
    "tool_name": "bash",
    "args_jsonpath": "$.command",
    "match_regex": "(^|[;&|()[:space:]])git([[:space:]]|$)"
  },
  "executor": {
    "type": "bash",
    "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"git command blocked by hook\",\"user_message\":\"Blocked by your yolop hook: git commands are disabled.\"}'"
  },
  "timeout_ms": 1000,
  "on_error": "block",
  "description": "Block bash commands that invoke git"
}
```

The model may offer to refine the matcher for narrower cases, such as allowing
read-only `git status` while blocking mutating commands.

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

Hooks are code execution. Workspace hooks are therefore a project trust signal,
similar to a committed build script or MCP stdio server list. Global hooks are
personal and live under the user's config dir.

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

1. `src/hooks_config.rs` parses the two scope files into a yolop-owned
   envelope, validates each hook through `UserHookSpec::validate`, and produces
   an effective upstream `UserHooksConfig`.
2. `src/runtime.rs` registers `everruns_core::capabilities::UserHooksCapability`
   and enables it with `AgentCapabilityConfig::with_config("user_hooks", ...)`
   only when hooks are configured.
3. `StartupInfo` includes effective hook counts; `--print` shows loaded hooks.
4. `HooksCapability` includes the hook self-configuration tools described in
   [Natural-Language Setup](#natural-language-setup).
5. `skills/yolop-hooks/SKILL.md` ships the self-configuration recipes used for
   natural-language hook setup.
6. Tests cover merge behavior, `hooks` tools, and a scripted llmsim
   `pre_tool_use` hook that blocks a matching `bash` tool call.

Follow-up:

- Add more embedded hook recipes once the minimal tool
  surface has settled.

## Non-goals

- No new hook events beyond upstream's six-event contract.
- No per-hook approval prompt. The act of writing the config is consent.
- No secret storage in hook config.
- No TOML format until there is a concrete reason to diverge from upstream's
  JSON schema.
- No separate yolop-only hook executor.

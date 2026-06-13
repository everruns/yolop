---
title: Hooks
description: Configure Yolop to block, mutate, or audit actions with workspace and global hook files.
---

Hooks let you attach deterministic automation to Yolop's agent loop. They are
useful when an instruction is too important to leave as a preference: block a
class of shell commands, rewrite unsafe tool arguments, or record an audit event
outside the model.

Yolop uses the upstream `everruns-core` `user_hooks` capability. The local
Yolop layer only discovers hook files, merges workspace and global scopes, and
exposes a `hooks` capability to configure those files from natural language.

## How it works

```mermaid
flowchart LR
  User["User: yolop setup a hook"] --> Your["your personalization"]
  Your --> Hooks["hooks capability"]
  Hooks --> Tools["validate_hook / upsert_hook"]
  Tools --> Files["hooks.json"]
  Files --> Runtime["user_hooks capability"]
  Runtime --> Decision{"Hook decision"}
  Decision -->|allow| Continue["run action"]
  Decision -->|block| Stop["stop action"]
  Decision -->|mutate| Rewrite["rewrite action/result"]
```

The model interprets the request, but it does not hand-edit hook JSON. The
embedded `yolop-hooks` skill maps common requests to hook specs, and the
`hooks` tools validate and write the config atomically.

## Scopes

| Scope | Path | Use for |
|---|---|---|
| Global | `<config_dir>/yolop/hooks.json` | Personal Yolop behavior across all workspaces |
| Workspace | `<workspace>/.agents/hooks.json` | Project-owned policy that can be reviewed with the repo |

Workspace hooks override global hooks with the same `id`. A workspace file can
also disable a lower-precedence global hook by listing the id in `disabled`.

## Configure from chat

Ask Yolop directly:

```text
yolop setup a hook to prevent calls to git
```

Yolop should translate that into a `pre_tool_use` hook and save it through
`upsert_hook`. If the scope is ambiguous and the hook blocks a broad class
of normal coding actions, Yolop should ask whether the hook is global or just
for the current workspace.

## Configure by file

```json
{
  "hooks": [
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
        "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"git command blocked by hook\",\"user_message\":\"Blocked by your Yolop hook: git commands are disabled.\"}'"
      },
      "timeout_ms": 1000,
      "on_error": "block",
      "description": "Block bash commands that invoke git"
    }
  ],
  "disabled": [],
  "disabled_contributions": []
}
```

## Events

Yolop accepts the upstream `user_hooks` event names. Tool-call hooks are the
primary v1 surface.

| Event | Can block? | Can mutate? | Typical use |
|---|---|---|---|
| `pre_tool_use` | yes | yes | Block or rewrite tool calls |
| `post_tool_use` | no | yes | Rewrite tool results |
| `user_prompt_submit` | yes | yes | Reject or rewrite inbound prompts |
| `turn_end` | no | no | Advisory reporting after a turn |
| `session_start` | no | no | Advisory setup or audit |
| `session_end` | no | no | Advisory cleanup |

Tool events can match on exact tool name, restricted tool glob, and simple
argument JSON paths. Regexes use Rust's `regex` crate.

## Hook decisions

A bash hook prints one JSON decision to stdout:

```json
{ "decision": "allow" }
```

```json
{
  "decision": "block",
  "reason": "git command blocked by hook",
  "user_message": "Blocked by your Yolop hook: git commands are disabled."
}
```

```json
{
  "decision": "mutate",
  "patch": { "arguments": { "command": "cargo fmt --check" } }
}
```

If stdout is empty, exit code `0` means allow and non-zero means block with
stderr as the reason. Non-JSON stdout is treated as a hook error.

## Trust model

Hooks are code execution. Global hooks are personal config. Workspace hooks are
project policy, similar to checked-in build scripts or `.mcp.json` stdio
servers. Review them before using an unfamiliar repository.

The hook executor is bounded by the upstream `user_hooks` implementation:
validated specs, timeout limits, output caps, and structured decisions. Yolop
does not add a second hook engine.

## Related

- [`specs/hooks.md`](../../specs/hooks.md)
- [`specs/your.md`](../../specs/your.md)

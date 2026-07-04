---
name: yolop-hooks
description: Configure Yolop hooks from natural-language self-configuration requests, especially block, allow, mutate, or audit rules around tool calls.
user-invocable: false
---

# Yolop Hooks

Use this skill when the user asks Yolop to configure its own behavior with
hooks, for example:

- "yolop setup a hook to prevent calls to git"
- "block yourself from running terraform apply in this repo"
- "add a workspace hook that audits shell commands"
- "remove the hook that blocks cargo publish"

Hooks are real configuration. Do not treat these requests as memory or as a
soft preference. Use the `hooks` capability tools:

1. Build a candidate hook spec.
2. Call `validate_hook`.
3. Call `upsert_hook` after validation succeeds.
4. Use `list_hooks` to inspect current state or confirm effective config.
5. Use `remove_hook` when the user asks to remove or disable a hook.

## Scope

Choose the narrowest scope that matches the request.

- Use `workspace` when the user says "this repo", "this project",
  "workspace", or references project policy.
- Use `global` when the user says "yolop", "your", "always", "everywhere",
  or frames the rule as a personal preference.
- Ask once if the rule would block broad normal coding behavior and the scope
  is ambiguous.

## Common Recipes

Block any Bash command that invokes `git`:

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
    "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"git command blocked by hook\",\"user_message\":\"Blocked by your Yolop hook: git commands are disabled.\"}'"
  },
  "timeout_ms": 1000,
  "on_error": "block",
  "description": "Block bash commands that invoke git"
}
```

Block mutating `git` commands but allow read-only status/log/diff:

```json
{
  "id": "block-mutating-git",
  "event": "pre_tool_use",
  "matcher": {
    "tool_name": "bash",
    "args_jsonpath": "$.command",
    "match_regex": "(^|[;&|()[:space:]])git[[:space:]]+(add|am|apply|bisect|branch|checkout|cherry-pick|clean|commit|fetch|merge|mv|pull|push|rebase|reset|restore|revert|rm|switch|tag)([[:space:]]|$)"
  },
  "executor": {
    "type": "bash",
    "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"mutating git command blocked by hook\",\"user_message\":\"Blocked by your Yolop hook: mutating git commands are disabled.\"}'"
  },
  "timeout_ms": 1000,
  "on_error": "block",
  "description": "Block mutating git commands"
}
```

Block publishing commands:

```json
{
  "id": "block-publish",
  "event": "pre_tool_use",
  "matcher": {
    "tool_name": "bash",
    "args_jsonpath": "$.command",
    "match_regex": "(^|[;&|()[:space:]])(cargo publish|npm publish|pnpm publish)([[:space:]]|$)"
  },
  "executor": {
    "type": "bash",
    "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"publish command blocked by hook\",\"user_message\":\"Blocked by your Yolop hook: publish commands are disabled.\"}'"
  },
  "timeout_ms": 1000,
  "on_error": "block",
  "description": "Block package publishing commands"
}
```

Audit Bash commands without blocking:

```json
{
  "id": "audit-bash",
  "event": "pre_tool_use",
  "matcher": {
    "tool_name": "bash"
  },
  "executor": {
    "type": "bash",
    "command": "printf '%s\\n' \"$EVERRUNS_HOOK_PAYLOAD_JSON\" >> .agents/hook-audit.jsonl && printf '%s\\n' '{\"decision\":\"allow\"}'"
  },
  "timeout_ms": 1000,
  "on_error": "warn",
  "description": "Append Bash tool calls to .agents/hook-audit.jsonl"
}
```

## Rules

- Prefer stable ids such as `block-git`, `block-mutating-git`, or
  `audit-bash`; stable ids make overrides and removal possible.
- Prefer `pre_tool_use` for blocking or mutating model-authored tool calls.
- Use `on_error: "block"` for safety-critical hooks and `on_error: "warn"`
  for audit hooks.
- Keep hook commands short. For complex logic, point the executor at a checked
  in script instead of embedding a large shell program.
- Do not put secrets in `hooks.json`; hook scripts can read environment
  variables at runtime.

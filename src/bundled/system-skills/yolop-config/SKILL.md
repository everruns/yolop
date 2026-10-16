---
name: yolop-config
description: Manage Yolop settings, continuation budgets, persistent default model, and model catalog through the config command.
user-invocable: true
---

# Yolop configuration

Use Bash to run `yolop config`. The command works both detached and through the
current Yolop session. Attached calls remain attached inside scripts, pipelines,
redirection, substitutions, and background execution.

## Persistent settings

- `yolop config get [key]` shows all settings or one schema key.
- `yolop config set <key> <value>` sets scalar and provider-scoped fields.
- `yolop config set capabilities --json '<object>'` appends one capability override; `--json` preserves the former structured configuration semantics.
- `yolop config clear <key>` clears a field, or all capability overrides when the key is `capabilities`.

## Automatic continuation budget

When the user asks to extend the budget, use the matching setting:

- `yolop config set continuation_max_turns 10`, default 5 automatic turns.
- `yolop config set continuation_max_tokens 512000`, default 256000 tokens.
- `yolop config set continuation_max_seconds 3600`, default 600 seconds.

Values must be positive integers. The first reached limit pauses continuation.
The original user turn is excluded. Attached changes apply at the next
continuation check without resetting usage; change the relevant limits when the
user asks for longer work. These are persistent settings, with named profile
overrides supported by `yolop config --profile <name> set|clear`.

At exhaustion Yolop asks the user to approve resuming with a fresh budget.
Approval is conversational and requires no specific words; approval alone does
not increase the persistent limits. Do not send approval on the user's behalf.

## Persistent default model

- `yolop config model show|set <model>|clear` inspects, sets, or clears the
  persistent default provider and model used by future sessions.
- A model may be `provider/model`, `provider:model`, or a bare id that is unique
  in the configured list.
- These commands do not switch the live session. Use `/model`, `/setup`, or the
  runtime model controls for live switching.

## Model list

- `yolop config models` (or `models list`) shows the ordered list.
- `yolop config models add <provider> <model> [--effort E] [--label L] [--position N]` adds an entry.
- `yolop config models rm <model>` removes an entry.
- `yolop config models move <model> <position>` reorders an entry.
- `yolop config models edit <model> [options]` edits an entry.
- `yolop config models reset` restores defaults.

Model-list edits persist to `settings.toml`. When run attached, they also refresh
the current session's menu. The config capability intentionally exposes no agent tools. Use these top-level
commands instead of `get_config` or `set_config`.

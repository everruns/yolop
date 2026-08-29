---
name: yolop-config
description: Manage yolop's persistent default model and ordered model catalog through the config command.
user-invocable: true
---

# Yolop configuration

Use the foreground Bash tool to run `yolop config` directly. The command works
both detached and through the current Yolop session. Do not use a pipeline,
redirection, substitution, or background execution when attached.

## Persistent settings

- `yolop config get [key]` shows all settings or one schema key.
- `yolop config set <key> <value>` sets scalar and provider-scoped fields.
- `yolop config set capabilities --json '<object>'` appends one capability override; `--json` preserves the former structured configuration semantics.
- `yolop config clear <key>` clears a field, or all capability overrides when the key is `capabilities`.

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

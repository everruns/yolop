---
name: yolop-config
description: View and change yolop's own configuration — default provider and model, per-provider API tokens and models, endpoint base URLs, and attribution. Use when the user asks to configure yolop, set a default provider/model, store an API key, point at a custom endpoint, or asks "what is your config / what can you configure".
user-invocable: true
---

# Yolop configuration

yolop stores its settings in a single TOML file (`settings.toml` in the yolop
config dir). The file is loaded tolerantly — unknown keys are ignored, never
fatal — and every known key carries semantics (title, description, type,
default, examples) that you can read at runtime. This skill is the entry point
for inspecting and editing that configuration the way a user describes it.

Do not hand-edit the TOML with the file tools. Use the schema-aware tools so
values are validated and persisted atomically.

## Inspect

1. Call `get_config` with no arguments to list **every** configuration key with
   its meaning, type, default, examples, and current value. Secrets (API
   tokens) are shown only as `stored` / `unset`, never echoed.
2. Call `get_config` with a single `key` (e.g. `default_provider`,
   `models.anthropic`, `tokens.openai`) to focus on one entry.

Lead with `get_config` whenever you are unsure of the exact key name or the
accepted values — the returned schema is the source of truth, so you never have
to guess.

## Change

Call `set_config` with a `key` and a `value`:

- `set_config key=default_provider value=anthropic` — the default provider when
  neither `--provider` nor an env credential forces a choice.
- `set_config key=default_model value="claude-sonnet-4-5"` — global fallback
  model for the active provider. A per-provider pick wins over it.
- `set_config key=models.openai value="gpt-5.5 high"` — remember a model for one
  provider (survives provider switches). The spec is `model [reasoning-effort]`.
- `set_config key=tokens.anthropic value=…` — store an API token (owner-only on
  disk). Environment variables still override stored tokens.
- `set_config key=base_urls.custom value=http://localhost:8000/v1` — endpoint
  for the OpenAI-compatible `custom` provider.
- `set_config key=attribution value=off` — turn commit/PR attribution on/off.

Pass `value=clear` to unset an optional or secret key
(e.g. `set_config key=tokens.openai value=clear`).

Provider and model edits are persisted and take effect on the **next run**. To
switch the *live* model in the current session, use the interactive `/setup`
command instead.

## Related surfaces

- **Durable preferences / memory** ("remember that I prefer terse answers"):
  these are not config keys. Use the `remember` / `recall` / `forget` tools
  (the global `memory` capability), not `set_config`. Memory tuning
  (`disclosed_titles`, `recall_limit`, `soft_cap`) is per-capability config
  exposed via the capability's `config_schema`, not a `settings.toml` key.
- **Behavioral hooks** (block/allow/audit tool calls): use the `yolop-hooks`
  skill and the `*_your_hook` tools.
- **Interactive provider/model setup**: the `/setup` command runs a guided
  wizard and switches the live model immediately.

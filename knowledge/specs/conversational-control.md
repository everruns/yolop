---
type: Product Specification
title: Conversational Control Specification
description: Defines the conversational control specification contract for Yolop.
---

# Conversational Control Specification

## Abstract

Every user-facing control over a yolop session must be reachable **conversationally**:
the agent can perform it by calling a tool in response to ordinary prose, or on its
own initiative when the task calls for it, without the user typing a slash command
and without an interactive overlay the user must confirm. Slash commands and TUI
overlays remain as convenient front-ends, but they are never the *only* path.

This spec is the durable contract behind that promise. It says **what** must be
conversational and **how new control surfaces inherit the same treatment**; it does
not duplicate each tool's schema (those live in source).

## Motivation

yolop is an agent first. If the agent decides it needs more reasoning budget for a
hard step, a different model for a task, or wants to uninstall a stale skill, it
should just do it, the same way a human collaborator would, rather than printing
"please type `/effort high`". Controls that are reachable only through a slash
command, an overlay confirmation, or a next-run-only settings write fail this bar.

## Required behavior

1. **Live, agent-invocable, no confirmation.** Each control surface listed below
   exposes a model-facing tool that (a) the agent can call from a natural-language
   request or autonomously, (b) takes effect on the **live session** (at the latest,
   the next turn, never "next process run only"), and (c) does not require the user
   to confirm an interactive overlay.

2. **One implementation, many front-ends.** A control's mutation logic lives in one
   place; the slash command, any overlay, and the agent tool all route through it.
   Adding a tool must not fork the logic, it shares the command's code path so
   validation, persistence, and live application are identical. (`SetupController` is
   the reference: `/setup`, the model picker overlay, and the `set_*` tools all call
   its `change_*` methods.) Model selection applies immediately but is persisted only
   after that model completes a successful turn, so an inaccessible or unsupported
   model does not become the next session's default.

3. **Errors are recoverable.** A bad argument (unknown effort, unknown provider,
   missing skill) returns a tool error whose message names the valid options or the
   reason, so the agent can correct itself without user help.

4. **Discoverability.** The agent is told these tools exist and when to use them via
   a capability system-prompt contribution, so it reaches for them instead of
   instructing the user. Keep that guidance short and conservative ("prefer the
   smallest change; do not thrash").

5. **New control surfaces inherit this contract.** Any future setting, mode, or
   resource a user can change in a session ships with its conversational tool in the
   same change, not as a follow-up. A control that is only a slash command, only an
   overlay, or only a next-run settings key is incomplete and should be treated as a
   bug against this spec.

## Current control surfaces

| Surface | Conversational tool | Front-ends sharing the logic |
|---|---|---|
| Reasoning effort | `set_reasoning_effort` | `/effort` overlay, `/setup effort` |
| Model | `search_models` / `set_model` | `/model` overlay, `/setup model` |
| Provider | `set_provider` | `/setup provider` |
| Skills, list | `list_skills` (upstream) | system-prompt listing |
| Skills, search (skills.sh) | `search_skills` |, |
| Skills, install from registry | `install_skill` |, |
| Skills, install/update by content | `write_skill` (upstream) |, |
| Skills, uninstall | `delete_skill` |, |
| Settings (provider/model/tokens/urls/capabilities, next-run) | `get_config` / `set_config` | `/setup`, `yolop-config` skill |
| Any slash command the host's registry holds (`/setup`, `/background`, `/undo`, `/redo`, `/rewind`, `/goal`, plus the terminal ones in the TUI) | `run_command` (every host) | the slash commands themselves |

Notes:

- `set_model` / `set_provider` / `set_reasoning_effort` apply to the live session; the
  `/model` and `/effort` overlays still exist for humans, but the agent no longer
  needs them (it does not pre-seed an overlay the user must confirm).
- `search_models` queries usable providers through the runtime driver registry and
  returns provider-qualified matches. `set_model` rejects a partial name when it
  matches discovered models but is not an exact ID for the current provider.
- `set_config` is intentionally next-run for provider/model edits, it edits the
  settings file. The *live* equivalents are the `set_*` tools above.
- **Attached administration is the deliberate exception to "a model-facing tool".**
  Extension and coordination administration is reachable conversationally by
  running `yolop <subcommand> ...` in the foreground Bash tool, which the host
  attaches to the live session, rather than by tool schemas that would cost
  context every turn. It still meets the rest of this contract: live effect,
  no confirmation overlay, one shared implementation behind the CLI, `/command`,
  and control plane. Discoverability comes from the single control-plane prompt
  block described in [`extensions.md`](./extensions.md), not from per-capability
  prompt text.
- `run_command` runs on every host and dispatches the whole registry, not a
  curated subset, through `runtime.execute_command`. Hosts differ only in what
  their registry holds: terminal commands exist in the TUI, where the host port
  also returns `/mcp` and `/tools` transcript output. `Skill` commands stay
  prompt-activated and `/shell` stays typed-only (the agent has `bash`). See
  [`commands.md`](./commands.md).

## Known gap

- **Mid-turn reasoning-effort change** (within a single `run_turn`, not just at the
  next turn boundary) requires upstream `everruns-runtime` support and is tracked in
  **EVE-595**. `set_reasoning_effort` delivers turn-boundary escalation today.

## Ownership boundary

- This spec owns the conversational-control contract and the inventory above.
- `crate::capabilities::host` owns `SetupController` and the `set_*` tools.
- `crate::capabilities::skills` owns `delete_skill`; `crate::capabilities::skill_registry`
  owns `search_skills` / `install_skill`; the upstream `ScopedSkillsCapability`
  owns `list_skills` / `write_skill` (see [`skills.md`](./skills.md)).
- `crate::capabilities::config` owns `get_config` / `set_config` (see
  [`configuration.md`](./configuration.md)).
- The command registry and `run_command` are owned by [`commands.md`](./commands.md).

## Related

- [`commands.md`](./commands.md), slash-command surface and natural-language dispatch.
- [`skills.md`](./skills.md), skill scopes and management tools.
- [`configuration.md`](./configuration.md), the settings file and its schema.

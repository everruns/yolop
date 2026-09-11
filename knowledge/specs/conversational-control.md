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
   is reachable conversationally, either through a model-facing tool or through the
   attached CLI (`yolop <subcommand> ...` in the foreground Bash tool), that
   (a) the agent can call from a natural-language request or autonomously,
   (b) takes effect on the **live session** (at the latest, the next turn, never
   "next process run only"), and (c) does not require the user to confirm an
   interactive overlay.

2. **One implementation, many front-ends.** A control's mutation logic lives in one
   place; the slash command, any overlay, and the attached CLI all route through it.
   Adding a front-end must not fork the logic, it shares the command's code path so
   validation, persistence, and live application are identical. (`SetupController` is
   the reference: `/setup`, the model picker overlay, and `yolop setup ...` all call
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
   resource a user can change in a session ships with its attached CLI
   (`yolop <subcommand> ...` plus a control route for prompt discovery) in the
   same change, not as a follow-up. Configuration capabilities do not get
   model-invoked mutation tools: the CLI keeps secrets and credentials on the
   human-driven path and out of tool schemas. A control that is only a slash
   command, only an overlay, or only a next-run settings key is incomplete and
   should be treated as a bug against this spec.

## Current control surfaces

| Surface | Conversational tool | Front-ends sharing the logic |
|---|---|---|
| Reasoning effort | `yolop model use <id>:<effort>` (attached CLI) | `/effort` overlay, `/setup effort` |
| Model | `yolop model use <target>` (attached CLI) | `/model` overlay, `/setup model` |
| Model list (the menu `/model` and ACP offer) | `yolop config models …` (attached CLI) | `/model` overlay, `[[models]]` in settings |
| Provider | `yolop setup login <provider>` (attached CLI) | `/setup provider` |
| Skills, list | `list_skills` (upstream) | system-prompt listing |
| Skills, package management | `yolop skills` (attached CLI) | skills control route |
| Hooks, configuration | `yolop config hooks` (attached CLI) | hooks control route |
| Model selection and model-list edits | `yolop config model` / `yolop config models` | `/setup`, `yolop-config` skill |
| Any slash command the host's registry holds (`/setup`, `/background`, `/undo`, `/redo`, `/rewind`, `/goal`, plus the terminal ones in the TUI) | `run_command` (every host) | the slash commands themselves |

Notes:

- `yolop model use` / `yolop setup login` apply to the live session; the
  `/model` and `/effort` overlays still exist for humans, but the agent no longer
  needs them (it does not pre-seed an overlay the user must confirm).
- `yolop model` (show) lists usable models through the runtime driver registry.
  `yolop model use` rejects a partial name when it matches discovered models but
  is not an exact ID for the current provider; append `:effort` to set effort
  (`yolop model use openai/gpt-5.4:high`). Never guess an ID: list first.
- `yolop config model show|set|clear` owns persistent model selection, while
  `yolop config models` edits the persisted ordered list. Attached list edits
  also refresh the current session menu; neither command switches the live model.
- **Attached administration is the rule for configuration, not the exception.**
  Extension, coordination, model-list, MCP, connector, model, and setup
  administration is reachable conversationally by
  running `yolop <subcommand> ...` in the foreground Bash tool, which the host
  attaches to the live session, rather than by tool schemas that would cost
  context every turn. It still meets the rest of this contract: live effect,
  no confirmation overlay, one shared implementation behind the CLI, `/command`,
  and control plane. Discoverability comes from the single `yolop` prompt
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
  next turn boundary) requires upstream `everruns-host` support and is tracked in
  **EVE-595**. `yolop model use <id>:<effort>` delivers turn-boundary escalation today.

## Ownership boundary

- This spec owns the conversational-control contract and the inventory above.
- `crate::capabilities::host` owns `SetupController`; model and session
  configuration reach it through `yolop model ...` and `yolop setup ...`, not
  through model-invoked tools.
- `crate::capabilities::skills` owns attached skill package management;
  `crate::capabilities::skill_registry` owns its skills.sh client. The upstream
  `ScopedSkillsCapability` owns model-visible `list_skills` and `activate_skill`
  (see [`skills.md`](./skills.md)).
- `crate::capabilities::hooks` owns the hook control route; `config` nests its
  CLI at `yolop config hooks`.
- `crate::capabilities::config` owns the attached `config` command and delegates
  model-list actions to `ModelListCapability` (see
  [`configuration.md`](./configuration.md)).
- The command registry and `run_command` are owned by [`commands.md`](./commands.md).

## Related

- [`commands.md`](./commands.md), slash-command surface and natural-language dispatch.
- [`skills.md`](./skills.md), skill scopes and management tools.
- [`configuration.md`](./configuration.md), the settings file and its schema.

Hook and skill mutation is intentionally outside conversational control. The model may list and activate installed skills, but operators manage hooks with `yolop config hooks ...` and skill storage with `yolop skills ...`.

## Detached administration

Hook CRUD and skill installation, editing, deletion, and registry operations are host administration. They run under `yolop config hooks` and `yolop skills`, not as model tools. The model skill surface remains limited to discovery and activation.

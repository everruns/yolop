---
type: Product Specification
title: Herdr integration
description: Defines the herdr integration contract for Yolop.
---

# Herdr integration

## Purpose

Yolop should behave as a first-class process when launched inside
[Herdr](https://herdr.dev/) without requiring users to install hooks or copy a
skill into every project.

## Environment contract

The integration activates only when all of these inherited variables are
present and valid:

- `HERDR_ENV=1`
- `HERDR_PANE_ID`
- `HERDR_SOCKET_PATH`

`HERDR_BIN_PATH` selects the CLI binary when present; otherwise Yolop invokes
`herdr` from `PATH`. Commands are executed directly, never through a shell.
Missing or incompatible Herdr installations are non-fatal. Yolop also reports
pane metadata so Herdr can follow the session title and distinguish concurrent
sessions in its agent list.

## Lifecycle reporting

Yolop reports to the current pane with source `yolop:lifecycle` and agent label
`yolop`:

- initial state and completed, failed, or cancelled turns: `idle`
- a started turn: `working`
- an explicit user-ask evaluation that needs user input: `blocked`

Reports include Yolop's session id and a process-independent monotonic sequence.
The machine agent remains `yolop` for grouping and ownership, while its display
name is `Yolop · <session title>`. Before a title exists, Yolop uses a stable
short session-id suffix. Yolop also forwards `session.title.updated` as the pane
title and maps lifecycle states to readable labels. Reports are best-effort,
bounded by a short timeout, and never delay or fail a model turn. Herdr's
attention layer may display a completed background `idle` transition as `done`.

When the last reporter owner is dropped, Yolop directly spawns Herdr's
`release-agent` command for its own source and label. This exit-safe cleanup
uses the next sequence value, and does not remove or alter reports owned by
other integrations.

Yolop does not infer `blocked` from prose. Soft-approval questions therefore
still need Herdr screen detection until Yolop gains a structured question
lifecycle.

## Conditional skill

Inside Herdr, the capability mounts a read-only `herdr` skill under a
session-local environment-skill VFS. It is visible through the normal skills
capability and follows normal precedence, so a workspace or global `herdr`
skill can override it. The mount is
in memory for the session: Yolop does not fetch instructions at startup or
write `.agents/skills` or a global skill directory.

Outside Herdr, the capability contributes no mount and performs no reporting.

## Ownership

- `capabilities::herdr` owns environment validation, lifecycle and metadata
  reports, and the skill mount.
- The runtime event bus drives cross-host lifecycle transitions for TUI,
  `--print`, and ACP turns.
- `ScopedSkillsCapability` owns discovery and activation of the mounted
  environment scope.

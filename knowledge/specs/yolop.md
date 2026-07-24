---
type: Product Specification
title: `yolop` — self-address framing
description: Defines the `yolop` — self-address framing contract for Yolop.
---

# `yolop` — self-address framing

Status: implemented (framing only).

## Why

Users sometimes address yolop itself — *"what can **you** do?"*, *"what is
**your** config?"*, *"set yolop blue"* — rather than asking for a change to the
current repository. Those are **global** requests about the tool and must be
distinguished from project work (which belongs in the repo's `AGENTS.md`,
source, and tests).

Each yolop-owned capability already contributes its own system-prompt block and
tools. The `yolop` capability adds only the framing layer: teach the model when
a request is about yolop itself.

## What

The capability contributes a standard `<capability id="yolop">` block through
`system_prompt_contribution`. It exposes no tools and no slash commands.

Concrete self-configuration — settings, memory, hooks, approval, skills — lives
in the capabilities that own those surfaces (`yolop_config`, `memory`, `hooks`,
and so on). This capability does not route to them; their own prompts and skills
carry that guidance.

## Non-goals

- Not a router — do not duplicate or override other capabilities' instructions.
- Not a secret store — tokens stay in `settings.toml`.
- Not project memory — repo-scoped guidance stays in `AGENTS.md`.

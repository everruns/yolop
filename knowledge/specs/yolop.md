---
type: Product Specification
title: `yolop`, self-address framing plus attached administration
description: Defines the `yolop`, self-address framing and session administration contract for Yolop.
---

# `yolop`, self-address framing plus attached administration

Status: implemented (framing plus administration).

## Why

Users sometimes address yolop itself, *"what can **you** do?"*, *"what is
**your** config?"*, *"set yolop blue"*, rather than asking for a change to the
current repository. Those are **global** requests about the tool and must be
distinguished from project work (which belongs in the repo's `AGENTS.md`,
source, and tests).

The same block teaches how to act on the live session: run
`yolop <subcommand> ...` in the bash tool, with the route list
derived from the routes actually registered. Administration is deliberately not
a set of model tools (their schemas would cost context every turn).

Each yolop-owned capability already contributes its own system-prompt block and
tools. The `yolop` capability adds the framing layer plus the attached
administration: teach the model when a request is about yolop itself, and how
to administer the session it is attached to.

The CLI remains ordinary shell software. Session attachment survives scripts,
pipelines, redirection, command substitution, and background expressions. A
child sends raw argv over an authenticated local endpoint before parsing them;
the host's exact executable owns the canonical grammar and typed request.

## What

The capability contributes a single standard `<capability id="yolop">` block
through `system_prompt_contribution`: framing always, plus the administration
section listing only the routes actually registered. It exposes no tools and no
slash commands.

Concrete self-configuration, settings, memory, hooks, approval, skills, lives
in the capabilities that own those surfaces (`config`, `memory`, `hooks`,
and so on). This capability does not route to them; their own prompts and skills
carry that guidance.

### Help is the contract agents read

The administration block names the routes; `--help` is where a model that has
never seen yolop learns their spelling. Every visible `yolop` subcommand, built
in or contributed by a capability, therefore has a one-line description, and
every leaf command has an `Examples:` block in its `after_help` that pairs an
intent with a runnable command naming that leaf. One good example saves the
exploratory calls a model would otherwise spend guessing flags.
`every_cli_leaf_has_about_and_examples` in `src/main.rs` walks the assembled
clap tree and fails when a command lacks either, and parses every example line
against the real grammar. A wrong example is worse than none because a model
copies it verbatim; the check was added after two published examples named
flags that did not exist (`config hooks set --matcher`, `config models add
--after`). Lines with a `<placeholder>` are templates and are not parsed. Help
stays short and written for people too: one or two examples, not a manual.

## Non-goals

- Not a router, do not duplicate or override other capabilities' instructions.
- Not a secret store, tokens stay in `settings.toml`.
- Not project memory, repo-scoped guidance stays in `AGENTS.md`.

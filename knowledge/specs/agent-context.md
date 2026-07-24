---
type: Policy
title: Agent Context Specification
description: Defines how this repository organizes the context coding agents read — AGENTS.md, knowledge, and skills.
---

# Agent Context Specification

## Purpose

Yolop's repository is read by coding agents on every turn. This specification
defines how that context is organized so the agent gets high-signal material
without paying for the whole repository up front.

The guidance targets current-generation models, which need less constraint and
more judgment space than earlier ones. Instructions that spell out steps a
capable model would choose anyway do not improve behavior; they crowd out the
project-specific facts that do.

## Principles

1. **One owner per instruction.** Any rule lives in exactly one place. A spec
   states the bar; the skill states the workflow; `AGENTS.md` states the
   repository facts. Restating a rule in a second file makes the two drift and
   forces the agent to reconcile them.
2. **Progressive disclosure.** Load context when it is needed, not before.
   `AGENTS.md` is read every turn, so it carries only what is needed on every
   turn; depth lives behind a link the agent follows when the task calls for it.
3. **Gotchas, not the obvious.** Document what an agent cannot infer from the
   file system, `Cargo.toml`, or the code: non-obvious ordering, environment
   requirements, invariants a test does not express. Do not restate what
   reading the repository already shows.
4. **Judgment over ritual.** State the required outcome and the constraint that
   makes it non-negotiable, then let the agent choose the path. Prescribe exact
   steps only where the cost of a wrong choice is high and irreversible —
   publishing, security review, credentials, git history.
5. **No conflicting constraints.** A hard requirement and an invitation to use
   judgment must not cover the same ground. Where both appear, the agent
   deliberates instead of working. Pick one and mean it.
6. **Prefer references in code.** A test, a script, a manifest, or a runnable
   command is higher-fidelity than prose describing it. Point at the artifact
   rather than paraphrasing it; paraphrase drifts, the artifact does not.

## Layers

| Layer | Read when | Carries |
| --- | --- | --- |
| `AGENTS.md` | Every turn | Repository purpose, gotchas, the commands that gate a change, pointers to the layers below. |
| `knowledge/specs/` | The task touches the concept | Durable intent, constraints, tradeoffs, and the success bar. Why and what, never exhaustive how. |
| `.agents/skills/` | The named workflow is requested | Execution workflow for a request the user can name (`/ship`, `/release`). |
| Code, tests, benches, `evals/` | The task reaches them | The authoritative how, colocated with what it describes. |

Detail belongs at the deepest layer that can hold it. Operational how-to about
a directory belongs in that directory — bench procedure next to the benches,
eval procedure next to the evals — not in `AGENTS.md`.

## Skills

A skill is a lightweight guide for retrieving the right context, not a
procedure manual. `SKILL.md` carries the goal, when to use it, the outcomes
that define success, and the decision points. Reference material — templates,
long command sequences, per-surface checklists — goes in sibling files under
the skill directory and is read when that part of the work comes up.

Constrain hard only where it matters: gates that protect users, published
artifacts, or credentials. Everywhere else, the outcome is the instruction.

## Ownership boundary

- A spec MUST NOT restate a skill's workflow, and a skill MUST NOT restate a
  spec's rationale. Each links to the other.
- `AGENTS.md` MUST NOT duplicate a spec's bar or a skill's steps. It names them
  and links.
- Public documentation stays outside this boundary entirely; see
  [`documentation.md`](./documentation.md).

## Change requirements

- Adding a rule means choosing its single owner first. If it already exists
  elsewhere, update that copy instead of adding a second.
- Growing `AGENTS.md` is a signal, not a neutral act: prefer moving the detail
  to the layer that owns it and leaving a pointer.
- When a skill file grows past the point where its opening section still
  answers "what am I being asked to do", split the reference material out.

## Related

- [`documentation.md`](./documentation.md) — the public/internal documentation boundary.
- [`skills.md`](./skills.md) — the runtime skills capability yolop ships.
- [`okf.md`](./okf.md) — why the knowledge bundle is plain OKF markdown.

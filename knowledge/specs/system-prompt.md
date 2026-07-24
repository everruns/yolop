---
type: Product Specification
title: System prompt composition
description: Defines the system prompt composition contract for Yolop.
---

# System prompt composition

Status: implemented.

## Scope

This is the prompt **yolop ships to the model at runtime** — `system.md` plus
capability contributions. The context *this repository* presents to coding
agents reading it is a different subject, owned by
[agent context](agent-context.md).

The two share their principles — one owner per instruction, progressive
disclosure, judgment over ritual, prefer references in code — and this spec does
not restate them. What follows is only what is specific to a prompt assembled
per turn from capabilities.

## Why

Yolop's system prompt is not a file. `src/runtime/system.md` is around a
kilobyte, but the prefix the model actually reads is that file plus a block from
every enabled capability plus `AGENTS.md`. For a long time only the file was
watched, so the file stayed lean while the assembly grew to roughly five times
its size — most of it prose restating what a tool's own `description()` already
said.

Restating is not free and not neutral. It is paid on every turn including the
turns that never touch the capability, it competes with the task for attention,
and two copies of the same instruction drift apart. Newer models also need less
of it: they infer sequencing and stylistic decisions from context that earlier
models had to be told.

## What

### One fact, one home

A fact belongs in exactly one place, chosen by what the fact is about:

| Fact is about | Home |
| --- | --- |
| How to call a tool — arguments, sequencing, preconditions | that tool's `description()` |
| What to do about a specific result — truncation, a guard warning, a failure | the tool result itself |
| That a capability exists at all, when nothing else reveals it | a capability prompt block |
| Cross-cutting policy — safety, untrusted input, output shape | `system.md` |

A capability block that names its own tools and explains how to call them is
duplicating the tool description; delete it. `progress_guard` and `repo_map` are
the reference cases for the second row: both emit their guidance inside the
result, at the moment it applies, so neither needs a block.

### Discovery is always on; how-to is reveal-gated

`tool_search` hides deferred tools' parameter schemas until the model asks for
them ([tool search](tool-search.md)), but names and descriptions always ride. A
capability block therefore splits in two:

- **Discovery** — "this capability exists", "these memories are stored". Always
  contributed. Gating it would be circular: the model cannot ask to reveal a
  tool it has no reason to know about.
- **How-to** — argument shapes, file locations, when an edit takes effect.
  Contributed only once `tool_search` has revealed one of the capability's
  tools, because until the schema loads the model cannot act on it anyway.

`capabilities::tool_reveal` implements the gate. A `PostToolExecHook` reads
`tool_search`'s structured `loaded` field into a session-keyed registry, and
gated capabilities consult it in `system_prompt_contribution`. Reads come from
what the model was actually shown rather than a mirror of upstream state, so the
two cannot drift. Reveals are per session and the registry is bounded.

`config` and `memory` are gated. `memory` splits along the line above: titles
disclose every turn, the framing paragraph waits for a reveal, and empty memory
with no reveal contributes nothing.

### Judgement yields to measurement

Judgment over ritual applies here as it does everywhere
([agent context](agent-context.md)); the shapes to watch for in a prompt are
numeric thresholds ("only for work with at least three steps"), counters ("if
stuck twice, ask"), and habit instructions ("do not repeat passing checks").

The prompt-specific caveat is that the preference is a default, not an absolute.
The `lsp` block is deliberately directive because softer phrasing drove adoption
to near zero in `evals/lsp_integration`. Where an eval contradicts the
preference, the eval wins — and a preference is not evidence.

### The budget is a test

`always_on_capability_prompts_within_budget` sums the always-on static blocks
and fails past a cap; `system_prompt_within_budget` covers `system.md`. Trimming
once does not keep anything trimmed — each new capability's prose looks small on
its own — so growth has to trip a gate. Raising a cap is a deliberate edit
justified in the commit, never a silent side effect.

## Validation

Prompt changes are behavioral, and yolop is multi-provider: a prompt that suits
one model's judgement may not suit another's. Prompt-composition changes are
A/B'd through [`evals/harness_basic`](../../evals/harness_basic/README.md)
before they are trusted.

Lean-versus-verbose is a **binary** comparison, not a harness variant — the
verbose prompt is an earlier revision of yolop, so it is the `baseline` arm
against `candidate`. Reveal gating is a **harness** variant (`no-tool-reveal`),
because it is a capability toggle on one binary.

## Non-goals

- No per-model prompt variants. Composition is uniform across providers; when a
  model needs different phrasing, the evidence for it comes from an eval first.
- No prompt content in `AGENTS.md`'s place. Project policy lives there and is
  loaded by the `agent_instructions` capability, deliberately late in the prefix
  so it does not invalidate the cached stable prefix.

## Related

- [agent context](agent-context.md) — the same principles applied to the context
  this repository presents to agents reading it.
- [tool search](tool-search.md) — the deferral mechanism reveal gating rides on.
- [memory](memory.md) — the two-tier disclosure this contract produces.

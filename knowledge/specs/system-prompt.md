---
type: Product Specification
title: System prompt composition
description: Defines the system prompt composition contract for Yolop.
---

# System prompt composition

Status: implemented.

## Scope

This is the prompt **yolop ships to the model at runtime**: `system.md` plus
capability contributions. The context *this repository* presents to coding
agents reading it is a different subject, owned by
[agent context](agent-context.md).

The two share their principles, one owner per instruction, progressive
disclosure, judgment over ritual, prefer references in code, and this spec does
not restate them. What follows is only what is specific to a prompt assembled
per turn from capabilities.

## Why

Yolop's system prompt is not a file. `src/runtime/system.md` is around a
kilobyte, but the prefix the model actually reads is that file plus a block from
every enabled capability plus `AGENTS.md`. For a long time only the file was
watched, so the file stayed lean while the assembly grew to roughly five times
its size, most of it prose restating what a tool's own `description()` already
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
| How to call a tool, arguments, sequencing, preconditions | that tool's `description()` |
| What to do about a specific result, truncation, a guard warning, a failure | the tool result itself |
| That a capability exists at all, when nothing else reveals it | a capability prompt block |
| Cross-cutting policy, safety, untrusted input, output shape | `system.md` |

A capability block that names its own tools and explains how to call them is
duplicating the tool description; delete it. `progress_guard` and `repo_map` are
the reference cases for the second row: both emit their guidance inside the
result, at the moment it applies, so neither needs a block.

### Discovery is always on; how-to is reveal-gated

`tool_search` hides deferred tools' parameter schemas until the model asks for
them ([tool search](tool-search.md)), but names and descriptions always ride. A
capability block therefore splits in two:

- **Discovery**: "this capability exists", "these memories are stored". Always
  contributed. Gating it would be circular: the model cannot ask to reveal a
  tool it has no reason to know about.
- **How-to**: argument shapes, file locations, when an edit takes effect.
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

### The host's rendering is a fact the model cannot infer

An answer is text until something paints it, and what paints it differs per
host: the TUI renders the transcript itself, `--print` writes raw text, and an
ACP editor renders the markdown its own way. The model cannot deduce which one
it is talking to, so anything it might reasonably decide *because* of the answer
being rendered belongs in `<environment_context>` next to `client_ui`.

`ui_capabilities` is that field: an additive list of what the host is known to
render (`supports_markdown`, `supports_markdown_mermaid`), or `none` for a host
that renders nothing. Additive is the point. A listed capability is a claim
yolop can stand behind; an absent one means "not known to render", which is the
only honest thing to say about Mermaid over ACP, the editor renders the
markdown, and the protocol never says how. A per-capability true/false would
force a guess there, and `none` beats omitting the field, which would read as a
value yolop failed to compute.

State the capability, not the instruction: whether a diagram is worth drawing is
the model's call.

Keep these fields *static per host*. Live values that change mid-session, the
terminal width, the current scroll position, would rewrite the prefix on every
resize and cost the cached prefix that `AGENTS.md`'s placement exists to
protect. A capability that only holds at some widths still advertises `true`;
degrading gracefully at the narrow end is the renderer's job, not the prompt's.

### Live model identity rides the conversation, not the prefix

The effective provider, model, and reasoning effort can change while a session
is open through ACP configuration or Yolop's live-config tools. The provider
receives reasoning effort as a request control, but that control is not visible
to the model as conversation content; asking the model which effort it uses can
therefore produce an answer based on stale dialogue.

`model_runtime_context` adds a trusted `<runtime_context>` annotation to the
prompt-facing view of the newest user message on the first turn and whenever
one of those values changes. Stored messages and rendered transcripts remain
untouched. Each annotation is retained against its stable message id in later
model views, so unchanged turns extend the same provider-cache prefix instead
of rewriting the system prompt. If compaction or checkpoint rewind removes the
last annotated message, the capability re-emits the current values on the
newest surviving user message.

The request control remains present on every turn; only the model-visible
annotation is change-triggered. This separates execution correctness from
conversational awareness.

### Judgement yields to measurement

Judgment over ritual applies here as it does everywhere
([agent context](agent-context.md)); the shapes to watch for in a prompt are
numeric thresholds ("only for work with at least three steps"), counters ("if
stuck twice, ask"), and habit instructions ("do not repeat passing checks").

The prompt-specific caveat is that the preference is a default, not an absolute.
Two blocks are directive on measured grounds:

- `lsp`, softer phrasing drove adoption to near zero in `evals/lsp_integration`.
- `repo_map`, its truncation rule was deleted on the reasoning that the same
  words already ship inside the truncated result, and `repo-map-bounded`
  regressed on gpt-5.5: repeated exploration calls breached their zero bar in
  5/5 trials against 2/5 before.

Where an eval contradicts the preference, the eval wins, and a preference is
not evidence.

### Investigation earns an owner before mutation

For a non-obvious bug, the model identifies the root cause and the abstraction
that owns it from repository evidence before its first mutation. This is a
semantic threshold, not a read counter: an explicit local edit can proceed after
one targeted read, while an investigated bug must not stop at the first visible
adapter boundary.

This policy stays in the stable prompt because it must cover every mutation
path, including arbitrary shell scripts whose target is not observable to a
pre-tool hook. A runtime evidence counter was rejected after it added ceremony
to simple local-edit controls without reliably recognizing ownership. The
`owner-selection` harness study owns the positive and negative cases.

### Reactive text does not substitute for anticipatory text

The `repo_map` regression refines the "fact is about a specific result" row
above. Putting guidance in the tool result is right when it tells the model what
a result *means*. It is not sufficient when the guidance must shape the choice
the model makes *on seeing* that result: by then the next call is already being
formed, and a rule it has been carrying is not the same as a rule it has just
been handed. `progress_guard` still earns no stable prompt block: its result
warning activates a host-enforced pre-tool gate, and the bounded checkpoint's
how-to lives in the `progress_checkpoint` tool schema. See
[progress guard trajectory control](progress-guard.md).

The distinction is also model-specific. The same deletion that cost gpt-5.5 an
extra call per truncated map was free on claude-sonnet-4-5. Prompt content that
looks redundant against one model's judgement can be essential for another's,
which is why composition changes get A/B'd on both providers rather than
reasoned about.

### Parallelism follows data dependencies

Yolop asks providers for parallel tool calling and tells the agent to emit
independent calls together. A call whose arguments depend on an earlier result
stays in a later model round. Title, todo, and live-status updates piggyback on
substantive tool batches when independent work is ready; they remain ordinary
runtime tools, so event replay and every host keep the same semantics.

The `orchestration-efficiency` study compares the provider affordance, the
dependency-aware prompt policy, and their combination. On the initial gpt-5.5
study, the combined arm preserved 9/9 task success, reduced mean model calls by
27%, raised mean tool batch width from 1.27 to 1.89, cut standalone
bookkeeping rounds from 20 to 6, and reduced cumulative input including cache
reads by 26%. The dependent-read control never co-batched the read that
discovers a path with the read of that path. Provider preference alone was
neutral; the combination is retained because it improved width and latency
beyond the policy-only arm without changing dependency safety.

Deterministic host-side title or todo handling is not the shortcut: it would
create a second owner for behavior currently recorded as runtime tool calls and
would make session replay diverge from live execution.

Independent file discovery also has a structured path that does not depend on
parallel tool emission. The filesystem capability owns `read_many_files`, a
bounded ordered read for paths known before the call. Yolop keeps that schema
eager beside `read_file`; a derived path remains a dependency and is read in a
later round.

The `batch-native-discovery` study isolates the dependency bump with matched
serial-emission binaries. Across three trials, one batch read reduced mean task
LLM calls from 4 to 2 and cumulative input including cache reads from 53,412 to
27,583 tokens. All six dependent controls kept the route and derived path in
separate rounds with logical read width 1.

### Tool schemas follow the task without rewriting the prefix

The stable prompt and tool catalogue always disclose every capability name and
description. Only parameter schemas are progressive: the first-turn profile
keeps repository discovery, including the compact `repo_map` schema,
bookkeeping, and the mandatory progress-checkpoint transition eager, while
mutation, background, release/control, `repo_symbols`, and other specialized
schemas load through `tool_search`. An explicitly enabled host profile may add
eager schemas, and extension manifests may opt individual tools out of
deferral.

This policy is static for a host/session. A model classifier must not rewrite
the system prefix from the wording of each new task; that would make cache
behavior volatile and could trap a turn in an incapable profile. Deferred tools
remain discoverable and executable after reveal on every provider, including
providers that require registered structured-call schemas.

The composition regression records the undeferred baseline and candidate
through the assembled runtime entry point. On the current default surface, the
stable prompt remains capped at 12,888 bytes, provider-visible tool definitions
must fall by at least 24%, and the historical parameter-schema surface by at
least 45%. Keeping the mandatory checkpoint and bounded batch-read schemas eager
intentionally spends schema bytes so neither a host-required transition nor an
independent-read plan depends on another schema-discovery round. The compact
`repo_map` schema spends another 261 bytes so the first broad repository
orientation has its real argument contract. Its tool description states that
fields are optional and records the compact defaults. Making `repo_symbols`
eager offered no recovery benefit and exceeded the unchanged schema floor, so
it stays deferred.

### The budget is a test

`always_on_capability_prompts_within_budget` sums the always-on static blocks
and fails past a cap; `system_prompt_within_budget` covers `system.md`. Trimming
once does not keep anything trimmed, each new capability's prose looks small on
its own, so growth has to trip a gate. Raising a cap is a deliberate edit
justified in the commit, never a silent side effect.

## Validation

Prompt changes are behavioral, and yolop is multi-provider: a prompt that suits
one model's judgement may not suit another's. Prompt-composition changes are
A/B'd through [`evals/harness_basic`](../../evals/harness_basic/README.md)
before they are trusted.

Lean-versus-verbose is a **binary** comparison, not a harness variant, the
verbose prompt is an earlier revision of yolop, so it is the `baseline` arm
against `candidate`. Reveal gating is a **harness** variant (`no-tool-reveal`),
because it is a capability toggle on one binary.

Tool-round orchestration uses the four binary arms in the
`orchestration-efficiency` preset: unchanged baseline, provider preference
only, prompt policy only, and the combined candidate.

Batch-native discovery uses the dependency baseline and candidate arms in the
`batch-native-discovery` preset. Both eval binaries disable provider parallel
tool emission so task LLM calls measure the structured batch itself.

### A profile may add a standing job

A named profile's `instructions` / `instructions_file`
([configuration](configuration.md)) are appended once, right after `system.md`,
under a `## Profile instructions` heading and ahead of the capability blocks'
per-turn contributions. That is the one seam where the operator, not a
capability, adds prompt text: a profile exists to make yolop a particular agent
for a run (triage, review, release duty), and that job cannot be expressed by
any capability's own block. It stays subject to the same budget discipline as
`system.md`, since it is paid on every turn of that profile's sessions.

Project policy still belongs in `AGENTS.md` and durable user preference in
memory. The distinguishing question is lifetime: an instruction that should
apply whenever *this repository* is open is not a profile instruction.

## Non-goals

- No per-model prompt variants. Composition is uniform across providers; when a
  model needs different phrasing, the evidence for it comes from an eval first.
- No prompt content in `AGENTS.md`'s place. Project policy lives there and is
  loaded by the `agent_instructions` capability, deliberately late in the prefix
  so it does not invalidate the cached stable prefix.

## Related

- [agent context](agent-context.md), the same principles applied to the context
  this repository presents to agents reading it.
- [tool search](tool-search.md), the deferral mechanism reveal gating rides on.
- [memory](memory.md), the two-tier disclosure this contract produces.

---
name: ship
description: Goal-oriented workflow for landing a requested change to yolop safely. Use when the user asks to ship, fix and ship, take a change through validation, or drive PR/CI/merge to completion.
metadata:
  internal: true
user-invocable: true
---

# Ship

Goal: land the requested change safely, with evidence, and merge only after CI
is green.

Read [`knowledge/specs/shipping.md`](../../../knowledge/specs/shipping.md) § Required Outcomes first — it
owns the bar every shipped change must clear. This skill owns how to reach it.

"Fix and ship" means implement first, then switch into shipping mode.

## Working the change

Start from the goal and the changed risk surface, not from checklist order.
Review the delta (`git diff origin/main...HEAD`, `git log origin/main..HEAD`),
confirm the requested behavior is actually implemented, then pick the smallest
evidence that would convince a skeptical reviewer — targeted diff review,
focused tests, then the [checks in `AGENTS.md`](../../../AGENTS.md) for the
surfaces you touched.

Two pieces of evidence are not interchangeable, and neither substitutes for the
other:

- **The feature test.** It must drive the changed behavior's real entry point —
  dispatch the command, call the handler, run the turn — not assert on a
  constructor or on adjacent code that still compiles. Changes to transcript
  output, live activity, or status values assert the terminal-independent
  presentation model; terminal-buffer tests are layout coverage, not proof of
  visible semantics.
- **The smoke test.** Run the affected flow end to end. Agent-loop changes go
  through a live provider (`doppler run -- cargo run -- --provider openai -p
  "<focused prompt>"`); for offline-safe changes, `--provider llmsim` proving
  the binary still starts is enough.

Before pushing, reread the diff for duplication and accidental complexity you
introduced, and fix it.

Stop and report only for blockers you cannot resolve alone: merge conflicts you
cannot judge, missing credentials, ambiguous product intent, CI failures you
cannot reproduce.

## Security review

Mandatory for every change touching code, configuration, or infrastructure —
perceived low risk does not excuse it. Yolop is a coding agent with disk and
shell access on the user's host, so the categories are concentrated:

- **TM-FS** — filesystem. The write blocklist still covers `.git/`,
  `node_modules/`, `target/`, `dist/`, `build/`, `.next/`, `.venv/`, `venv/`,
  `.tox/`, `.gradle/` at any depth; reads stay unrestricted only inside the
  workspace root.
- **TM-BASH** — shell execution. Timeouts and per-stream output caps survive.
  Any change to `tools.rs` must preserve the bounded execution model.
- **TM-LLM** — prompt construction and key handling. Keys are read from process
  env only, and never logged or written to session JSONL.
- **TM-TOOL** — capability registration. New capabilities respect the write
  blocklist.
- **TM-DEP** — dependency risk. A new crate needs a one-line justification.
  `everruns-*` versions move together; mismatched versions are a soft API break.

For each relevant category, check the diff for injection (command, prompt, path
traversal), data exposure, missing validation at trust boundaries, and resource
exhaustion (unbounded loops, missing limits).

Record the result in the PR body under **Security**. Docs-only, comment-only, or
test-only changes may state "No security-relevant code changes" with a one-line
justification.

## PR and merge

Write the body around functional change and impact — what changed, why, how it
was validated, notable risks — using
[`.github/pull_request_template.md`](../../../.github/pull_request_template.md).
Two sections are never omitted: **Security** (above) and **Follow-ups**
(everything deferred, one line of rationale each, or "No follow-ups."). Default
to implementing in-scope work rather than deferring it.

Record the knowledge decision explicitly: either the concepts you updated, or
"No knowledge update required" with a reason.

Every review comment gets an inline reply on its own thread and is marked
resolved — including nits, low-confidence suggestions, and bot comments. A reply
is required even when the resolution is a pure code change.

Merge with `gh pr merge --squash` after CI is green and a final comment sweep is
clean; give async reviewer bots at least 2 minutes after CI turns green. Do not
enable auto-merge — bots can post after the last push. After the merge lands,
watch main CI for the merge commit and fix or revert promptly if it fails.

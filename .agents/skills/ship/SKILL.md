---
name: ship
description: Goal-oriented workflow for landing a requested change to yolop safely. Use when the user asks to ship, fix and ship, take a change through validation, or drive PR/CI/merge to completion.
metadata:
  internal: true
user-invocable: true
---

# Ship

Goal: land the requested change safely, with evidence, and merge only after CI is green.

This skill implements [`specs/shipping.md`](../../../specs/shipping.md). Keep operational guidance here. Keep the shipping success bar and constraints in the spec.

This skill is outcome-oriented. Do not blindly walk a fixed checklist. Start from the goal and changed risk surface, then choose the smallest path that proves the change is ready.

## When To Use

Use this skill when the user asks to:

- ship or fix and ship a change
- take work through validation, PR creation, CI, and merge
- prove a branch is merge-ready

## Required Outcomes

**ALL outcomes below are MANDATORY. Do not skip or weaken any requirement.**

1. **The branch state is safe.**
   - Do not ship from `main` or `master`.
   - The working tree must be clean before the final push.
   - Prefer rebasing onto the latest `origin/main` before merge.

2. **The requested goal is achieved with evidence.**
   - Review the delta with `git diff origin/main...HEAD` and `git log origin/main..HEAD`.
   - Confirm the requested behavior is actually implemented.
   - Validation must match risk. For bugs, prefer a failing test first when practical.

3. **The changed code is fit to merge.**
   - Simplify obvious duplication or accidental complexity.
   - Perform the structured security review below.
   - Fix issues you find and refresh the evidence.

4. **Relevant artifacts stay in sync.**
   - Update only the artifacts affected by the change: `specs/`, `AGENTS.md`, `README.md`.

5. **Smoke test impacted functionality.**
   - Always smoke test the flows affected by the change end-to-end.
   - For changes that touch the agent loop, run `doppler run -- cargo run -- --provider openai -p "<focused prompt>"` and read the output.
   - For offline-safe changes, `cargo run -- --provider llmsim -p "hi"` is enough to prove the binary still starts.
   - Docs-only or config-only changes that do not affect runtime may skip smoke testing with explicit justification.

6. **Follow-ups are surfaced, not silently dropped.**
   - Default to implementing everything in scope before merging.
   - For each candidate, decide explicitly: **implement now** (preferred) or **defer**.
   - For anything deferred, list it under a **Follow-ups** section in the PR body with a one-line rationale.
   - If there are no follow-ups, state "No follow-ups." in the PR body.

7. **The PR is mergeable and merged safely.**
   - Push the branch.
   - Create or update the PR.
   - Address every review comment — including low-confidence suggestions, nits, and bot comments. For each comment, either apply the fix or post a reply inline on the same thread with a clear explanation, then mark that thread resolved. No comment may be left unanswered or unresolved before merge.
   - Wait for CI to go green.
   - Merge with squash only after CI is green and the final review sweep is clean.
   - After merging, monitor main CI for the merge commit. If it fails, fix or revert promptly.

## Operating Model

- Start from the goal and risk surface, not checklist order.
- Choose the highest-signal path first: targeted diff review, focused tests, relevant builds, then smoke tests.
- "Fix and ship" means implement first, then switch into shipping mode.
- Stop only for blockers you cannot safely resolve alone: merge conflicts, missing credentials, ambiguous product intent, or CI failures you cannot reproduce.

## Security Review

Mandatory for every change that touches code, configuration, or infrastructure. Yolop is a coding agent with disk and shell access on the user's host, so the relevant categories are concentrated:

- **TM-FS** — filesystem access. Verify the write blocklist still covers `.git/`, `node_modules/`, `target/`, `dist/`, `build/`, `.next/`, `.venv/`, `venv/`, `.tox/`, `.gradle/` at any depth. Verify reads remain unrestricted only inside the workspace root.
- **TM-BASH** — shell execution. Verify timeouts, output caps, and approval-gate behavior. Any change to `tools.rs` must preserve the bounded execution model.
- **TM-LLM** — prompt construction and API key handling. Verify keys are never logged or written to session JSONL. Verify provider env vars are read from process env only.
- **TM-TOOL** — capability registration. Verify new capabilities respect the same approval gate and blocklist.
- **TM-DEP** — dependency risk. New crates need a one-line justification. Bump `everruns-*` versions together; mismatched versions are a soft API break.

For every relevant category, check the diff for: injection (command/prompt/path traversal), data exposure (keys in logs, session files), input validation at trust boundaries, dependency risk, and resource exhaustion (unbounded loops, missing limits).

Document the review in the PR body under **Security**. Changes that are purely docs, comments, or test-only may state "No security-relevant code changes" with a one-line justification.

## Common Evidence Commands

Pick only what matches the changed surface:

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features`
- `cargo fetch --locked`
- `cargo build --release` (when changing binary surface or release profile)
- `doppler run -- cargo test --all-features --test integration` (when a live-provider integration test is relevant)
- `doppler run -- cargo run -- --provider openai -p "<smoke prompt>"`

## PR And Merge

- Use a Conventional Commit style PR title under 70 characters.
- In the PR body, explain what changed, why, how it was validated, notable risks, and an explicit **Follow-ups** section (or "No follow-ups.").
- After CI is green, wait at least 2 minutes for async reviewer bots, then do one last comment sweep before merge.
- Merge with `gh pr merge --squash` only after CI is green and the final review sweep is clean.
- Do not use auto-merge: async review bots can post after the last push.

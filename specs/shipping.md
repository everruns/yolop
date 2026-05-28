# Shipping Specification

## Abstract

This specification defines goal-oriented shipping for yolop. Shipping completes the requested goal, gathers convincing evidence, creates a mergeable PR, and merges only after CI is green.

The canonical agent workflow lives in [`.agents/skills/ship/SKILL.md`](../.agents/skills/ship/SKILL.md). That skill is user-invocable so shipping can be requested directly as `/ship`.

## Design Goals

1. Reach the requested goal, not just perform rituals around it.
2. Match validation depth to the actual risk surface.
3. Keep affected artifacts in sync (`README.md`, `AGENTS.md`, `specs/`).
4. Merge only from a safe branch state with green CI.

## Ownership Boundary

- This spec owns the shipping intent, constraints, and success bar.
- The skill owns the execution workflow, heuristics, and commands.

## Required Outcomes

Every shipped change MUST satisfy ALL of these. These are mandatory, not suggestions.

1. **Safe branch state.** No shipping from `main` or `master`. Working tree clean before final push. Prefer rebasing onto latest `origin/main` before merge.
2. **Goal achieved with evidence.** The requested behavior is implemented and validated with proof matching the risk.
3. **Merge-ready code.** Touched code is reviewed for avoidable complexity. A structured security review is performed (see `.agents/skills/ship/SKILL.md` § Security Review). Issues are addressed or explicitly blocked.
4. **Synced artifacts.** Affected artifacts are updated: README, AGENTS.md, specs. No code-duplicating prose.
5. **Smoke test impacted functionality.** Mandatory unless the change is docs-only or config-only with explicit justification. For runtime changes, run at least one live-provider smoke through Doppler.
6. **Follow-ups surfaced.** TODOs, partial fixes, declined suggestions, missed edges, and spec/doc drift are explicitly listed under **Follow-ups** in the PR body (or `"No follow-ups."` if none).
7. **Safe merge.** PR uses the template, CI is green, every review comment is addressed, answered inline on its own thread (code change or written explanation), and marked resolved before merge. Merge is squash-only after a final clean comment sweep. Async reviewer bots get at least 2 minutes to comment after CI turns green.

## Constraints

- Shipping is outcome-oriented, not a mandatory linear checklist.
- Validation starts with the smallest high-signal proof and deepens only when risk requires it.
- Bug fixes prefer a failing test before the fix when practical.
- Docs-only or config-only changes may skip code tests if the choice is justified and the relevant lint/build was run.
- Security review is mandatory for code, configuration, or infrastructure changes. Perceived low risk does not justify skipping it.
- Every review comment must be explicitly addressed, answered inline on its own thread, and resolved before merge — including low-confidence suggestions, nits, and bot comments.
- Auto-merge is not used: async reviewer bots can post after the last push or after CI turns green.
- If a blocker cannot be resolved safely by the agent alone, shipping stops and reports rather than guesses.

## Validation Menu

Use the smallest set that gives high confidence.

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test --all-features`
4. `cargo build --release` when changing the binary surface or release profile.
5. `doppler run -- cargo test --all-features --test integration` for live-provider proof when the change touches the agent loop or tool wiring.
6. `doppler run -- cargo run -- --provider openai -p "<focused prompt>"` for end-to-end smoke.
7. `cargo run -- --provider llmsim -p "hi"` for offline smoke when CI access to provider keys is unavailable.
8. Ensure test coverage proves the fix or acceptance criteria, including important negative paths.
9. Update `README.md`, `AGENTS.md`, and `specs/` when relevant.

## Merge Discipline

- Conventional Commits PR titles under 70 characters.
- Squash and Merge only.
- GitHub Actions is the CI source of truth.
- Never merge red CI.
- After merging, monitor main CI for the merge commit. If it fails, treat it as a shipping regression and fix or revert promptly.

## Related

- [`specs/maintenance.md`](./maintenance.md)
- [`.agents/skills/ship/SKILL.md`](../.agents/skills/ship/SKILL.md)
- [`.agents/skills/maintenance/SKILL.md`](../.agents/skills/maintenance/SKILL.md)

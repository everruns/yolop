# Manual test scenarios

Scenarios a person or agent runs by hand against a real Yolop build, kept here
because the automated suite cannot reach them. Each one names an observable
outcome and the acceptance criteria that decide pass or fail.

A scenario earns a place here only when `cargo test` cannot own it. The usual
reasons are a live provider, network access, a real terminal, or a judgement
call about what the screen looks like. Anything that a test could assert belongs
in a test instead, see [`shipping.md`](../specs/shipping.md), which still
requires an automated test for every behavior change. These scenarios are
additional coverage, never a substitute.

## Scenarios

| Scenario | Area | Needs |
| --- | --- | --- |
| [Install a registry skill and render its diagram](skill-install-mermaid-render.md) | Skills, transcript rendering | Live provider, network |

## Related manual checks

Not every manual check belongs here. Two live elsewhere because another concept
owns when they run:

- The human tier of [terminal verification](../specs/release.md#terminal-verification)
, walking `yolop tuika-gallery` across real GUI emulators, stays in the
  release spec, which owns the pre-release gate that requires it.
- [`surfaces.md`](../../.agents/skills/maintenance/surfaces.md) carries the
  commands a maintenance pass runs by hand. It checks repository health, not
  product behavior.

[Release](../specs/release.md#pre-release-checklist) draws its smoke paths from
the scenarios below, so keeping them current is what keeps a release cut from
improvising one.

## Running one

Build first (`cargo build`), then follow the scenario's Setup and Steps exactly.
Record the result as pass or fail against the acceptance criteria, not against
whether the run "looked fine". A scenario that passes for the wrong reason is
worse than one that fails.

Where a scenario has known-benign variation, a model wording things
differently, a diagram laid out differently, it says so under Expected
variation. Treat anything outside that as a failure worth reporting.

## Adding one

Name the file for the outcome it verifies, not the feature it touches, and give
it OKF frontmatter with `type: Test Scenario`. Cover these sections:

- **Purpose**: the outcome under test, and why an automated test cannot own it.
- **Preconditions**: credentials, network, platform, terminal size.
- **Setup**: commands that put the workspace in a known state. Point at a
  checked-in fixture script rather than restating steps; a script stays true,
  prose drifts.
- **Steps**: what the operator types, verbatim.
- **Acceptance criteria**: each one independently checkable, phrased so two
  people would agree on pass or fail.
- **Expected variation** and **Known failure modes**: what is benign, and what
  looks like a product bug but is not.

Then add a row to the table above.

---
type: Process Specification
title: Release Specification
description: Defines the release contract for publishing Yolop and its supporting crates.
---

# Release Specification

## Abstract

This spec defines how yolop is cut, published, and verified. Releases are
agent-prepared, human-merged, and CI-published to two registries: crates.io
and the `everruns/homebrew-tap` Homebrew tap.

The canonical agent workflow lives in
[`.agents/skills/release/SKILL.md`](../../.agents/skills/release/SKILL.md). That
skill is user-invocable as `/release`.

## Versioning

Yolop follows [Semantic Versioning](https://semver.org/):

- **MAJOR** (X.0.0): incompatible CLI flags, removed providers, breaking config
- **MINOR** (0.X.0): new features, new tools, new providers
- **PATCH** (0.0.X): bug fixes, documentation, dependency bumps

Pre-1.0 (current): minor bumps may carry breaking changes if they are flagged
in the changelog.

## Release Targets

Every yolop release ships to:

| Target          | Surface                                  | How users install                       |
|-----------------|------------------------------------------|-----------------------------------------|
| GitHub Release  | tag `vX.Y.Z`, source archive, binaries   | `gh release download vX.Y.Z`            |
| crates.io       | `yolop` binary, the `yolop-yep` library, and the first-party extension crates | `cargo install yolop --locked`          |
| Homebrew tap    | formula at `everruns/homebrew-tap`       | `brew install everruns/tap/yolop`       |

The `yolop-yep` extension SDK is versioned independently of `yolop` and is
published as a side effect of a `yolop` release only when its in-tree version
isn't already on crates.io (see § `publish.yml`). Extension authors consume it
on its own (`cargo add yolop-yep`).

First-party extensions in `extensions/` are versioned independently too, and
ride the same release on the same terms: `publish.yml` publishes any whose
in-tree version isn't live yet, and `cli-binaries.yml` builds their servers for
the three CLI targets and uploads them under a per-extension tag
(`<crate>-v<version>`). A published extension crate ships source, so an
extension whose code changed and whose version did not is published nowhere and
gets no new binaries, while the manifest keeps pointing at the previous tag.
Bumping a changed extension, in both its `Cargo.toml` and its `plugin.json`,
is therefore part of preparing the release; a test pins the two together.

The TUI toolkit yolop renders through, `tuika` and `tuika-codeformatters`, is
**not** released from here. It ships from
[`everruns/tuika`](https://github.com/everruns/tuika) on its own schedule, and a
yolop release simply depends on whatever version is already live. A yolop
release that needs new toolkit behavior therefore waits on a tuika release
first; see [`tuika.md`](./tuika.md).

Prebuilt CLI binaries are produced for:

| OS    | Target                       | Runner          | Accelerated build |
|-------|------------------------------|-----------------|-------------------|
| macOS | `aarch64-apple-darwin`       | `macos-latest`  | `metal`           |
| macOS | `x86_64-apple-darwin`        | `macos-latest`  | `metal`           |
| Linux | `x86_64-unknown-linux-gnu`   | `ubuntu-latest` | `cuda`            |

Every target ships twice: `yolop-<target>.tar.gz` built with default features,
and `yolop-<target>-<backend>.tar.gz` carrying the local-inference engine and
that backend. The plain one is what the Homebrew formula points at, because it
is the only build that runs wherever the target does; see
[Local inference](local-inference.md#distribution) for why the split exists.

## Release Flow

```
┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
│ Human    │   │ Agent    │   │ Agent    │   │ Human    │   │ CI       │   │ Agent    │
│ asks     │──>│ prepares │──>│ verifies │──>│ merges   │──>│ tags +   │──>│ monitors │
│ release  │   │ PR       │   │ publish  │   │ PR       │   │ publishes│   │ registries│
└──────────┘   └──────────┘   └──────────┘   └──────────┘   └──────────┘   └──────────┘
```

Skipping `verify-can-publish` risks tagging a release that fails to publish.
Skipping `monitor-published` risks declaring "shipped" while one of the two
registries silently failed.

### Human Steps

1. **Ask the agent** to create a release:
   - "Cut release v0.2.0"
   - "Prepare a patch release"
2. **Review the PR** the agent opens, including its publish-readiness report.
3. **Squash and merge**: CI handles the GitHub Release, crates.io publish,
   binary builds, and Homebrew formula update.
4. **Ask the agent to monitor** (or let it auto-monitor if subscribed to PR
   activity) until both registries report the new version.

### Agent Steps

The procedure lives in [`.agents/skills/release/SKILL.md`](../../.agents/skills/release/SKILL.md). These are the
constraints it must satisfy:

1. **The commit set is complete.** Cloud sandboxes are often shallow-cloned,
   which silently hides commits and yields a wrong commit count or changelog.
   Full history is established before anything is counted or listed.
2. **The version is confirmed.** Either given by the human, or proposed from the
   unreleased commits under § Versioning and confirmed before proceeding.
3. **The changelog is honest.** Every commit since the previous tag appears, in
   the format defined in § Changelog Format.
4. **Versions agree.** `Cargo.toml` and `Cargo.lock` read `X.Y.Z`, and each
   separately versioned library crate carries a version consistent with every
   workspace path dependency requirement that references it.
5. **Publish-readiness is proven before the PR opens.** The library dry-runs
   succeed and `X.Y.Z` exceeds what crates.io serves. A release PR is never
   opened, and never merged, with a known-broken publish path.
6. **Post-merge verification is independent.** Green workflows are not evidence
   of a release. The agent checks crates.io and the Homebrew tap itself and
   declares **shipped** only when both report the new version. A failure rolls
   forward via hotfix rather than leaving the release half-published.

**Ordered publish.** `yolop` and the extensions depend on `yolop-yep` by
version, so crates.io requires it live first. `publish.yml` derives the
dependency-first order from Cargo metadata via `scripts/publish_order.py`
(currently `yolop-yep`, `yolop-extension-logfire`, `yolop`) and skips versions
already live, so a new publishable workspace member cannot be silently omitted.
A consequence: `cargo publish --dry-run` fails locally for anything depending on
a `yolop-yep` version that isn't on crates.io yet. That is expected, not a
broken release, `yolop-yep` is dry-run locally and CI validates the dependents
after it goes live.

## CI Automation

### `release.yml`

- **Trigger**: push to `main` whose commit message starts with
  `chore(release): prepare v`, or manual `workflow_dispatch`.
- **Actions**: extracts the version from the commit subject, verifies it
  matches `Cargo.toml`, extracts the matching `CHANGELOG.md` section as
  release notes, creates the GitHub Release with tag `vX.Y.Z`, then
  explicitly dispatches `publish.yml` and `cli-binaries.yml` against the
  new tag.
- **Why explicit dispatch**: a GitHub Release created with `GITHUB_TOKEN`
  does not fire `release: published` events (anti-recursion), so the
  downstream workflows must be kicked manually.

### `publish.yml`

- **Trigger**: `release: published`, or `workflow_dispatch --ref vX.Y.Z` from
  `release.yml`.
- **Actions**: installs the pinned Rust toolchain, verifies the tag matches
  `Cargo.toml`, publishes every publishable workspace crate in the
  dependency-first order `scripts/publish_order.py` derives (skipping
  versions already live), then runs
  `scripts/verify_crates_publish.py` to confirm crates.io serves the new version.
- **Secret**: `CARGO_REGISTRY_TOKEN`.

### `cli-binaries.yml`

- **Trigger**: `workflow_dispatch --ref vX.Y.Z` with the `tag` input, from
  `release.yml`.
- **Actions**: builds each bundled extension server for the three targets and
  uploads it, with its `.sha256`, to a per-extension release
  (`<crate>-v<version>`) that the job creates on first upload.
  `scripts/check_release_matrix.py` asserts that job's matrix expands to the
  same targets the CLI builds: a GitHub Actions matrix fails quietly, an
  `include` entry naming no existing dimension merges into every combination
  instead of creating one, and v0.17.0 shipped Linux-only extension servers
  that way with a green workflow. Then builds
  release binaries for the three CLI targets, packages them as
  `yolop-<target>.tar.gz`, uploads tarballs and `.sha256` files to the GitHub
  Release, and regenerates the Homebrew formula and pushes it to
  `everruns/homebrew-tap`. A separate `build-accelerated` job uploads the
  engine builds (`yolop-<target>-<backend>.tar.gz`) beside them. It is
  deliberately not in the formula job's `needs`: `cuda` is the least proven
  build in the release, and a failure there must not hold back the tap.
- **Secret**: `DOPPLER_TOKEN`. The Doppler config holds
  `HOMEBREW_TAP_GITHUB_TOKEN`, a fine-grained PAT scoped to
  `everruns/homebrew-tap` only.

## Pre-Release Checklist

The agent verifies before opening the release PR:

- [ ] All CI checks pass on `main`.
- [ ] `cargo fmt`, `cargo clippy`, `cargo test` clean.
- [ ] `CHANGELOG.md` has an entry for every commit since the last release.
- [ ] `Cargo.toml` and `Cargo.lock` both read `X.Y.Z`.
- [ ] Every workspace crate whose code changed since the last release carries a
      bumped version, and an extension's `plugin.json` matches its `Cargo.toml`.
- [ ] `cargo publish --dry-run` succeeds for each library crate (dependents of
      an unpublished `yolop-yep` are validated by CI once it is live, see
      § Agent Steps).
- [ ] `X.Y.Z` is greater than the latest crates.io version.
- [ ] Terminal verification current (see below) if the TUI renderer changed,
      Tier 1 green on the release commit, Tier 3 walked by a human.

Which end-to-end paths to smoke follows from what the release changed. Work out
that impact from the commits since the previous tag, then draw the paths from
[manual test scenarios](../test-scenarios/index.md), each one already states a
setup and acceptance criteria, so a release walks a known path instead of
improvising a new one per cut. A release whose impact no scenario covers is a
gap in the collection worth filling rather than a reason to skip the smoke.

## Terminal Verification

Terminal behavior is checked at three tiers. They are documented together
because the failure mode is reporting the whole matrix as "unverified" when two
of the tiers are already green: that invites a human to re-walk what a machine
gates on every PR, and it buries the rows nobody actually checked.

### Tier 1, asserted on every PR

The `Build + Tests` job in `ci.yml` runs both of these for any change touching
Rust, so they are green on the release commit before the release PR is cut.

| Check | What it proves |
|-------|----------------|
| `tests/tuika_pty.rs` | Drives the real binary under a PTY and asserts against a `vt100` reference terminal: alternate-screen enter/exit, OSC 9;4 progress set **and** cleared, OSC 8 hyperlinks present when enabled and absent when not, a 24-bit truecolor RGB cell in the grid, a Braille glyph in the grid, and survival across a resize. |
| tmux gallery capture | Runs `yolop tuika-gallery` inside real tmux at 120×40 under `TERM=tmux-256color`, gated by `scripts/assert-gallery.sh`: box chrome, a real Braille glyph, and the footer URL. This is an independent implementation *interpreting* the bytes, where `tuika_pty` parses them with a reference crate. |

A release PR cites the CI run for these rows. It does not list them as
unverified, and it does not ask a human to walk them.

### Tier 2, best-effort nightly

`.github/workflows/nightly-terminals.yml` drives GUI emulators on a schedule and
on `workflow_dispatch`. Being nightly, it can lag the commit being released,
dispatch it against the tag when a cut needs the evidence fresh.

| Leg | Runner | Capture | Status |
|-----|--------|---------|--------|
| kitty | Linux (Xvfb, software GL) | remote-control text + screenshot | Best-effort, captured as an artifact; assertion is a warning, not a failure. |
| iTerm2 | macOS | AppleScript session text + `screencapture` | Best-effort, artifact for inspection. |
| Windows Terminal | Windows | screenshot | Best-effort, artifact for inspection. |

Promote a leg to Tier 1 once its capture is proven stable on the runner. The
best-effort legs are `continue-on-error`, so a flaky GUI runner never reports the
nightly red on its own.

### Tier 3, the human walk

What no tier above reaches: how a specific GUI emulator *paints* the bytes. Walk
this before a release when the TUI renderer changed. It is a checklist, not a
record of results, tick a box only after confirming it yourself, and leave it
unticked rather than inferring it from a green CI run.

Run `cargo run -- tuika-gallery` in each terminal and check truecolor fidelity,
wide and Braille glyph shaping, mouse-wheel scroll, and, with
`YOLOP_HYPERLINKS=1`, that the footer URL is a clickable OSC 8 link with the
surrounding text, colors, and wrapping undamaged:

- [ ] Ghostty
- [ ] iTerm2
- [ ] WezTerm
- [ ] Kitty
- [ ] Windows Terminal
- [ ] Konsole

tmux is deliberately absent: Tier 1 gates it per PR. Walk it by hand only when
changing tmux-specific behavior, where truecolor needs `Tc`/`RGB` in
`terminal-overrides`.

### Terminal capability reference

**Native OSC 9;4 progress** support is a fixed property of each terminal (not
something to re-verify per release). Terminals that render it: **Ghostty** (bar
at the top of the window), **Windows Terminal** and **ConEmu** (taskbar),
**WezTerm**, **Konsole**, **mintty**. Others (e.g. **iTerm2**, **Kitty**)
silently ignore the unknown OSC, so emitting it is safe everywhere, the
in-terminal UI is unaffected.

**OSC 8 hyperlinks** (tuika's `HyperlinkBackend`)
wrap `http(s)` URL runs so a supporting terminal makes them clickable:
**Ghostty**, **iTerm2**, **WezTerm**, **Kitty**, **Konsole**, recent **GNOME
Terminal / VTE**. Others ignore the escape and render the URL as plain (usually
still auto-linkified) text, so emitting it is safe everywhere. Unlike OSC 9;4,
this one *is* worth re-checking, because it writes styled spans straight to the
terminal: confirm the link is clickable **and** that surrounding text, colors,
and wrapping are undamaged. In yolop it is opt-in (`YOLOP_HYPERLINKS=1`),
default-off until Tier 3 is walked, that is what the checkboxes above verify.

## Post-Release Verification

Run after both publish workflows finish. This is a required post-merge gate;
the release is not complete until crates.io serves `X.Y.Z` and the Homebrew
tap formula points at `vX.Y.Z`.

```bash
# crates.io
cargo search yolop --limit 1                       # shows X.Y.Z
cargo search yolop-yep --limit 1                   # shows the SDK version cut
cargo search yolop-extension- --limit 5            # shows each extension version cut

# Prebuilt extension servers (one release per extension version)
gh release view yolop-extension-logfire-vX.Y.Z --repo everruns/yolop

# GitHub Release
gh release view vX.Y.Z --repo everruns/yolop       # tarballs + checksums present

# Homebrew tap — the formula has no explicit `version`; Homebrew scans it
# from the release tag in the download URL, so verify that instead.
curl -sSfL https://raw.githubusercontent.com/everruns/homebrew-tap/main/Formula/yolop.rb \
  | grep -oE 'download/v[0-9][^/]*' | sed 's|download/||'   # shows vX.Y.Z

# End-to-end install (optional, on macOS / Linux)
brew untap everruns/tap 2>/dev/null; brew install everruns/tap/yolop
yolop --version
```

If any registry is missing the new version, inspect the corresponding
workflow run (`gh run view <run-id> --log-failed`) and either re-run
(transient) or open a hotfix PR (packaging bug).

## Changelog Format

Follow the everruns convention:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Highlights

- 2–5 bullet points summarizing the most impactful changes.

### Breaking Changes

- **Short description**: what changed, why, migration.
  - Before: `old_flag`
  - After: `new_flag`

### What's Changed

* feat(scope): description ([#42](https://github.com/everruns/yolop/pull/42)) by @contributor
* fix(scope): description ([#41](https://github.com/everruns/yolop/pull/41)) by @contributor

**Full Changelog**: https://github.com/everruns/yolop/compare/vA.B.C...vX.Y.Z
```

Rules:

- PRs listed newest-first by number.
- `### Breaking Changes` only when present; required for MINOR or MAJOR.
- `### Highlights` is the human summary; `### What's Changed` is the
  mechanical PR list.

## Hotfix Releases

For urgent fixes:

1. Ask agent: "Cut patch release vX.Y.Z+1 for the &lt;fix&gt;".
2. Agent branches from the latest tag, cherry-picks the fix, runs the same
   pre-release checklist, and opens the PR.
3. Human reviews and merges.

## Rollback

If a published version is broken, yank it:

```bash
cargo yank --version X.Y.Z yolop
```

Yanked versions remain usable by existing `Cargo.lock` files but are not
selected for new resolves. For Homebrew, push a follow-up commit to
`everruns/homebrew-tap` that reverts `Formula/yolop.rb` to the previous
release.

## Authentication

**Repo secrets** (Settings → Secrets and variables → Actions):

| Secret                  | Used by              | Source                                                  |
|-------------------------|----------------------|---------------------------------------------------------|
| `CARGO_REGISTRY_TOKEN`  | `publish.yml`        | https://crates.io/settings/tokens, publish scope       |
| `DOPPLER_TOKEN`         | `cli-binaries.yml`   | Doppler service token for the `release` config          |

**Doppler secrets** (loaded by `cli-binaries.yml` via `doppler secrets get`):

| Secret                       | Purpose                                                  |
|------------------------------|----------------------------------------------------------|
| `HOMEBREW_TAP_GITHUB_TOKEN`  | Fine-grained PAT scoped to `everruns/homebrew-tap` only. |

Scoping the tap PAT to the tap repo means a leak cannot touch the main
`yolop` repo.

## Related

- [`.agents/skills/release/SKILL.md`](../../.agents/skills/release/SKILL.md)
- [`knowledge/specs/shipping.md`](./shipping.md)
- [`knowledge/specs/maintenance.md`](./maintenance.md)

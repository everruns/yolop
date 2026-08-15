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
| crates.io       | `yolop` binary + the `yolop-yep` library  | `cargo install yolop --locked`          |
| Homebrew tap    | formula at `everruns/homebrew-tap`       | `brew install everruns/tap/yolop`       |

The `yolop-yep` extension SDK is versioned independently of `yolop` and is
published as a side effect of a `yolop` release only when its in-tree version
isn't already on crates.io (see § `publish.yml`). Extension authors consume it
on its own (`cargo add yolop-yep`).

The TUI toolkit yolop renders through — `tuika` and `tuika-codeformatters` — is
**not** released from here. It ships from
[`everruns/tuika`](https://github.com/everruns/tuika) on its own schedule, and a
yolop release simply depends on whatever version is already live. A yolop
release that needs new toolkit behavior therefore waits on a tuika release
first; see [`tuika.md`](./tuika.md).

Prebuilt CLI binaries are produced for:

| OS    | Target                       | Runner          |
|-------|------------------------------|-----------------|
| macOS | `aarch64-apple-darwin`       | `macos-latest`  |
| macOS | `x86_64-apple-darwin`        | `macos-latest`  |
| Linux | `x86_64-unknown-linux-gnu`   | `ubuntu-latest` |

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
3. **Squash and merge** — CI handles the GitHub Release, crates.io publish,
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

**Two crates, ordered publish.** `yolop` depends on `yolop-yep` by version, so
crates.io requires it live first. `publish.yml` derives the dependency-first
order from Cargo metadata (currently `yolop-yep`, then `yolop`) and skips
versions already live. A consequence: `cargo publish --dry-run -p yolop` can
fail locally until a new `yolop-yep` version is on crates.io. That is expected,
not a broken release — `yolop-yep` is dry-run locally and CI validates `yolop`
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
  `Cargo.toml`, publishes `yolop-yep` and `yolop` in dependency order
  (skipping versions already live), then runs
  `scripts/verify_crates_publish.py` to confirm crates.io serves the new version.
- **Secret**: `CARGO_REGISTRY_TOKEN`.

### `cli-binaries.yml`

- **Trigger**: `workflow_dispatch --ref vX.Y.Z` with the `tag` input, from
  `release.yml`.
- **Actions**: builds release binaries for the three CLI targets, packages
  them as `yolop-<target>.tar.gz`, uploads tarballs and `.sha256` files to
  the GitHub Release, then regenerates the Homebrew formula and pushes it
  to `everruns/homebrew-tap`.
- **Secret**: `DOPPLER_TOKEN`. The Doppler config holds
  `HOMEBREW_TAP_GITHUB_TOKEN`, a fine-grained PAT scoped to
  `everruns/homebrew-tap` only.

## Pre-Release Checklist

The agent verifies before opening the release PR:

- [ ] All CI checks pass on `main`.
- [ ] `cargo fmt`, `cargo clippy`, `cargo test` clean.
- [ ] `CHANGELOG.md` has an entry for every commit since the last release.
- [ ] `Cargo.toml` and `Cargo.lock` both read `X.Y.Z`.
- [ ] `cargo publish --dry-run` succeeds for each library crate (`-p yolop` is
      validated by CI once they are live — see § Agent Steps).
- [ ] `X.Y.Z` is greater than the latest crates.io version.
- [ ] Manual terminal matrix walked (see below) if the TUI renderer changed.

Which end-to-end paths to smoke follows from what the release changed. Work out
that impact from the commits since the previous tag, then draw the paths from
[manual test scenarios](../test-scenarios/index.md) — each one already states a
setup and acceptance criteria, so a release walks a known path instead of
improvising a new one per cut. A release whose impact no scenario covers is a
gap in the collection worth filling rather than a reason to skip the smoke.

## Manual Terminal Matrix

The automated tests cover the *protocol* the terminal renderer emits — the
`tests/tuika_pty.rs` PTY smoke drives the real binary and asserts alternate-screen
enter/exit, OSC 9;4 progress, OSC 8 hyperlinks, 24-bit truecolor SGR, and Braille
glyphs. What they cannot verify is how a specific emulator actually *paints* those
bytes. This is a manual checklist to walk before a release when the TUI renderer
changed, not a record of verified results — tick a box only after confirming it
yourself.

Run `cargo run -- tuika-gallery` in each terminal and check alt-screen
enter/exit, Braille/wide glyphs, truecolor, mouse-wheel scroll, and — with
`YOLOP_HYPERLINKS=1` — that the footer URL is a clickable OSC 8 link:

- [ ] Ghostty
- [ ] iTerm2
- [ ] WezTerm
- [ ] Kitty
- [ ] Windows Terminal
- [ ] Konsole
- [ ] tmux (truecolor needs `Tc`/`RGB` in `terminal-overrides`)

**Native OSC 9;4 progress** support is a fixed property of each terminal (not
something to re-verify per release). Terminals that render it: **Ghostty** (bar
at the top of the window), **Windows Terminal** and **ConEmu** (taskbar),
**WezTerm**, **Konsole**, **mintty**. Others (e.g. **iTerm2**, **Kitty**)
silently ignore the unknown OSC, so emitting it is safe everywhere — the
in-terminal UI is unaffected.

**OSC 8 hyperlinks** (tuika's `HyperlinkBackend`)
wrap `http(s)` URL runs so a supporting terminal makes them clickable:
**Ghostty**, **iTerm2**, **WezTerm**, **Kitty**, **Konsole**, recent **GNOME
Terminal / VTE**. Others ignore the escape and render the URL as plain (usually
still auto-linkified) text, so emitting it is safe everywhere. Unlike OSC 9;4,
this one *is* worth re-checking, because it writes styled spans straight to the
terminal: confirm the link is clickable **and** that surrounding text, colors,
and wrapping are undamaged. In yolop it is opt-in (`YOLOP_HYPERLINKS=1`),
default-off until this matrix is walked — that is what the checkbox above
verifies.

### Nightly cross-terminal job

`.github/workflows/nightly-terminals.yml` runs `yolop tuika-gallery` inside real
terminal emulators on a nightly schedule (and on `workflow_dispatch`), narrowing
how much of the matrix a human has to walk. In-repo tests already prove yolop
emits the right bytes (`tests/tuika_pty.rs` asserts the protocol and the parsed
vt100 grid); the nightly checks how emulators *interpret* those bytes. Legs
differ in maturity:

| Leg | Runner | Capture | Status |
|-----|--------|---------|--------|
| tmux | Linux | `capture-pane` text | **Asserted** — `scripts/nightly-assert-gallery.sh` gates the job on the box chrome, a real Braille glyph, and the footer URL. |
| kitty | Linux (Xvfb, software GL) | remote-control text + screenshot | Best-effort — captured as an artifact; assertion is a warning, not a failure. |
| iTerm2 | macOS | AppleScript session text + `screencapture` | Best-effort — artifact for inspection. |
| Windows Terminal | Windows | screenshot | Best-effort — artifact for inspection. |

A green **tmux** leg means the "alt-screen / Braille / layout / footer" rows are
already verified in a real emulator, so the manual walk reduces to the
per-emulator painting the best-effort legs only screenshot. Promote a best-effort
leg to asserting once its capture is proven stable on the runner. The best-effort
legs are `continue-on-error`, so a flaky GUI runner never reports the nightly red
on its own.

## Post-Release Verification

Run after both publish workflows finish. This is a required post-merge gate;
the release is not complete until crates.io serves `X.Y.Z` and the Homebrew
tap formula points at `vX.Y.Z`.

```bash
# crates.io
cargo search yolop --limit 1                       # shows X.Y.Z

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
| `CARGO_REGISTRY_TOKEN`  | `publish.yml`        | https://crates.io/settings/tokens — publish scope       |
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

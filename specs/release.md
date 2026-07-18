# Release Specification

## Abstract

This spec defines how yolop is cut, published, and verified. Releases are
agent-prepared, human-merged, and CI-published to two registries: crates.io
and the `everruns/homebrew-tap` Homebrew tap.

The canonical agent workflow lives in
[`.agents/skills/release/SKILL.md`](../.agents/skills/release/SKILL.md). That
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
| crates.io       | `yolop` binary + `yolop-yep` & `tuika` libs | `cargo install yolop --locked`       |
| Homebrew tap    | formula at `everruns/homebrew-tap`       | `brew install everruns/tap/yolop`       |

The two library crates (`yolop-yep`, `tuika`) are versioned independently of
`yolop` and are published as a side effect of a `yolop` release only when their
in-tree version isn't already on crates.io (see § `publish.yml`). They can also
be consumed on their own (`cargo add tuika`).

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

When asked to release, the agent:

0. **Ensure full git history.** Cloud sandboxes are often shallow-cloned,
   which silently hides commits and yields a wrong commit count or changelog.
   Run `git fetch --unshallow origin main 2>/dev/null || git fetch origin main`
   before counting or listing commits.

1. **Determine the version.** Use the version specified by the human, or
   propose the next version based on the unreleased commits (patch / minor /
   major) and confirm before proceeding.

2. **Update `CHANGELOG.md`.** Add a `## [X.Y.Z] - YYYY-MM-DD` section, list
   PRs in descending order with GitHub-style links and contributor handles,
   end with `**Full Changelog**: URL`. For minor/major bumps, add an explicit
   `### Breaking Changes` block with before/after migration snippets.

3. **Bump the version** in `Cargo.toml` and regenerate `Cargo.lock`
   (`cargo update -p yolop`). The library crates under `crates/` — the
   `yolop-yep` SDK and the `tuika` TUI toolkit — are **versioned separately**:
   bump one only when its own API changes (for `yolop-yep`, also the wire
   protocol), and when you do, update both `crates/<crate>/Cargo.toml` and the
   matching `<crate> = { version = … }` requirement in the root `Cargo.toml`.

4. **Run local verification:**
   - `cargo fmt --check`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo test`

5. **Verify publish-readiness** (catches what local tests don't — the
   `cargo publish` packaging step, missing files, version drift):
   - `cargo publish --dry-run -p yolop-yep` must succeed.
   - Confirm `Cargo.toml` and `Cargo.lock` agree on `X.Y.Z`.
   - Confirm `X.Y.Z` is greater than the latest published version on
     crates.io (`cargo search yolop --limit 1`).
   - If any check fails, fix the root cause and re-run before opening the
     PR. **Do not** merge a release PR with a known-broken publish path.

   **Three crates, ordered publish.** `yolop` depends on both `yolop-yep` and
   `tuika` by version, so crates.io requires both live first. `publish.yml`
   publishes `yolop-yep`, then `tuika`, then `yolop` (each skipped when already
   live). Because of that ordering, `cargo publish --dry-run -p yolop`
   **fails locally** — *"no matching package named `tuika` found"* (or
   `yolop-yep`) — until both in-tree library versions are on crates.io. That
   is expected, not a broken release; dry-run the two library crates and rely
   on CI to validate `yolop` after they go live.

6. **Commit and push** the changes on a feature branch with message
   `chore(release): prepare vX.Y.Z`.

7. **Open a PR** titled `chore(release): prepare vX.Y.Z`. Include the
   changelog excerpt and a **publish-readiness report** (which dry-runs ran,
   what the registry currently shows).

8. **Monitor and verify post-merge publishing.** After the human
   squash-merges the PR:
   - Watch `release.yml` complete and confirm tag `vX.Y.Z` + the GitHub
     Release were created.
   - Watch `publish.yml` and `cli-binaries.yml` to completion. Surface any
     failure immediately.
   - Run post-release verification (see below) yourself and report which
     targets show the new version. Workflow success is not enough: the agent
     must independently check crates.io and the Homebrew tap after merge.
   - Only declare the release **shipped** when crates.io reports `X.Y.Z` and
     the Homebrew tap formula points at `vX.Y.Z`. If one fails, open a hotfix
     PR rather than leaving the release half-published.

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
  `Cargo.toml`, runs `cargo publish -p yolop`, then runs
  `scripts/verify_crates_publish.py` to confirm crates.io serves the new
  version.
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
- [ ] `cargo publish --dry-run -p yolop` succeeds.
- [ ] `X.Y.Z` is greater than the latest crates.io version.

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

- [`.agents/skills/release/SKILL.md`](../.agents/skills/release/SKILL.md)
- [`specs/shipping.md`](./shipping.md)
- [`specs/maintenance.md`](./maintenance.md)

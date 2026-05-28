---
name: release
description: Cut a new yolop release. Prepares the release PR, runs publish-readiness checks, monitors CI publish to crates.io and the everruns Homebrew tap. Use when the user asks to release, cut a version, publish, or ship to crates.io / brew.
metadata:
  internal: true
user-invocable: true
---

# Release

Goal: cut a new yolop release and verify it lands on **both** crates.io and
the `everruns/homebrew-tap` Homebrew tap.

This skill implements [`specs/release.md`](../../../specs/release.md). Keep
operational guidance here. Keep design intent in the spec.

## When To Use

Use this skill when the user asks to:

- release / cut a release / publish vX.Y.Z
- ship to crates.io or Homebrew
- prepare a release PR or a hotfix release

For a generic "ship this change" request (PR → CI → merge of a non-release
change), use [`/ship`](../ship/SKILL.md) instead.

## Required Outcomes

**All outcomes below are MANDATORY.**

1. **The version is correct.** `Cargo.toml` and `Cargo.lock` agree on
   `X.Y.Z`. `X.Y.Z` is strictly greater than the latest version on crates.io.
2. **The changelog is honest.** `CHANGELOG.md` lists every commit landed
   since the previous tag, in descending order, with PR numbers and authors.
3. **Publish-readiness is proven before merge.** `cargo publish --dry-run -p
   yolop` succeeds. The PR body includes a publish-readiness report.
4. **CI publishes to both registries.** crates.io shows `X.Y.Z`. The
   Homebrew tap's `Formula/yolop.rb` is bumped to `X.Y.Z`. A "release" is
   not done until both are confirmed.
5. **A failure rolls forward, not backward.** If a publish fails, open a
   hotfix PR (or fix-forward in the same PR if not yet merged). Do not
   leave the release half-shipped.

## Operating Model

- Releases are agent-prepared, human-merged, CI-published.
- Start by gathering the unreleased commit set, not by guessing the version.
- The agent never tags the release directly — `release.yml` does that when
  the `chore(release): prepare vX.Y.Z` commit lands on `main`.
- The agent never pushes to `everruns/homebrew-tap` directly —
  `cli-binaries.yml` does that.

## Step-By-Step

### 0. Sync local state

Shallow clones lie. Before counting commits, force a full history:

```bash
git fetch --unshallow origin main 2>/dev/null || git fetch origin main
git fetch --tags
```

Cross-check the commit count if anything looks off:

```bash
LATEST=$(git describe --tags --abbrev=0)
git log "$LATEST"..origin/main --oneline | wc -l
gh api "repos/everruns/yolop/compare/$LATEST...main" --jq '.total_commits'
```

If those disagree, the clone is still shallow.

### 1. Pick the version

If the user gave a version, use it. Otherwise, propose based on the diff:

- only `fix`, `docs`, `chore`, `refactor`, `test` commits → patch
- one or more `feat` commits → minor
- breaking changes flagged in commit bodies → major (or minor pre-1.0 with
  an explicit `### Breaking Changes` block)

Confirm with the user before proceeding.

### 2. Update the changelog

`CHANGELOG.md` lives at the repo root. Add a section at the top under any
intro:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Highlights

- 2–5 bullets summarizing the most user-visible changes.

### Breaking Changes

- (only when applicable; required for MINOR / MAJOR with breakage)

### What's Changed

* feat(scope): description ([#42](https://github.com/everruns/yolop/pull/42)) by @contributor
* fix(scope): description ([#41](https://github.com/everruns/yolop/pull/41)) by @contributor

**Full Changelog**: https://github.com/everruns/yolop/compare/vA.B.C...vX.Y.Z
```

Build the PR list mechanically:

```bash
git log "$LATEST"..HEAD --pretty=format:'%s' --reverse \
  | grep -v '^chore(release): prepare v'
```

Map commits to PRs via `gh pr list --state merged --base main --limit 200`
when commit subjects don't carry the PR number.

### 3. Bump the version

Edit `Cargo.toml`:

```toml
[package]
name = "yolop"
version = "X.Y.Z"
```

Refresh the lockfile entry:

```bash
cargo update -p yolop
```

### 4. Run local verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

### 5. Verify publish-readiness

This is the step that catches what local tests don't — the `cargo publish`
packaging boundary, missing files referenced by `Cargo.toml`, version drift:

```bash
cargo publish --dry-run -p yolop
cargo search yolop --limit 1     # confirm CURRENT crates.io version < X.Y.Z
grep '^version' Cargo.toml       # confirm reads X.Y.Z
grep '"yolop"' Cargo.lock | head -1  # confirm reads X.Y.Z
```

If `cargo publish --dry-run` fails, fix the root cause and re-run. Do **not**
open a release PR with a known-broken publish path.

### 6. Commit and push

Stage explicitly — never `git add .`:

```bash
git add CHANGELOG.md Cargo.toml Cargo.lock
git commit -m "chore(release): prepare vX.Y.Z"
git push -u origin "$(git branch --show-current)"
```

### 7. Open the PR

Title: `chore(release): prepare vX.Y.Z` (under 70 chars).

Body must include:

- The full `## [X.Y.Z] - …` changelog section.
- A **Publish-readiness** block:

  ```markdown
  ## Publish-readiness

  - [x] `cargo fmt --check`
  - [x] `cargo clippy --all-targets --all-features -- -D warnings`
  - [x] `cargo test --all-features`
  - [x] `cargo publish --dry-run -p yolop`
  - [x] crates.io currently serves `A.B.C` → publishing `X.Y.Z`
  - [x] `Cargo.toml` + `Cargo.lock` agree on `X.Y.Z`
  ```

- A **Post-merge plan** noting that `release.yml` will tag + dispatch
  `publish.yml` and `cli-binaries.yml`.

### 8. Monitor publishing after merge

Subscribe to PR activity for the release PR so the loop wakes you on each
workflow completion. Then watch:

```bash
gh run list --workflow=release.yml      --limit 1
gh run list --workflow=publish.yml      --limit 1
gh run list --workflow=cli-binaries.yml --limit 1
```

Confirm each finishes green, then run the post-release checks:

```bash
# crates.io
cargo search yolop --limit 1   # shows X.Y.Z

# GitHub Release
gh release view "vX.Y.Z"       # tag + 3 tarballs + 3 sha256 files

# Homebrew tap
curl -sSfL https://raw.githubusercontent.com/everruns/homebrew-tap/main/Formula/yolop.rb \
  | grep '^  version "'        # shows X.Y.Z
```

Declare **shipped** only when crates.io and the Homebrew tap both report
`X.Y.Z`. If a workflow fails, inspect logs (`gh run view <id> --log-failed`)
and either re-run (transient — network / registry propagation) or open a
hotfix PR (packaging bug — see `specs/release.md` § Hotfix Releases).

## Common Pitfalls

- **Shallow clone.** Cloud sandboxes default to depth ≈ 50 and silently
  drop older commits from `git log`. Always `git fetch --unshallow` first.
- **Tag/Cargo drift.** A v0.4.0 → v0.4.1-style hotfix is almost always
  caused by version drift between `Cargo.toml` and what `cargo publish`
  actually sees. The dry-run catches it.
- **Half-shipped releases.** crates.io publishes near-instantly; Homebrew
  takes minutes (build + tap commit). Don't declare shipped until the tap
  formula reflects the new version.
- **Auto-merge.** Do not enable auto-merge on the release PR. A human must
  click the squash button so a real reviewer sees the changelog.

## Authentication

Required repo secrets — set up once, see [`specs/release.md`](../../../specs/release.md)
§ Authentication for the full table.

- `CARGO_REGISTRY_TOKEN` — crates.io publish scope.
- `DOPPLER_TOKEN` — service token; the Doppler config holds
  `HOMEBREW_TAP_GITHUB_TOKEN`, a fine-grained PAT scoped to the tap repo
  only.

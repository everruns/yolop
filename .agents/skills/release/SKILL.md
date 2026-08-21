---
name: release
description: Cut a new yolop release. Prepares the release PR, runs publish-readiness checks, monitors CI publish to crates.io and the everruns Homebrew tap. Use when the user asks to release, cut a version, publish, or ship to crates.io / brew.
metadata:
  internal: true
user-invocable: true
---

# Release

Goal: cut a new yolop release and verify it lands on **both** crates.io and the
`everruns/homebrew-tap` Homebrew tap.

[`knowledge/specs/release.md`](../../../knowledge/specs/release.md) owns the versioning rules, the changelog
format, the CI automation contract, post-release verification, hotfix and
rollback policy, and the secrets table. This skill owns the procedure. For a
non-release change, use [`/ship`](../ship/SKILL.md) instead.

Releases are agent-prepared, human-merged, CI-published. The agent never tags
the release and never pushes to the tap — `release.yml` and `cli-binaries.yml`
do that.

## 0. Sync local state

Shallow clones lie: cloud sandboxes default to depth ≈ 50 and silently drop
older commits from `git log`.

```bash
git fetch --unshallow origin main 2>/dev/null || git fetch origin main
git fetch --tags

LATEST=$(git describe --tags --abbrev=0)
git log "$LATEST"..origin/main --oneline | wc -l
gh api "repos/everruns/yolop/compare/$LATEST...main" --jq '.total_commits'
```

If those two counts disagree, the clone is still shallow.

## 1. Pick the version

Use the version the user gave. Otherwise propose one from the unreleased commit
set under the spec's versioning rules, and confirm before proceeding.

## 2. Update the changelog

Build the commit list mechanically, then write the section in the format the
spec defines (§ Changelog Format):

```bash
git log "$LATEST"..HEAD --pretty=format:'%s' --reverse \
  | grep -v '^chore(release): prepare v'
```

Map commits to PRs with `gh pr list --state merged --base main --limit 200`
when the subjects don't carry the number.

## 3. Bump the version

Edit `version` in `Cargo.toml`, then `cargo update -p yolop` to refresh the
lockfile entry.

`yolop-yep` is versioned separately. Bump it when its API or the wire protocol
changes, and update every workspace path dependency requirement that references
it.

First-party extensions under `extensions/` are versioned separately too. Bump
one whose code changed since the last release, in both its `Cargo.toml` and its
`plugin.json`, or it is published nowhere and its manifest keeps pointing at the
previous tag's binaries. `python3 scripts/publish_order.py` lists exactly what
the release publishes:

```bash
python3 scripts/publish_order.py
git log "$LATEST"..HEAD --oneline -- extensions/ crates/
```

`tuika` and `tuika-codeformatters` are not released from here — they ship from
[`everruns/tuika`](https://github.com/everruns/tuika). Bumping their version
requirements is an ordinary dependency bump, and a release that needs new
toolkit behavior waits on a tuika release first.

## 4. Verify locally

Review the commits since the previous tag for durable knowledge impact and
update the affected concepts, `knowledge/index.md`, and `knowledge/log.md`.
Confirm `README.md` and `docs/` describe the released behavior. Then run the
[checks in `AGENTS.md`](../../../AGENTS.md).

## 5. Verify publish-readiness

This is the step local tests can't stand in for — it exercises the `cargo
publish` packaging boundary, missing files referenced by `Cargo.toml`, and
version drift.

```bash
cargo publish --dry-run -p yolop-yep   # SDK; publishes before everything else
cargo search yolop --limit 1           # crates.io version must be < X.Y.Z
grep '^version' Cargo.toml
grep '"yolop"' Cargo.lock | head -1
```

`cargo publish --dry-run` **fails locally** for `yolop` and for any extension
whenever a new `yolop-yep` version isn't on crates.io yet — expected, not a
broken release. CI validates those publishes after the SDK goes live. If the *SDK* dry-run fails, fix
the root cause and re-run; never open a release PR with a known-broken publish
path.

## 6. Commit, push, open the PR

```bash
git add CHANGELOG.md Cargo.toml Cargo.lock
git commit -m "chore(release): prepare vX.Y.Z"
git push -u origin "$(git branch --show-current)"
```

Title the PR `chore(release): prepare vX.Y.Z`. The body carries the full
changelog section, a **Publish-readiness** report (which dry-runs ran, what
crates.io currently serves, that `Cargo.toml` and `Cargo.lock` agree), and an
unchecked **Post-merge verification** list covering `release.yml`, `publish.yml`,
`cli-binaries.yml`, crates.io, and the tap formula.

Do not enable auto-merge: a human must click squash so a real reviewer reads the
changelog.

## 7. Monitor after merge

Subscribe to PR activity so workflow completions wake you, then watch:

```bash
gh run list --workflow=release.yml      --limit 1
gh run list --workflow=publish.yml      --limit 1
gh run list --workflow=cli-binaries.yml --limit 1
```

Green workflows are not proof. Run the spec's post-release verification yourself
and declare **shipped** only when crates.io reports `X.Y.Z`, every bumped
library and extension crate is live, each bumped extension has its
`<crate>-v<version>` release carrying the three server archives, and the tap
formula points at `vX.Y.Z`. crates.io publishes near-instantly; Homebrew takes minutes
for the build and tap commit, so a half-verified release looks shipped when it
isn't.

On failure, read the logs (`gh run view <id> --log-failed`) and either re-run
(transient — network or registry propagation) or roll forward with a hotfix PR
per the spec. Never leave a release half-shipped.

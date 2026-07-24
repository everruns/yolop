---
type: Product Specification
title: Git worktrees
description: Defines the git worktrees contract for Yolop.
---

# Git worktrees

Status: v1 implemented.

## Why

Coding sessions that change the repository should not move the user's primary
checkout off `main` or mix unrelated edits into their working tree. Git
worktrees give each yolop session an isolated directory and branch while the
main checkout stays untouched.

## What

### Modes (`worktrees` in `settings.toml`)

| Value | Behavior |
|-------|----------|
| `auto` (default) | Provision a worktree when a user prompt looks like implementation work |
| `always` | Provision at session start inside git repositories |
| `off` | Never create worktrees; operate in the resolved cwd |

### Layout

- Worktrees live **outside** the repository: `$TMPDIR/yolop/worktrees/<repo-id>/<session-id>/`, falling back to `~/.yolop/worktrees/...` when tmp is unavailable.
- Branches are named `<slug>-<id>` (e.g. `fix-auth-a1b2c3d4`), branched from `origin/main` when available.
- Session metadata in `workspace.json` records `repo_root`, `active_root`, and worktree fields for resume.

### Ignored local files (`.worktreeinclude`)

`git worktree add` only materializes tracked files, so intentionally-ignored
setup files (`.env`, local secrets, …) are missing from a fresh worktree. A
`.worktreeinclude` file at the repo root lists, using `.gitignore`-style
patterns, which **git-ignored** paths to copy into newly-created worktrees.

```
# .worktreeinclude
.env
.env.local
config/secrets.json
```

- Only ignored files matching a pattern are copied; tracked files and other
  untracked-but-not-ignored files are left alone.
- An ignored `AGENTS.override.md` is copied automatically without being listed.
- Symlinks are skipped, and files already present in the worktree are never
  overwritten.
- Copying is best-effort: per-file failures are logged (`RUST_LOG`) and never
  block worktree activation. The same copy runs when a worktree is recreated on
  resume.

### Runtime behavior

- `active_root` is the effective cwd for file tools, bash, repo scans, and environment context.
- `repo_root` is the git toplevel of the user's checkout; git identity/remote context comes from there.
- Sub-agents inherit the parent session's active worktree.
- Resume reattaches to a saved worktree or recreates it when tmp was cleared.
- The TUI compact status bar shows `wt <slug>` when a worktree is active; expand
  the status bar for branch and path. Mid-session activation posts a light
  `Switched to worktree · <branch>` system line instead of the raw path.

### Commands

- `/worktree` — show active worktree, branch, and path (or mode when inactive)
- `/worktree off` — disable auto-activation for future turns in this session
- `yolop worktree list` — list worktree directories on disk
- `yolop worktree prune` — remove worktrees not referenced by any saved session (`--dry-run` to preview)


Harness and `<environment_context>` tell the model to edit and commit only in
the session worktree and never change git state in `repo_root`.

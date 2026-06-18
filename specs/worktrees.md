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

### Runtime behavior

- `active_root` is the effective cwd for file tools, bash, repo scans, and environment context.
- `repo_root` is the git toplevel of the user's checkout; git identity/remote context comes from there.
- Sub-agents inherit the parent session's active worktree.
- Resume reattaches to a saved worktree or recreates it when tmp was cleared.

### Agent guidance

Harness and `<environment_context>` tell the model to edit and commit only in
the session worktree and never change git state in `repo_root`.

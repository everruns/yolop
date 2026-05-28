# Yolop — coding-agent guidance

Yolop is a minimal terminal coding agent built on top of
[`everruns-runtime`](https://crates.io/crates/everruns-runtime). The binary is
named `yolop`; the crate is `yolop`.

This file is read on every turn by the agent itself when run inside this
repository, so keep it short, factual, and project-specific.

## Workflow

- Telegraph. Drop filler. Keep updates short and factual.
- Fix the root cause. If unsure, read more code; if still stuck, ask with short options.
- Unrecognized working-tree changes are probably from another agent or the user. Work with them. Stop only if they make the task unsafe.
- Start from latest `main` by default: `git fetch origin main`, then branch from or rebase onto `origin/main`.
- Keep changes small, PR-sized, testable, and runnable locally.
- For bug fixes, write or update a failing test before the fix when practical.
- Important decisions belong as concise comments near the relevant code, not in scratch docs.
- No backward compatibility required unless a spec says so — `yolop` is pre-1.0.

## Cloud and secrets

Use Doppler for any secret-backed command. `OPENAI_API_KEY` is the default
provider key; `ANTHROPIC_API_KEY` is the secondary. CI loads both via the
`DOPPLER_TOKEN` repository secret.

```bash
doppler run -- cargo test --all-features
doppler run -- cargo run -- -p "say hi"
```

If GitHub auth fails, do not tell the user the token expired. Try:

```bash
doppler run -- bash -lc 'GH_TOKEN="$GITHUB_TOKEN" <command>'
```

## Specs and docs

- `specs/` contains durable feature specifications. Read the relevant spec
  before changing behavior in that area.
- `README.md` is the user-facing entry point — keep it current.
- Specs capture **why** and **what**, not exhaustive **how**. Do not duplicate
  fields, enum variants, or exact tool schemas already in source.

## Local dev and tests

Yolop is a small Cargo project. For touched code:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Quick offline smoke test (no API key needed — uses the bundled `llmsim`
provider):

```bash
cargo run -- --provider llmsim -p "hi"
```

Real provider smoke test through Doppler:

```bash
doppler run -- cargo run -- --provider openai -p "hi"
```

`RUST_LOG` is honored for the tracing layer (stderr).

## Git and commits

- Conventional Commits: `type(scope): description`.
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
- Use `chore` for updates to `specs/`, `AGENTS.md`, or CI metadata.
- Never add Claude/session/AI attribution links in commits, PRs, docs, or code comments.
- Stage files explicitly by name. Avoid broad `git add .` / `git add -A`.

Commit attribution must be a real human user. If git identity is missing or
agent-like, stop and ask before committing.

## PRs and CI

- PR titles must be Conventional Commits and under 70 characters.
- Use **Squash and Merge**.
- GitHub Actions is the CI source of truth.
- Never merge red CI.
- Before merge, prefer rebasing onto latest `origin/main`.

## Shipping, maintenance, and releases

- "Ship" means implement, gather evidence, perform a security review, open a
  mergeable PR, address every review comment, and merge only after CI is green.
- When asked to ship, follow [`.agents/skills/ship/SKILL.md`](.agents/skills/ship/SKILL.md)
  and [`specs/shipping.md`](specs/shipping.md).
- When asked for maintenance or release readiness, follow
  [`.agents/skills/maintenance/SKILL.md`](.agents/skills/maintenance/SKILL.md)
  and [`specs/maintenance.md`](specs/maintenance.md).
- When asked to release, cut a version, or publish to crates.io / Homebrew,
  follow [`.agents/skills/release/SKILL.md`](.agents/skills/release/SKILL.md)
  and [`specs/release.md`](specs/release.md). Releases publish to both
  crates.io and the `everruns/homebrew-tap` Homebrew tap.

## Upstream relationship

Yolop is a friendly fork / promotion of the `examples/coding-cli` example in
[`everruns/everruns`](https://github.com/everruns/everruns). When the upstream
example changes meaningfully, mirror the useful parts here. Keep public
runtime crate versions in lockstep with what is published on crates.io.

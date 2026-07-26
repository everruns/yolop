# Yolop — coding-agent guidance

Yolop is a terminal coding agent built on
[`everruns-runtime`](https://crates.io/crates/everruns-runtime). The binary and
the crate are both named `yolop`.

This file is read on every turn. It carries repository facts and gotchas only;
depth lives behind the links below. [`knowledge/specs/agent-context.md`](knowledge/specs/agent-context.md)
defines how this repository organizes what agents read.

Telegraph — keep updates short and factual. Fix root causes; when you are still
stuck after reading the code, ask with short options.

## Layout

A Cargo workspace of two packages; root `cargo test` / `cargo clippy` cover
both.

- `.` — the `yolop` binary.
- `crates/yolop-yep/` — the YEP extension protocol and server SDK, published
  separately for extension authors; the host depends on it for the wire types.

Both renderers are built on [tuika](https://github.com/everruns/tuika), a
terminal-UI toolkit that lives in its own repository: the default renderer is
its alternate-screen mode, and `--inline` its split-footer mode. The three
tuika crates currently track that repository's `main` branch, because the
split-footer mode is unreleased; they move back to crates.io versions together
once it ships. See [Tuika](#tuika).

## Gotchas

- Secrets come from Doppler. `OPENAI_API_KEY` is the default provider key,
  `ANTHROPIC_API_KEY` the secondary; CI loads both from the `DOPPLER_TOKEN`
  repository secret.

  ```bash
  doppler run -- cargo test --all-features
  doppler run -- cargo run -- --provider openai -p "hi"
  ```

- Try `gh` directly first. Only if it reports that it is not authenticated,
  retry through Doppler — do not reach for Doppler preemptively:
  `doppler run -- bash -lc 'GH_TOKEN="$GITHUB_TOKEN" <command>'`.
- `--provider llmsim` needs no API key, so `cargo run -- --provider llmsim -p "hi"`
  is the offline smoke test.
- Yolop is pre-1.0: no backward compatibility is required unless a spec says so.
- Unrecognized working-tree changes are probably from another agent or the user.
  Work with them; stop only if they make the task unsafe.
- Decisions worth keeping belong as concise comments near the relevant code, not
  in scratch documents.
- For bug fixes, prefer writing the failing test before the fix.

`RUST_LOG` is honored for the tracing layer (stderr).

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
python3 scripts/validate_okf.py knowledge --check-links   # when knowledge/ changed
```

## Where things live

- [`knowledge/`](knowledge/index.md) — the OKF bundle and durable development
  memory: intent, architecture, policy, constraints, and the success bars for
  shipping, maintenance, and release. Read the index first, then only the
  concepts the task touches.
- [`.agents/skills/`](.agents/skills) — workflows the user can request by name:
  `/ship`, `/maintenance`, `/release`, `/author-extension`.
- [`evals/README.md`](evals/README.md) — the Mira eval studies (SWE-bench
  Verified, harness A/Bs, LSP isolation). Outside the Cargo workspace.
- [`README.md`](README.md) and [`docs/`](docs/) — the public surface. Neither may
  link into `knowledge/` or `.agents/`; see
  [`knowledge/specs/documentation.md`](knowledge/specs/documentation.md).

## Keeping knowledge current

When a change alters durable behavior, intent, architecture, policy,
constraints, terminology, or maintainer process, update the affected concepts in
the same change. Update `knowledge/index.md` when concepts are added, removed,
renamed, or reclassified, and `knowledge/log.md` for significant knowledge
changes. Transient plans, task status, and source-level detail stay out of the
bundle.

## Commits

- Conventional Commits: `type(scope): description`, using `feat`, `fix`, `docs`,
  `refactor`, `test`, or `chore`. `chore` covers `knowledge/`, `AGENTS.md`, and
  CI metadata.
- Stage files explicitly by name. Avoid `git add .` / `git add -A`.
- Never add Claude/session/AI attribution to commits, PRs, docs, or comments.
- Commit attribution must be a real human user. If the git identity is missing
  or agent-like, stop and ask before committing.

Start from latest `main` by default: `git fetch origin main`, then branch from or
rebase onto `origin/main`. The merge bar (PR template, CI, squash) is owned by
[`knowledge/specs/shipping.md`](knowledge/specs/shipping.md).

## Tuika

Toolkit-shaped work — layout, components, overlays, focus, keymap, markdown
rendering, terminal escapes, screen modes — belongs in
[everruns/tuika](https://github.com/everruns/tuika), not here; what belongs here
is how yolop *composes* it. Land a needed toolkit change there, then bump the
version — or, while it is unreleased, the pinned revision. `tests/tuika_pty.rs`
stays because it drives the `yolop` binary. See [`knowledge/specs/tuika.md`](knowledge/specs/tuika.md).

## Upstream relationship

Yolop is a friendly fork of the `examples/coding-cli` example in
[`everruns/everruns`](https://github.com/everruns/everruns). Mirror meaningful
upstream changes, and keep the public runtime crate versions in lockstep with
what is published on crates.io.

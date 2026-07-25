# Knowledge Log

Significant changes to Yolop's durable knowledge are recorded here. Routine
wording, formatting, and link fixes do not need entries.

## 2026-07-24 — Tuika moved to its own repository

- `tuika` and `tuika-codeformatters` left this workspace for
  [everruns/tuika](https://github.com/everruns/tuika) and are now consumed from
  crates.io. Yolop publishes two crates again (`yolop-yep`, `yolop`).
- Replaced the two tuika-internal concepts (keymap engine, image rendering) with
  [Tuika](specs/tuika.md), which records the dependency boundary, the seams Yolop
  fills, and the testing split. The toolkit's own design rationale now lives in
  its repository.

## 2026-07-24 — Agent context layering

- Added [Agent context](specs/agent-context.md): one owner per rule, progressive
  disclosure across `AGENTS.md` → specs → skills → code, and constraint reserved
  for irreversible actions.
- Applied it: `AGENTS.md` now carries repository facts and gotchas only, with
  benchmark procedure moved to the toolkit's own repository and eval
  procedure left to `evals/README.md`; the ship, maintenance, and release skills
  stopped restating their specs' bars, and maintenance and release split their
  reference material out of `SKILL.md`.
- Resolved two conflicting constraints: the ship skill no longer both mandates a
  checklist and tells the agent not to walk one, and the release pre-release
  checklist no longer requires a `-p yolop` dry-run that is expected to fail
  locally.

## 2026-07-24

- Added [System prompt composition](specs/system-prompt.md): one fact has one
  home, discovery is always-on while how-to is reveal-gated, judgement is
  preferred over encoded rules unless an eval says otherwise, and the per-turn
  budget is enforced by a test.
- Reveal gating (`capabilities::tool_reveal`) lets a capability withhold its
  how-to prose until `tool_search` has loaded one of its tools; `config` and
  `memory` use it, and `memory` now discloses titles separately from framing.
- Corrected the release contract to publish all four workspace crates in dependency order, including `tuika-codeformatters` before `yolop`.
- Added [Crash reporting](specs/crash-reporting.md): bounded, owner-only local
  panic reports that remain visible after TUI terminal restoration.
- Crash reports and restored-terminal diagnostics now identify the active
  single-session runtime when its session ID is available.
- Added `specs/keymap.md`: the tuika declarative key-binding engine and Yolop's dispatch of its global shortcuts through it. (Superseded — the engine's rationale moved to the tuika repository; see [Tuika](specs/tuika.md).)
- Excluded the tuika demo/theme/styling GIFs from the published `.crate` (~8.8 MiB → ~1 MiB), keeping only the two README-embedded assets; documented the GitHub-only/crate split in the documentation spec.

## 2026-07-23

- Established `knowledge/` as Yolop's OKF bundle.
- Migrated product specifications to `knowledge/specs/` and added typed OKF metadata.
- Added repository, shipping, maintenance, release, and CI rules to keep the bundle current.

## 2026-07-24 — Fullscreen renderer became the default

- Made the alternate-screen fullscreen renderer the default for interactive sessions.
- Added `--inline` as the explicit opt-out for terminal-native scrollback.

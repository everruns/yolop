# Knowledge Log

Significant changes to Yolop's durable knowledge are recorded here. Routine
wording, formatting, and link fixes do not need entries.

## 2026-07-27 — Herdr identifies concurrent Yolop sessions

- [Herdr integration](specs/herdr.md) now forwards Yolop session titles to pane
  metadata and uses them in display-agent labels, with a stable session suffix
  before title generation, so concurrent Yolop agents remain distinguishable.
  Lifecycle states also gain human-readable labels while retaining the `yolop`
  machine identity for grouping and ownership.

## 2026-07-27 — First-party extension placement

- First-party extension packages now live under `extensions/`, reserving
  `crates/` for core libraries and workspace packages. Rust extension releases
  package their manifest and README so crates.io installation can provision the
  extension package. See [Extension system](specs/extensions.md).

## 2026-07-26 — OKF skill tracks the authoritative v0.2 spec

- The bundled `okf` skill was rewritten against `SPEC.md` in
  `GoogleCloudPlatform/knowledge-catalog`, now named as the only authoritative
  source; the `okf.md` site it previously cited is not normative and described a
  superseded v0.1 model. [`okf`](specs/okf.md) gained the v0.2 families the skill
  must teach — `sources` with credibility signals, `generated`/`verified` with
  the actor convention and trust tiers, `status`/`stale_after`, and the
  `Attested Computation` contract — plus the rule that validator lint stays
  behind `--strict` so a permissive format is not taught as a strict one, and the
  requirement that the skill's validator copy stay byte-identical to the one CI
  runs.

## 2026-07-26 — The scrollback renderer is tuika's split-footer mode

- `--inline` now composes tuika's `ScreenMode::SplitFooter` instead of yolop's
  own inline-viewport anchoring and `insert_before` publishing.
  [Tuika](specs/tuika.md) gained the screen-mode boundary — the toolkit owns
  pinning, publishing, and footer teardown; yolop owns which lines get
  published — and [Presentation](specs/presentation.md) states what the mode
  guarantees a user.
- A transcript entry now appears exactly once: the footer paints only what is
  not yet published, and publishing holds back the rows the footer still shows,
  cutting an entry in half when one straddles the edge. Everything is published
  at exit. [Presentation](specs/presentation.md) carries the rule.
- The dependency rule in [Tuika](specs/tuika.md) now says *why* the crates.io
  pin is a constraint rather than a preference: `cargo publish` rejects a
  dependency without a version requirement, so a git dependency would leave
  yolop unreleasable for as long as it stayed.

## 2026-07-25 — Host rendering advertised to the model

- `<environment_context>` now carries `ui_capabilities` beside `client_ui` — an
  additive list of what the host renders — so the model can decide whether a
  diagram is worth drawing.
  [System prompt composition](specs/system-prompt.md) gained the rule these
  fields follow: state the host's capability rather than an instruction, and
  keep the fields static per host so a mid-session change cannot churn the
  cached prefix.

## 2026-07-25 — Mermaid fences in the transcript

- Yolop fills tuika's `FencedBlockRenderer` seam with `tuika-mermaid`, so
  ` ```mermaid ` fences render as Unicode diagrams. [Tuika](specs/tuika.md)
  gained the third companion crate and the rule that a fenced block which does
  not fit the transcript width falls back to the themed code block rather than
  being painted clipped.

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

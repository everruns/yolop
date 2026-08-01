# Knowledge Log

Significant changes to Yolop's durable knowledge are recorded here. Routine
wording, formatting, and link fixes do not need entries.

## 2026-08-01 — Tuika structured-markdown renderers

- [Tuika](specs/tuika.md) now records the HTML companion renderer and the
  current structured-markdown renderer API alongside Mermaid diagrams.

## 2026-08-01 — Conversational skills.sh search and install

- [Skills](specs/skills.md) and [Conversational control](specs/conversational-control.md)
  now cover `search_skills` / `install_skill`: query the public skills.sh
  registry, ask which match to install, and write the snapshot into workspace
  or global scope without restarting.

## 2026-08-01 — Owner evidence before non-obvious mutation

- [System prompt composition](specs/system-prompt.md) now requires repository
  evidence for the root cause and owning abstraction before the first mutation
  of a non-obvious bug, while preserving the one-read path for explicit local
  edits.

## 2026-07-31 — Dependency-aware tool batching

- Yolop now requests provider parallel tool calling and carries a measured
  dependency-aware batching rule: independent calls share a model round,
  dependent calls remain sequential, and title/todo/status bookkeeping
  piggybacks on substantive work. The focused gpt-5.5 A/B kept 9/9 tasks correct
  while reducing mean model calls by 27% and cumulative input including cache
  reads by 26%; its dependent-read control never co-batched a call with the
  result it needed.
- Title and todo handling remains in runtime tools rather than a deterministic
  host side channel, preserving event replay and presentation ownership.

## 2026-07-31 — Discovery reuse and useful session admission

- Repeated large read/search results on an unchanged target now return a compact
  freshness marker, with reuse invalidated by workspace mutation; the first full
  result remains available in context.
- Local session discovery counts only logs with user-visible messages or a
  recorded model failure toward its 500-session scan budget, so interrupted
  empty/invalid shells cannot crowd useful overlapping work out.

## 2026-07-31 — Responsive activity rail and session-scoped transcripts

- The interactive TUI now opens a passive right-hand activity rail when
  sub-agents appear. Its flat Yolop-native chrome groups agents separately from
  background commands and waiting monitors; overflow scrolls and follows new
  work, while narrow focused rails become visible drawers instead of trapping
  focus off-screen. Root transcript catch-up filters child-session events just
  like live delivery, preventing child title and tool narration from leaking
  into the coordinator's transcript.

## 2026-07-31 — Local sub-agents enabled

- Yolop now drives linked child sessions through its local platform runner and
  enables `spawn_agent` by default. A bounded two-level hierarchy can contain 20
  active sub-agents, while each session retains the upstream five-background-run
  ceiling. The `Ctrl+B` activity rail presents the hierarchy and branch usage.

## 2026-07-31 — Named configuration profiles

- [Configuration](specs/configuration.md) now defines explicit `--profile`
  selection, sparse execution overlays above global settings, global-only
  credential and structural keys, active-layer persistence, and profile
  visibility. [ACP](specs/acp.md) records profile defaults below standard live
  model selection, while [Presentation](specs/presentation.md) makes the active
  profile part of safety status.
## 2026-07-30 — ACP model selection uses standard session configuration

- [ACP integration](specs/acp.md) now exposes model and reasoning-effort choices
  through standard `configOptions` and applies changes through
  `session/set_config_option`. The earlier private `yolop.dev/acp`
  `selectedModel` metadata contract was removed because editor clients cannot
  discover or use private selection protocols without bespoke integration.

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

## 2026-07-30 — Terminal-Bench 2.1 eval study

- Added `evals/terminal_bench/`: yolop on Terminal-Bench 2.1 (89 containerized
  tasks), stacking the Mira host over Harbor — Mira owns the matrix and
  reporting, Harbor owns the container, the agent run, and the task's verifier.
- Realized the Harbor half of [Trajectory export](specs/trajectory.md): the
  study's Harbor agent adapter hands `--trajectory-out` ATIF straight to Harbor
  with no converter, so the export's stated consumer is now an actual one.

## 2026-07-31 — Terminal-Bench trajectories retained

- Terminal-Bench now keeps Harbor job directories by default so ATIF
  trajectories and Yolop event logs survive result summarization; constrained
  runs can opt out with `TB_KEEP_JOBS=0`.
- Eval metadata now distinguishes matrix-requested provider settings from the
  effective provider, model, and reasoning effort recorded on completed model
  responses.

## 2026-07-31 — Durable monitoring without polling turns

- [Background execution](specs/background.md) now defines semantic polling
  detection across a bounded observation window, so heterogeneous status/task
  cycles steer to one durable background watch without flagging one-off checks.
- Background completions queued at one idle boundary are coalesced into one
  TUI or ACP wake turn while retaining their durable task results.

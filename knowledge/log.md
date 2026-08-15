# Knowledge Log

Significant changes to Yolop's durable knowledge are recorded here. Routine
wording, formatting, and link fixes do not need entries.

## 2026-08-15 — Interactive tracing no longer corrupts terminal frames

- Fullscreen and inline renderers now route `RUST_LOG` diagnostics to private,
  bounded files under the platform data directory instead of stderr. CLI,
  `--print`, and ACP behavior is unchanged.

## 2026-08-15 — Terminal verification split into tiers

- The release spec's "Manual Terminal Matrix" became
  [Terminal Verification](specs/release.md#terminal-verification), organized by
  what actually checks each row: asserted per PR, best-effort nightly, or human.
  Presenting one undifferentiated checklist led a release to report rows as
  unverified that CI had already gated on the same commit, and invited humans to
  re-walk them.
- The tmux gallery capture moved from nightly-only to `ci.yml`'s `Build + Tests`
  job, so a real terminal implementation checks the render on every PR and a
  release can cite a run against its own commit. `scripts/nightly-assert-gallery.sh`
  lost its `nightly-` prefix accordingly; the nightly leg keeps running as a
  scheduled re-check.
- The human tier now lists GUI emulators only. tmux left the manual list because
  a machine gates it.

## 2026-08-14 — Layered tool-call shape enforcement and repair

- Added [tool-call shape enforcement](specs/tool-calling.md): compatible Codex
  schemas request strict generation, every provider is guarded by full-schema
  pre-execution validation, and Everruns' bounded `tool_call_repair` is enabled
  for one corrective attempt.
- [Progress guard](specs/progress-guard.md) keeps `progress_checkpoint` eager and
  makes array shapes explicit, while [tool search](specs/tool-search.md) records
  the mandatory transition as part of the static eager profile.

## 2026-08-14 — ACP authentication recovery through setup

- [ACP](specs/acp.md) now routes Codex authentication failures to `/setup` and
  API-key failures to the secure environment/restart path; plain `/setup`
  selects the active provider's advertised login, refreshes open sessions, and
  invalid Codex credentials are cleared before the user signs in again.

## 2026-08-14 — Manual test scenarios collection

- Added [manual test scenarios](test-scenarios/index.md), a home for flows the
  automated suite cannot reach — live provider, network, or a judgement about
  what the terminal actually shows. Scenarios supplement the shipping bar's
  automated-test requirement; they never replace it.
- First scenario covers installing a skills.sh skill mid-session and rendering
  its Mermaid answer as a terminal diagram.
- [Release](specs/release.md) now draws its smoke paths from the collection:
  impact analysis picks which scenarios a cut walks, so a release stops
  improvising a smoke path. The manual terminal matrix stays in the release
  spec, which owns the gate requiring it.

## 2026-08-14 — OpenRouter PKCE browser login

- [Configuration](specs/configuration.md) records that OpenRouter can mint a
  user-controlled API key through PKCE browser login, stored as
  `tokens.openrouter` like a pasted key.
- [ACP](specs/acp.md) advertises `openrouter_browser` alongside `codex_browser`
  so editors can connect OpenRouter without a pre-set environment variable.

## 2026-08-14 — Registry skill install reaches real sessions

- `search_skills` / `install_skill` / `delete_skill` were registered but never
  enabled in the default coding harness, so no session ever exposed them while
  [Skills](specs/skills.md) and the README documented them. The harness now
  enables `yolop_skill_management`, and the cold-start guard asserts the three
  tools reach the assembled session rather than only the capability.
- Tool descriptions are provider-visible even when schemas are deferred, so the
  three registry tools dropped the workflow prose the skill-management skill
  already owns.

## 2026-08-12 — Streaming workspace grep

- [Sandboxing](specs/sandboxing.md) records that broad structured grep streams
  bounded files instead of failing on a small aggregate input cap, while
  retaining path, per-file, pagination, and response limits.

## 2026-08-12 — User-ask tracking becomes experimental opt-in

- [User ask](specs/user-ask.md) is no longer part of the default harness. The
  registered `yolop_user_ask` capability remains available through an explicit
  `[[capabilities]]` settings override while the completion behavior is
  experimental.

## 2026-08-11 — Fullscreen mouse selection survives Ctrl and Ctrl+C copies

- [Presentation](specs/presentation.md) records that fullscreen drag-select is
  application-owned: bare modifier key events must not dismiss the highlight,
  and with an active selection `Ctrl+C` re-arms OSC 52 copy instead of
  interrupting. Typing still clears the selection.

## 2026-08-09 — Meta Model API provider

- Yolop adopts the Everruns 0.17.25 family and registers `everruns-meta` as the
  first-class `meta` provider. `MODEL_API_KEY` enables Muse Spark 1.2 and its
  Contributor profile through CLI, setup, settings, and model discovery.

## 2026-08-09 — Everruns 0.17.24 adoption; upstream example is no longer a mirror

- [Maintenance](specs/maintenance.md) no longer treats `examples/coding-cli` as a
  mirror source. Upstream rebuilt it as the acceptance test for its new
  `everruns` facade crate — one dependency, no TUI/MCP/provider wiring — so
  yolop is now the more complete agent and has nothing left to pull from it.
  Track the `everruns-*` library surface and upstream's changelog instead.
- Maintenance also records that a clean compile is not sufficient evidence of
  adoption, citing the two 0.17.24 behavior changes that raised no compile
  error: driver model discovery began returning embedding models, and MCP OAuth
  began requiring an HTTPS resource origin.
- `everruns-platform` joins the pinned everruns family; the identity and
  platform-store types yolop implements moved there out of `everruns-core`.
- Yolop deliberately does not adopt the new `everruns` facade. The rationale
  lives beside the dependencies in `Cargo.toml`.

## 2026-08-08 — Provider stall recovery budget and failure UX

- [Checkpointing](specs/checkpointing.md) now records that Yolop installs a
  stall liveness window with an elapsed recovery budget large enough for full
  stall retries (upstream's default elapsed budget is shorter than one window).
- [User ask](specs/user-ask.md) now classifies provider/runtime failures as
  failed before charging the continuation budget, so a stall never surfaces as
  "budget exhausted".

## 2026-08-07 — Connected ACP model catalog and authentication

- ACP session creation now falls back from a stale disconnected preference to
  a usable provider, exposes only connected providers, advertises agent-handled
  Codex browser authentication, and pushes standard `config_option_update`
  notifications after authentication or `/setup` changes.
- The cross-provider `default_model` setting was removed. Durable model choices
  are provider-scoped under `models.<provider>`; connection state determines
  which choices ACP exposes.

## 2026-08-07 — Shared completion and host wake routing

- [User ask](specs/user-ask.md) now delegates deterministic turn completion and
  continuation budgets to `everruns-core`; Yolop retains ask-specific tagging,
  prompts, evaluation projection, and host streaming.
- [Background execution](specs/background.md) now delegates live-session route
  ownership and retryable closed-route handling to `everruns-local`; Yolop's
  inner runner retains authenticated task handoffs, wake coalescing, and
  synchronous child-session turns.
- [MCP](specs/mcp.md) now delegates OAuth discovery, dynamic registration,
  PKCE, resource binding, callback-issuer validation, code exchange, and
  serialized token refresh to the shared Everruns client. Yolop retains the
  loopback callback host, connection-file adapter, environment fallback, and
  explicit loopback egress exception.

## 2026-08-06 — Upstream interactive approval and cancellation

- ACP tool gating now delegates risk classification, permission decisions, and
  remembered answers to `everruns-core`; Yolop retains only the adapter that
  supplies its mutable central approval level on every call.
- ACP cancellation now awaits runtime task teardown before returning, allowing
  active tools' cooperative `ToolContext` cancellation tokens to reach detached
  child work before the client continues.

## 2026-08-05 — Bounded provider-turn recovery

- Provider stalls, transport failures, overload, and retryable server failures
  recover inside the active everruns reason phase under shared attempt and time
  budgets, preserving completed tool outputs and checkpoint identity.
- Yolop serializes single-use Codex token rotation across driver instances and
  rejects models absent from an available provider catalog before persisting the
  ask.

## 2026-08-05 — Cumulative-cost context checkpoints

- [Checkpointing](specs/checkpointing.md) now defines cumulative uncached input
  and accumulated raw tool-result bytes as early proactive-compaction signals.
  Cost pressure shares the existing durable replacement, history retrieval,
  rewind lineage, retry bounds, and failure fallback; a marginal prompt floor
  keeps short follow-ups out of the compaction path.
- Active turns now admit already-persisted event boundaries for durable context
  checkpoints while rejecting future boundaries, so proactive replacement can
  install before the turn closes without weakening rewind lineage.
## 2026-08-05 — Default bounded task completion

- [User ask](specs/user-ask.md) is now the default host completion safety net
  across TUI, `--print`, and ACP. Cheap deterministic evidence closes trivial,
  failed, blocked, and background-waiting turns; only ambiguous tool-using
  candidate finals pay for semantic evaluation. In-progress work continues from
  compact state inside six-turn, 64k-token, and ten-minute budgets.

## 2026-08-05 — Task-shaped capability disclosure

- [Tool search](specs/tool-search.md) now keeps only first-turn repository
  discovery and bookkeeping schemas eager. Mutation, background,
  release/control, and specialized tools remain visible and load their schemas
  progressively; opt-in host profiles and extension manifests can retain eager
  schemas where measured or explicitly requested.
- [System prompt composition](specs/system-prompt.md) records the cache-stable
  profile and its default-surface result: unchanged prompt bytes, 39.3% fewer
  provider-visible tool-definition bytes, and 69.4% fewer schema bytes.

## 2026-08-05 — Compact background completion handoffs

- Automatic background-completion turns now replace the provider-visible parent
  transcript prefix with a bounded, host-provenance handoff assembled from the
  durable task snapshot. Active intent, scope, outcome, validation, and artifact
  references survive while full session history and raw logs remain queryable.
- Missing or invalid task summaries fall back to the lossless history path, and
  task-authored free-form content is marked as untrusted execution data rather
  than instructions.

## 2026-08-05 — Progress guard became a trajectory controller

- Added [Progress guard trajectory control](specs/progress-guard.md): warnings
  are one-shot per unchanged evidence state, exact repeated reads reuse compact
  freshness markers, and post-budget exploration is host-blocked until a
  bounded structured checkpoint records facts, hypothesis, missing evidence,
  and one decisive action.
- Guard state is bounded and resumes only against a matching active tool
  trajectory; mutation, validation, new scopes, and externally changed result
  bytes reset only the state they invalidate.

## 2026-08-05 — Non-blocking repository pulse at startup

- [Presentation](specs/presentation.md) now defines startup as a transient empty
  state rather than synthetic transcript history. Workspace readiness and the
  composer appear immediately; fullscreen Git-derived repository, branch,
  cleanliness, and latest-commit context arrive from a background worker
  without extending time to first input. Inline mode keeps the minimal state
  stable and skips repository inspection to avoid footer reflow.

## 2026-08-03 — Cache-stable live model context

- [System prompt composition](specs/system-prompt.md) now exposes the effective
  provider, model, and reasoning effort through prompt-only conversation
  annotations on the first turn and when the values change. Stable message-id
  placement preserves provider prompt-cache prefixes; compaction and rewind
  re-emit the current state when they remove the last marker.

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

## 2026-08-05 — Lazy session materialization

- Fresh runtimes no longer create discoverable session directories or empty
  event logs until a durable event, checkpoint, worktree, or other persisted
  artifact exists.
- Non-discoverable owner-private coordination locks preserve fail-fast
  simultaneous-open safety before the event log is materialized.

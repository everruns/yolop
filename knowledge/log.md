# Knowledge Log

## 2026-08-18, Session coordination uses attached CLI actions

- [Session coordination](specs/session-coordination.md): removed its four
  model-visible tools and made `yolop coordination` the sole agent-facing
  administration surface, consistent with extensions and the shared control
  plane.
- Added typed `dispatch` and `complete` CLI/control actions. Attached session
  identity and configured role authorize them; detached execution remains
  limited to presence listing.
- Multiword work and completion fields use unquoted word sequences so direct
  foreground commands stay eligible for conservative anonymous-pipe
  attachment.

## 2026-08-18, Attached administration is discovered from one prompt block

- [Extensions](specs/extensions.md): the discovery hint for attached
  `yolop <subcommand> ...` administration moved out of the Bash tool
  description, which hardcoded `extensions` and advertised it even where no
  control route was registered, and never mentioned `coordination`.
- `ControlPlaneCapability` renders one system-prompt block from the routes
  registered in the session. A capability contributing a CLI route supplies a
  one-clause `ControlRoute::summary` and no prompt prose of its own, so routes
  do not each grow a block.
- No registered route means no capability and no block, so the prompt never
  names a surface the session lacks.

## 2026-08-18, run_command covers the whole registry, on every host

- [Commands](specs/commands.md): `run_command` no longer carries its own
  allowlist of terminal commands. A name is resolved against
  `runtime.list_commands` and run through `runtime.execute_command`, so
  anything a user can type (for example `/setup reauthenticate <provider>`) is
  reachable from a turn. `command: help` returns the live list.
- The tool moved out of the TUI-gated client capability into `agent_commands`,
  registered on every host: ACP and `--print` sessions run the commands their
  own registry holds. The terminal's `HostUi` is now optional, used only so
  `/mcp` and `/tools` return the transcript lines the host printed.
- `Skill` commands stay prompt-activated and `/shell` stays typed-only.
- [Conversational control](specs/conversational-control.md): the control-surface
  inventory now names the whole registry, on every host.

## 2026-08-18, Local session coordination

- Added [Session coordination](specs/session-coordination.md): an opt-in local
  coordinator and worker protocol encapsulated in one capability.
- Dispatches reuse the canonical session-task registry while leased presence,
  atomic reservation, and durable assignment and completion inboxes share the
  private `everruns-local` SQLite store.
- Attached control from the extension CLI surface now also carries live
  coordination status and availability operations.

Significant changes to Yolop's durable knowledge are recorded here. Routine
wording, formatting, and link fixes do not need entries.

## 2026-08-18, Profiles carry a whole agent, not just execution settings

- [Configuration](specs/configuration.md): profiles gained `capabilities`,
  `mcp`, `instructions` / `instructions_file`, and `skills_dir`, with
  `capabilities_mode` / `mcp_mode` to replace the global set instead of layering
  on it. Only credentials and personal settings (`tokens`, `codex_auth`,
  `theme`, `attribution`, `proactive_wake`) stay global-only.
- The reason for the change: v1 profiles could switch provider and paranoia
  level but not define an agent with a job, so a purpose-built agent (triage,
  review, release duty) meant a second config directory. Everything that decides
  what an agent can do is now selectable per run.
- Follow-on effects recorded in the neighbouring specs: extension enablement is
  a `[[capabilities]]` entry, so it became per-profile for free
  ([extensions](specs/extensions.md)); skills gained a profile scope between
  workspace and global ([skills](specs/skills.md)); MCP gained a profile scope
  between global and workspace ([mcp](specs/mcp.md)); and a profile's standing
  instructions are the one operator-owned prompt seam
  ([system prompt](specs/system-prompt.md)).

## 2026-08-17, One-shot attached extension administration

- Replaced model-visible extension management tools with the `yolop extensions`
  CLI while retaining extension-contributed tools and commands.
- Added a versioned, resource-tagged internal control plane over anonymous
  one-request child pipes. Direct foreground commands can reconcile the current
  session without publishing an endpoint or ambient shell capability.
- Made attached administration an optional capability facet; the built-in
  extension capability owns its CLI action grammar, `/extensions` command,
  control execution, and rendering while contributing no model tools.
- Made the top-level CLI itself capability-contributed: `CliCapability`
  supplies the Clap command and typed decoder, and the root binary assembles
  registered contributions without an extension-specific enum or dispatcher.
- Seeded enabled extensions at the session layer so attached disable is
  reversible; detached operations remain global and reload requires a live
  session.

## 2026-08-18, Loopback setup page for ACP clients (experimental)

- [ACP](specs/acp.md) gained an opt-in `local_setup_page` authentication method:
  agent-handled auth is the one place the protocol hands an agent control for an
  out-of-band flow, so a loopback browser form now carries the credential input
  ACP cannot express. Off by default, enabled per run with `--acp-setup-page` or
  per user with the `acp_setup_page` setting.
- Records why the gate is runtime rather than a Cargo feature: the page pulls in
  no dependencies, so a build flag would only keep it out of the release
  binaries, which is where the feedback that decides its future would come from.
- Records why the no-provider hint exists: without a link posted at
  `session/new`, an editor with no key in its environment gets an llmsim-only
  session and no way to fix it from the client.

## 2026-08-16, In-process inference provider (experimental)

- Added [Local inference](specs/local-inference.md): a `local` provider that
  links the inference engine into yolop instead of talking to an external
  server. Weights are pulled explicitly with `yolop models pull` into a store
  that `yolop models list`/`rm` manages, so a turn never blocks on a download.
  The existing `ollama` provider is unchanged, it was never an Ollama
  integration, just the OpenAI driver aimed at loopback.
- Records the build-gate policy and why it points opposite to distribution:
  `local-inference` is off in default features so source builds stay fast, and
  on in the release binaries so the Homebrew install path carries the engine.
  Compile time justifies the gate; binary size is not something it fixes.

## 2026-08-15, Interactive tracing no longer corrupts terminal frames

- Fullscreen and inline renderers now route `RUST_LOG` diagnostics to private,
  bounded files under the platform data directory instead of stderr. CLI,
  `--print`, and ACP behavior is unchanged.

## 2026-08-15, Terminal verification split into tiers

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

## 2026-08-14, Layered tool-call shape enforcement and repair

- Added [tool-call shape enforcement](specs/tool-calling.md): compatible Codex
  schemas request strict generation, every provider is guarded by full-schema
  pre-execution validation, and Everruns' bounded `tool_call_repair` is enabled
  for one corrective attempt.
- [Progress guard](specs/progress-guard.md) keeps `progress_checkpoint` eager and
  makes array shapes explicit, while [tool search](specs/tool-search.md) records
  the mandatory transition as part of the static eager profile.

## 2026-08-14, ACP authentication recovery through setup

- [ACP](specs/acp.md) now routes Codex authentication failures to `/setup` and
  API-key failures to the secure environment/restart path; plain `/setup`
  selects the active provider's advertised login, refreshes open sessions, and
  invalid Codex credentials are cleared before the user signs in again.

## 2026-08-14, Manual test scenarios collection

- Added [manual test scenarios](test-scenarios/index.md), a home for flows the
  automated suite cannot reach, live provider, network, or a judgement about
  what the terminal actually shows. Scenarios supplement the shipping bar's
  automated-test requirement; they never replace it.
- First scenario covers installing a skills.sh skill mid-session and rendering
  its Mermaid answer as a terminal diagram.
- [Release](specs/release.md) now draws its smoke paths from the collection:
  impact analysis picks which scenarios a cut walks, so a release stops
  improvising a smoke path. The manual terminal matrix stays in the release
  spec, which owns the gate requiring it.

## 2026-08-14, OpenRouter PKCE browser login

- [Configuration](specs/configuration.md) records that OpenRouter can mint a
  user-controlled API key through PKCE browser login, stored as
  `tokens.openrouter` like a pasted key.
- [ACP](specs/acp.md) advertises `openrouter_browser` alongside `codex_browser`
  so editors can connect OpenRouter without a pre-set environment variable.

## 2026-08-14, Registry skill install reaches real sessions

- `search_skills` / `install_skill` / `delete_skill` were registered but never
  enabled in the default coding harness, so no session ever exposed them while
  [Skills](specs/skills.md) and the README documented them. The harness now
  enables `yolop_skill_management`, and the cold-start guard asserts the three
  tools reach the assembled session rather than only the capability.
- Tool descriptions are provider-visible even when schemas are deferred, so the
  three registry tools dropped the workflow prose the skill-management skill
  already owns.

## 2026-08-12, Streaming workspace grep

- [Sandboxing](specs/sandboxing.md) records that broad structured grep streams
  bounded files instead of failing on a small aggregate input cap, while
  retaining path, per-file, pagination, and response limits.

## 2026-08-12, User-ask tracking becomes experimental opt-in

- [User ask](specs/user-ask.md) is no longer part of the default harness. The
  registered `yolop_user_ask` capability remains available through an explicit
  `[[capabilities]]` settings override while the completion behavior is
  experimental.

## 2026-08-11, Fullscreen mouse selection survives Ctrl and Ctrl+C copies

- [Presentation](specs/presentation.md) records that fullscreen drag-select is
  application-owned: bare modifier key events must not dismiss the highlight,
  and with an active selection `Ctrl+C` re-arms OSC 52 copy instead of
  interrupting. Typing still clears the selection.

## 2026-08-17, Local persistence moves to the everruns facade

- Filesystem persistence now comes from `everruns = { default-features = false,
  features = ["local"] }` rather than the standalone `everruns-local` crate. The
  facade re-exports the same backend under `everruns::local`, so every type yolop
  used, `LocalBackends`, `LocalProfile`, `SqliteDb`, `LocalPlatformStore`,
  `LocalSessionRunner`, `LocalScheduleStore`, `LocalScheduleRunnerHandle`,
  `LocalSessionTaskRegistry`, `HostRoutedRunner`, and `WakeRoutes`, keeps its name
  and shape. Same SQLite, git-workspace, and durable-log backend, verified by a
  session that persists across two processes.
- This is what unblocked `everruns-host` 0.19.0. `everruns-local` is published only
  against host ^0.18.0, so depending on it resolved two copies of `everruns-host`
  and held yolop on the 0.18 line. The facade depends on host ^0.19.0 and does not
  use `everruns-local` at all.
- **host 0.19 absorbed the session services.** `everruns-host::session_services`
  now carries `SessionCapability`, `SessionStorageCapability`, and the
  `write_session_title` tool. Registering the standalone
  `everruns-session-services` copy against a 0.19 runtime compiles cleanly but
  silently does nothing: the capability writes to a store the runtime no longer
  reads, so session titles never update. Yolop takes the capability from
  `everruns-host` and no longer depends on the standalone crate.

## 2026-08-17, Everruns 0.18.0 adoption; credentials leave model selection

- Yolop is on the published Everruns 0.18.0 family, with no git dependency. The
  0.18 core decomposition moved driver, model, and provider types out of
  `everruns-core` into `everruns-provider`, moved the built-in capabilities into
  `everruns-builtins`, and left `everruns-core` mostly a set of traits.
- **Selection and credentials are now separate.** `ModelSpec` names a provider
  and a model and cannot carry a key; credentials reach a driver through
  `ProviderStore::get_provider_config`. Yolop keeps its own credentialed record
  for settings resolution and splits it at that boundary, and supplies its own
  provider store, because the built-in store owns no credentials and a host that
  keeps them elsewhere would otherwise build every driver without a key.
  Returning `None` is the supported "selected but not configured yet" state: the
  provider stays constructible so `/setup` still runs, and provider calls fail
  locally at the credential boundary instead of sending an empty key and
  surfacing a remote 401.
- `ChatDriver::chat_completion_stream` now receives the resolved
  `ProviderEndpoint` per call rather than having it baked into the driver, which
  is the same credential/endpoint split seen from the driver side. Drivers that
  do not talk to an endpoint, such as Codex and in-process inference, ignore it.
- The simulator behind `--provider llmsim` now comes from the production-safe
  `everruns-llmsim`. `everruns-test-support` documents itself as test-only;
  yolop still depends on it for `InMemoryMessageRetriever`, which has no other
  home.
- A clean compile again proved insufficient. `llm_sim` had briefly both
  registered the simulator and set it as the default model, which silently
  overrode yolop's selection for every provider when called after
  `default_model`. 0.18 splits that into `llm_sim` and `llm_sim_as_default`;
  yolop selects its default explicitly either way.

## 2026-08-09, Meta Model API provider

- Yolop adopts the Everruns 0.17.25 family and registers `everruns-meta` as the
  first-class `meta` provider. `MODEL_API_KEY` enables Muse Spark 1.2 and its
  Contributor profile through CLI, setup, settings, and model discovery.

## 2026-08-09, Everruns 0.17.24 adoption; upstream example is no longer a mirror

- [Maintenance](specs/maintenance.md) no longer treats `examples/coding-cli` as a
  mirror source. Upstream rebuilt it as the acceptance test for its new
  `everruns` facade crate, one dependency, no TUI/MCP/provider wiring, so
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

## 2026-08-08, Provider stall recovery budget and failure UX

- [Checkpointing](specs/checkpointing.md) now records that Yolop installs a
  stall liveness window with an elapsed recovery budget large enough for full
  stall retries (upstream's default elapsed budget is shorter than one window).
- [User ask](specs/user-ask.md) now classifies provider/runtime failures as
  failed before charging the continuation budget, so a stall never surfaces as
  "budget exhausted".

## 2026-08-07, Connected ACP model catalog and authentication

- ACP session creation now falls back from a stale disconnected preference to
  a usable provider, exposes only connected providers, advertises agent-handled
  Codex browser authentication, and pushes standard `config_option_update`
  notifications after authentication or `/setup` changes.
- The cross-provider `default_model` setting was removed. Durable model choices
  are provider-scoped under `models.<provider>`; connection state determines
  which choices ACP exposes.

## 2026-08-07, Shared completion and host wake routing

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

## 2026-08-06, Upstream interactive approval and cancellation

- ACP tool gating now delegates risk classification, permission decisions, and
  remembered answers to `everruns-core`; Yolop retains only the adapter that
  supplies its mutable central approval level on every call.
- ACP cancellation now awaits runtime task teardown before returning, allowing
  active tools' cooperative `ToolContext` cancellation tokens to reach detached
  child work before the client continues.

## 2026-08-05, Bounded provider-turn recovery

- Provider stalls, transport failures, overload, and retryable server failures
  recover inside the active everruns reason phase under shared attempt and time
  budgets, preserving completed tool outputs and checkpoint identity.
- Yolop serializes single-use Codex token rotation across driver instances and
  rejects models absent from an available provider catalog before persisting the
  ask.

## 2026-08-05, Cumulative-cost context checkpoints

- [Checkpointing](specs/checkpointing.md) now defines cumulative uncached input
  and accumulated raw tool-result bytes as early proactive-compaction signals.
  Cost pressure shares the existing durable replacement, history retrieval,
  rewind lineage, retry bounds, and failure fallback; a marginal prompt floor
  keeps short follow-ups out of the compaction path.
- Active turns now admit already-persisted event boundaries for durable context
  checkpoints while rejecting future boundaries, so proactive replacement can
  install before the turn closes without weakening rewind lineage.
## 2026-08-05, Default bounded task completion

- [User ask](specs/user-ask.md) is now the default host completion safety net
  across TUI, `--print`, and ACP. Cheap deterministic evidence closes trivial,
  failed, blocked, and background-waiting turns; only ambiguous tool-using
  candidate finals pay for semantic evaluation. In-progress work continues from
  compact state inside six-turn, 64k-token, and ten-minute budgets.

## 2026-08-05, Task-shaped capability disclosure

- [Tool search](specs/tool-search.md) now keeps only first-turn repository
  discovery and bookkeeping schemas eager. Mutation, background,
  release/control, and specialized tools remain visible and load their schemas
  progressively; opt-in host profiles and extension manifests can retain eager
  schemas where measured or explicitly requested.
- [System prompt composition](specs/system-prompt.md) records the cache-stable
  profile and its default-surface result: unchanged prompt bytes, 39.3% fewer
  provider-visible tool-definition bytes, and 69.4% fewer schema bytes.

## 2026-08-05, Compact background completion handoffs

- Automatic background-completion turns now replace the provider-visible parent
  transcript prefix with a bounded, host-provenance handoff assembled from the
  durable task snapshot. Active intent, scope, outcome, validation, and artifact
  references survive while full session history and raw logs remain queryable.
- Missing or invalid task summaries fall back to the lossless history path, and
  task-authored free-form content is marked as untrusted execution data rather
  than instructions.

## 2026-08-05, Progress guard became a trajectory controller

- Added [Progress guard trajectory control](specs/progress-guard.md): warnings
  are one-shot per unchanged evidence state, exact repeated reads reuse compact
  freshness markers, and post-budget exploration is host-blocked until a
  bounded structured checkpoint records facts, hypothesis, missing evidence,
  and one decisive action.
- Guard state is bounded and resumes only against a matching active tool
  trajectory; mutation, validation, new scopes, and externally changed result
  bytes reset only the state they invalidate.

## 2026-08-05, Non-blocking repository pulse at startup

- [Presentation](specs/presentation.md) now defines startup as a transient empty
  state rather than synthetic transcript history. Workspace readiness and the
  composer appear immediately; fullscreen Git-derived repository, branch,
  cleanliness, and latest-commit context arrive from a background worker
  without extending time to first input. Inline mode keeps the minimal state
  stable and skips repository inspection to avoid footer reflow.

## 2026-08-03, Cache-stable live model context

- [System prompt composition](specs/system-prompt.md) now exposes the effective
  provider, model, and reasoning effort through prompt-only conversation
  annotations on the first turn and when the values change. Stable message-id
  placement preserves provider prompt-cache prefixes; compaction and rewind
  re-emit the current state when they remove the last marker.

## 2026-08-01, Tuika structured-markdown renderers

- [Tuika](specs/tuika.md) now records the HTML companion renderer and the
  current structured-markdown renderer API alongside Mermaid diagrams.

## 2026-08-01, Conversational skills.sh search and install

- [Skills](specs/skills.md) and [Conversational control](specs/conversational-control.md)
  now cover `search_skills` / `install_skill`: query the public skills.sh
  registry, ask which match to install, and write the snapshot into workspace
  or global scope without restarting.

## 2026-08-01, Owner evidence before non-obvious mutation

- [System prompt composition](specs/system-prompt.md) now requires repository
  evidence for the root cause and owning abstraction before the first mutation
  of a non-obvious bug, while preserving the one-read path for explicit local
  edits.

## 2026-07-31, Dependency-aware tool batching

- Yolop now requests provider parallel tool calling and carries a measured
  dependency-aware batching rule: independent calls share a model round,
  dependent calls remain sequential, and title/todo/status bookkeeping
  piggybacks on substantive work. The focused gpt-5.5 A/B kept 9/9 tasks correct
  while reducing mean model calls by 27% and cumulative input including cache
  reads by 26%; its dependent-read control never co-batched a call with the
  result it needed.
- Title and todo handling remains in runtime tools rather than a deterministic
  host side channel, preserving event replay and presentation ownership.

## 2026-07-31, Discovery reuse and useful session admission

- Repeated large read/search results on an unchanged target now return a compact
  freshness marker, with reuse invalidated by workspace mutation; the first full
  result remains available in context.
- Local session discovery counts only logs with user-visible messages or a
  recorded model failure toward its 500-session scan budget, so interrupted
  empty/invalid shells cannot crowd useful overlapping work out.

## 2026-07-31, Responsive activity rail and session-scoped transcripts

- The interactive TUI now opens a passive right-hand activity rail when
  sub-agents appear. Its flat Yolop-native chrome groups agents separately from
  background commands and waiting monitors; overflow scrolls and follows new
  work, while narrow focused rails become visible drawers instead of trapping
  focus off-screen. Root transcript catch-up filters child-session events just
  like live delivery, preventing child title and tool narration from leaking
  into the coordinator's transcript.

## 2026-07-31, Local sub-agents enabled

- Yolop now drives linked child sessions through its local platform runner and
  enables `spawn_agent` by default. A bounded two-level hierarchy can contain 20
  active sub-agents, while each session retains the upstream five-background-run
  ceiling. The `Ctrl+B` activity rail presents the hierarchy and branch usage.

## 2026-07-31, Named configuration profiles

- [Configuration](specs/configuration.md) now defines explicit `--profile`
  selection, sparse execution overlays above global settings, global-only
  credential and structural keys, active-layer persistence, and profile
  visibility. [ACP](specs/acp.md) records profile defaults below standard live
  model selection, while [Presentation](specs/presentation.md) makes the active
  profile part of safety status.
## 2026-07-30, ACP model selection uses standard session configuration

- [ACP integration](specs/acp.md) now exposes model and reasoning-effort choices
  through standard `configOptions` and applies changes through
  `session/set_config_option`. The earlier private `yolop.dev/acp`
  `selectedModel` metadata contract was removed because editor clients cannot
  discover or use private selection protocols without bespoke integration.

## 2026-07-27, Herdr identifies concurrent Yolop sessions

- [Herdr integration](specs/herdr.md) now forwards Yolop session titles to pane
  metadata and uses them in display-agent labels, with a stable session suffix
  before title generation, so concurrent Yolop agents remain distinguishable.
  Lifecycle states also gain human-readable labels while retaining the `yolop`
  machine identity for grouping and ownership.

## 2026-07-27, First-party extension placement

- First-party extension packages now live under `extensions/`, reserving
  `crates/` for core libraries and workspace packages. Rust extension releases
  package their manifest and README so crates.io installation can provision the
  extension package. See [Extension system](specs/extensions.md).

## 2026-07-26, OKF skill tracks the authoritative v0.2 spec

- The bundled `okf` skill was rewritten against `SPEC.md` in
  `GoogleCloudPlatform/knowledge-catalog`, now named as the only authoritative
  source; the `okf.md` site it previously cited is not normative and described a
  superseded v0.1 model. [`okf`](specs/okf.md) gained the v0.2 families the skill
  must teach, `sources` with credibility signals, `generated`/`verified` with
  the actor convention and trust tiers, `status`/`stale_after`, and the
  `Attested Computation` contract, plus the rule that validator lint stays
  behind `--strict` so a permissive format is not taught as a strict one, and the
  requirement that the skill's validator copy stay byte-identical to the one CI
  runs.

## 2026-07-26, The scrollback renderer is tuika's split-footer mode

- `--inline` now composes tuika's `ScreenMode::SplitFooter` instead of yolop's
  own inline-viewport anchoring and `insert_before` publishing.
  [Tuika](specs/tuika.md) gained the screen-mode boundary, the toolkit owns
  pinning, publishing, and footer teardown; yolop owns which lines get
  published, and [Presentation](specs/presentation.md) states what the mode
  guarantees a user.
- A transcript entry now appears exactly once: the footer paints only what is
  not yet published, and publishing holds back the rows the footer still shows,
  cutting an entry in half when one straddles the edge. Everything is published
  at exit. [Presentation](specs/presentation.md) carries the rule.
- The dependency rule in [Tuika](specs/tuika.md) now says *why* the crates.io
  pin is a constraint rather than a preference: `cargo publish` rejects a
  dependency without a version requirement, so a git dependency would leave
  yolop unreleasable for as long as it stayed.

## 2026-07-25, Host rendering advertised to the model

- `<environment_context>` now carries `ui_capabilities` beside `client_ui`, an
  additive list of what the host renders, so the model can decide whether a
  diagram is worth drawing.
  [System prompt composition](specs/system-prompt.md) gained the rule these
  fields follow: state the host's capability rather than an instruction, and
  keep the fields static per host so a mid-session change cannot churn the
  cached prefix.

## 2026-07-25, Mermaid fences in the transcript

- Yolop fills tuika's `FencedBlockRenderer` boundary with `tuika-mermaid`, so
  ` ```mermaid ` fences render as Unicode diagrams. [Tuika](specs/tuika.md)
  gained the third companion crate and the rule that a fenced block which does
  not fit the transcript width falls back to the themed code block rather than
  being painted clipped.

## 2026-07-24, Tuika moved to its own repository

- `tuika` and `tuika-codeformatters` left this workspace for
  [everruns/tuika](https://github.com/everruns/tuika) and are now consumed from
  crates.io. Yolop publishes two crates again (`yolop-yep`, `yolop`).
- Replaced the two tuika-internal concepts (keymap engine, image rendering) with
  [Tuika](specs/tuika.md), which records the dependency boundary, the boundaries Yolop
  fills, and the testing split. The toolkit's own design rationale now lives in
  its repository.

## 2026-07-24, Agent context layering

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
- Added `specs/keymap.md`: the tuika declarative key-binding engine and Yolop's dispatch of its global shortcuts through it. (Superseded, the engine's rationale moved to the tuika repository; see [Tuika](specs/tuika.md).)
- Excluded the tuika demo/theme/styling GIFs from the published `.crate` (~8.8 MiB → ~1 MiB), keeping only the two README-embedded assets; documented the GitHub-only/crate split in the documentation spec.

## 2026-07-23

- Established `knowledge/` as Yolop's OKF bundle.
- Migrated product specifications to `knowledge/specs/` and added typed OKF metadata.
- Added repository, shipping, maintenance, release, and CI rules to keep the bundle current.

## 2026-07-24, Fullscreen renderer became the default

- Made the alternate-screen fullscreen renderer the default for interactive sessions.
- Added `--inline` as the explicit opt-out for terminal-native scrollback.

## 2026-07-30, Terminal-Bench 2.1 eval study

- Added `evals/terminal_bench/`: yolop on Terminal-Bench 2.1 (89 containerized
  tasks), stacking the Mira host over Harbor, Mira owns the matrix and
  reporting, Harbor owns the container, the agent run, and the task's verifier.
- Realized the Harbor half of [Trajectory export](specs/trajectory.md): the
  study's Harbor agent adapter hands `--trajectory-out` ATIF straight to Harbor
  with no converter, so the export's stated consumer is now an actual one.

## 2026-07-31, Terminal-Bench trajectories retained

- Terminal-Bench now keeps Harbor job directories by default so ATIF
  trajectories and Yolop event logs survive result summarization; constrained
  runs can opt out with `TB_KEEP_JOBS=0`.
- Eval metadata now distinguishes matrix-requested provider settings from the
  effective provider, model, and reasoning effort recorded on completed model
  responses.

## 2026-07-31, Durable monitoring without polling turns

- [Background execution](specs/background.md) now defines semantic polling
  detection across a bounded observation window, so heterogeneous status/task
  cycles steer to one durable background watch without flagging one-off checks.
- Background completions queued at one idle boundary are coalesced into one
  TUI or ACP wake turn while retaining their durable task results.

## 2026-08-05, Lazy session materialization

- Fresh runtimes no longer create discoverable session directories or empty
  event logs until a durable event, checkpoint, worktree, or other persisted
  artifact exists.
- Non-discoverable owner-private coordination locks preserve fail-fast
  simultaneous-open safety before the event log is materialized.

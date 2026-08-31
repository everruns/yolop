# Knowledge Log

## 2026-08-31, Attached model switching is live-only

The singular attached `yolop model` command shows and switches the running session.
`model use` no longer writes provider or model defaults; persistent defaults remain under
`yolop config model`.

## 2026-08-29, Skills and repository tools use physical paths

- [Skills](specs/skills.md): every skill scope now points at a real directory.
  Built-in skills remain materialized under the data directory, extension skills
  use their installed package directories, and generated environment skills are
  materialized beneath the session directory.
- File tools read physical skill directories through an explicit read-only route.
  Yolop no longer creates virtual skill mounts or a `/workspace` repository alias.
- Host-backed repository scanners accept repository-relative paths and contained
  real absolute paths. Unrelated or synthetic absolute paths are rejected.

## 2026-08-29, Semantic navigation stays ready during code work

Code sessions now receive `repo_map`, `repo_symbols`, and `ast_grep` with their
full schemas in the eager tool profile. The progress guard tracks overlapping
file-read intervals across mutations and redirects repeated paging toward those
semantic tools before requiring a checkpoint.

## 2026-08-28, Auto worktrees are model-initialized

- [Git worktrees](specs/worktrees.md): `auto` no longer classifies prompt text
  with implementation-verb heuristics. The system prompt directs the model to
  run the attached `yolop worktree init` command before repository mutation.
- `/worktree` uses the same idempotent session initializer, while persistent mode
  changes remain owned by the config subsystem. Generic Git worktree lifecycle
  operations are not duplicated as Yolop session commands.

## 2026-08-26, Reasoning is ordered content, not a message field

- everruns 0.19 / provider 0.20 replaced the flat `Message.thinking` and
  `thinking_signature` pair with `ContentPart::Reasoning(ReasoningContentPart)`:
  an ordered list of artifacts, each carrying its own provider, id, signature,
  encrypted payload, and readable text. Ordering is the contract, every current
  provider replays artifacts in the position it issued them, which one flattened
  field per message could not express.
- Readable reasoning is now typed by what the provider actually exposes:
  verbatim chain-of-thought, a curated summary, or redacted. The stream carries
  the same distinction on `LlmStreamEvent::ReasoningDelta { summary }`, so the
  Codex driver reads it off the wire instead of downstream code guessing.
- `ReasonItemData` no longer carries `encrypted_content`. Opaque replay state
  lives on the message part and stays out of the event stream.
- [Checkpointing](specs/checkpointing.md): the Codex driver replays the
  provider's own `item_id` per artifact. It previously synthesized sequential
  `rs_%08x` ids because the flat field carried none, which Codex could not key
  replay against.
- `ReasoningConfig::effort` is typed. Yolop still holds the effort as a string,
  since model profiles and env are its sources, and parses once at the message
  boundary; an unrecognized value is dropped rather than sent, which is what the
  drivers used to do silently and inconsistently.

## 2026-08-25, The runtime crate yolop names is everruns-host

- [Maintenance](specs/maintenance.md): `everruns-runtime` has not existed since
  upstream deleted it in 0.18, but yolop kept naming it. `yolop --version` and
  the harness telemetry key both resolved their version from a package no
  lockfile contains, so both had been reporting `unknown` since the 0.18 bump.
  A clean compile hid it: `env!` on a build-script variable still expands when
  the lookup fell back to a literal.
- The dependency vector list, README, `AGENTS.md`, and the MCP, extensions,
  checkpointing, and conversational-control specs now name `everruns-host`.
  Historical references to the deleted crate stay as history.
- `everruns-http` is yanked on crates.io and its `DirectEgressService` now
  lives in `everruns-host` behind `direct-egress`, which `mcp-stdio` already
  enables. Dropping the yanked dependency also collapsed a duplicate
  `everruns-provider`, so the family is back to one version per crate.
- The upstream `everruns-core` 0.19 / `everruns-provider` 0.20 wave is not
  adoptable yet: the `everruns` facade and the five integration crates are
  still published against `everruns-core ^0.18.1`, so the bump resolves two
  cores and fails to compile.

## 2026-08-24, Repository mapping starts with its real contract

- [Tool search](specs/tool-search.md): `repo_map` keeps its compact parameter
  schema in the first-turn profile, while `repo_symbols` remains deferred.
  Recent apparent `/workspace`, linked-worktree, and nested-repository path
  failures were rejected before path resolution because a deferred stub hid
  every argument name and models invented foreign depth controls.
- A five-trial, three-arm OpenAI study kept all 30 answers correct. On the
  uncoached linked-worktree case, map-only used one task tool in every trial;
  the deferred baseline needed an extra discovery/control tool in two of five.
  Making `repo_symbols` eager bought no recovery or call-count improvement and
  exceeded the unchanged cold-start schema floor.
- The eager map schema stays at 261 bytes. Its tool description carries the
  optional defaults that strict providers need, after the first live pass
  showed that types and bounds alone encouraged an explicit 200-symbol limit.
  The map-only cold start retains the required tool-definition and schema
  reductions from the undeferred surface.

## 2026-08-24, Retention is not a recovery affordance

- [Tool output](specs/tool-output.md): full command output can remain available
  in the session filesystem without advertising a recovery path to the model.
  Complete inline results need no second tool round; only limited stream content
  receives a bounded path, with leading evidence preserved.
- `everruns-builtins::PersistOutputHook` owns the policy. Yolop verifies the
  installed boundary and compares dependency-isolated binaries for correctness,
  recovery calls, model calls, and result bytes.

## 2026-08-24, Equivalent failures require a different action

- [Progress guard](specs/progress-guard.md): rewritten invocations no longer
  count as progress when structured diagnostics show the same missing command,
  wrong path, usage mistake, or invocation failure on unchanged workspace
  state. The second equivalent failure warns, and the next call through the
  same tool is removed from the model-visible surface and blocked until a
  different tool/action or checkpoint is used.
- Failure evidence stays intact and the classifier is narrow. A nonzero exit is
  not itself misuse, and an initially failing test remains normal diagnostic
  evidence for a fix and revalidation cycle.
- Everruns already exposes the authoritative call and structured result to
  Yolop's pre-tool and post-tool hooks, so ownership remains in the Yolop host
  capability and no runtime dependency change is needed.

## 2026-08-24, A required checkpoint changes the next tool surface

- [Progress guard](specs/progress-guard.md): the checkpoint-required warning is
  now a real provider-visible transition. The next reasoning step omits the
  statically classified read, search, and waiting tools that the pre-tool gate
  would reject, while the eager checkpoint schema and decisive mutation or
  validation paths stay available.
- `tool_search` is part of the blocked exploration set during that transition.
  The warning no longer suggests revealing `progress_checkpoint`, because that
  tool has always been in Yolop's eager schema profile.
- Everruns already reapplies capability tool-definition transforms on every
  reasoning step. Yolop owns the policy and shared guard state, so no upstream
  runtime fork or dependency release is required.

## 2026-08-24, Independent file reads have a batch owner

- [System prompt](specs/system-prompt.md): `read_many_files` belongs to the
  filesystem capability and stays schema-eager in Yolop. It batches only paths
  known before the call; a path learned from content remains sequential.
- [Progress guard](specs/progress-guard.md): a batch read is exploration and
  its repeated-evidence signature retains the result-bearing path order.
- A matched serial-emission A/B measured the owning API rather than provider
  parallelism: three independent reads fell from 4 to 2 task LLM calls and from
  53,412 to 27,583 cumulative input tokens, while every dependent control kept
  logical read width 1.

## 2026-08-22, One route for every input event

- [Tuika](specs/tuika.md): input ownership is now one ordered table of surfaces,
  and every event kind is delivered against it through tuika 0.11's `Router`.
  `handle_key` and `handle_paste` each used to re-derive precedence with their
  own `if` chain.
- The paste chain had never grown the sandbox-approval branch the key chain had,
  so text pasted while the approval prompt owned the screen was inserted into
  the composer behind it and went out with the next prompt. Deriving precedence
  twice is what made that reachable, so the fix is the shared table rather than
  a seventh branch in the second chain.
- A running turn's Esc-to-cancel is a partial claim, not a modal, so ownership
  is resolved against the event rather than once per frame. Reverse search is
  the one surface that outranks the global chord layer.

## 2026-08-21, The default binary stops carrying the inference engine

- [Local inference](specs/local-inference.md): every release target now ships
  twice. `yolop-<target>.tar.gz` is built with default features and is what the
  Homebrew formula points at; `yolop-<target>-metal.tar.gz` and
  `yolop-<target>-cuda.tar.gz` carry the engine with the backend that target can
  use.
- The default asset is the portable one because it is the only build that runs
  wherever the target does: a `cuda` binary needs an NVIDIA driver present, and
  that is not a thing to hand someone who typed `brew install`. Through v0.17.0
  the single shipped binary carried the engine, CPU-only, at +41.3 MiB.
- No CPU-only engine build is offered for download. An unaccelerated engine
  measures the wrong thing, so shipping one would only spread that mistake; the
  feature stays available to anyone building from source.
- [Release](specs/release.md): the accelerated builds are their own job, kept
  out of the Homebrew job's `needs`. `cuda` is the least proven build in the
  release, and a failure there must not hold back the tap. It now at least
  compiles in CI, against Ubuntu's own `nvidia-cuda-toolkit` (nvcc 12.0) with
  its compute capability pinned to 8.0, because no runner has a GPU to
  interrogate. 8.0 is candle's floor, not a preference: its kernels call
  `__hmax_nan`/`__hmin_nan`, which nvcc defines only for `__CUDA_ARCH__ >= 800`,
  so pre-Ampere cards are out of reach of the shipped build. Evidence that it
  *runs* still needs a CUDA host.

## 2026-08-21, A green release workflow can still ship half the binaries

- [Release](specs/release.md): `cli-binaries.yml` built the extension servers
  for one target instead of three, and reported success. A GitHub Actions
  `include` entry that names no existing matrix dimension is merged into every
  combination rather than creating one, so three of them collapsed onto the
  single `crate` job and the last one won.
- v0.17.0 shipped that way: crates.io, the tap, and the CLI binaries were all
  correct, and `yolop extensions install logfire` still 404s on macOS.
- `scripts/check_release_matrix.py` now expands both matrices the way Actions
  does and fails when they disagree. Post-release verification counts the
  per-extension release's assets rather than trusting the tag's existence.

## 2026-08-21, A release publishes the extensions too

- [Release](specs/release.md): the release train carries every publishable
  workspace crate, not just `yolop` and `yolop-yep`. `scripts/publish_order.py`
  derives the order, so a new first-party extension is picked up without editing
  the workflow.
- An extension whose code changed and whose version did not is published
  nowhere, and its `plugin.json` keeps pointing at the previous tag's binaries.
  Bumping a changed extension in both files is part of preparing the release.
- Prebuilt servers ride a per-extension release (`<crate>-v<version>`), so
  post-release verification checks that tag as well as crates.io and the tap.

## 2026-08-21, A compiled extension installs without a toolchain

- [Extensions](specs/extensions.md): a package may declare
  `capabilityServer.binaries`, a URL template over `{name}`/`{version}`/
  `{target}`. When the declared command does not resolve, install fetches the
  archive for the host triple and puts the binary in the package's `bin/`.
- This is what makes a compiled extension work end to end. crates.io carries
  source, the install path is toolchain-free by design, and nothing built the
  binary, so `yolop-extension-logfire` installed cleanly and could never spawn.
- The `.sha256` sibling is required, not optional: install downloads an
  executable that yolop later spawns, so an unverifiable or mismatched archive
  is refused and nothing is written.
- A failed fetch leaves the package installed and reports why. Half-installing
  is worse, and the binary may legitimately arrive another way.
- The host triple comes from `build.rs` (`YOLOP_HOST_TARGET`). `std::env::consts`
  gives OS and arch but never the triple, and guessing it from those is wrong
  for targets that differ only by libc or ABI.
- Binaries publish under a per-extension tag (`<crate>-v<version>`) rather than
  riding yolop's release tag, so the URL is derivable from the package alone.
  The manifest version drives that URL while CI derives the tag from the crate
  version, so a test pins the two together.

## 2026-08-20, A wrong guess should cost one call, not six

- An unrecognized subcommand now lists the ones that exist. Clap's message names
  none of them and its usage line shows a `[COMMAND]` placeholder, so a caller
  that guessed wrong learned nothing and had to fetch `--help` separately. This
  is root-level, so every command group gets it.
- [Extensions](specs/extensions.md): `config show` and `config set` close the
  gap the guessing was reaching for. A non-secret field was reachable only
  through the enable-time prompt or by hand-editing `settings.toml`, and no
  output named where values were stored, so a session went hunting through the
  config directory with `find`, `cat`, and `grep`.
- Secrets keep their prompt-only path: `config set` refuses them and names
  `secret set`, and `config show` reports a secret as set or unset, never by
  value. The leak guard is the reason the read path was incomplete, so widening
  it deliberately stops short of secrets.
- The lesson generalizes: a model guessing a plausible verb is a given. What a
  CLI controls is whether that guess self-corrects in one call or turns into a
  filesystem investigation.

## 2026-08-20, The model menu is a list, not a catalog

- [Model list](specs/model-list.md): `/model`, the status bar, and ACP now serve
  an ordered, cross-provider `[[models]]` list instead of a provider's whole
  models API response. A user switches between a handful of models; nothing in
  yolop used to know which handful.
- Not "favourites" or "bookmarks". Those words describe marking items inside a
  larger list you still browse. Here the list *is* the offer and the catalog is
  the fallback, reachable from a "Browse all models…" row.
- An entry pairs provider with model, so one selection switches both, and the
  same weights through two accounts (direct, OpenRouter) are two entries.
- Two keys split apart: `[[models]]` is the curated menu, `[default_models]` is
  the per-provider memory that used to own the `models` name. A `models` table
  in an existing file still reads as `default_models`.
- Picking a model whose provider has no credential opens sign-in and then
  applies that model, instead of showing a dead option (ACP) or a catalog (TUI).
- Administration is `yolop models list|add|rm|move|use|reset` on the attached
  control plane, following extensions rather than adding model tools. The
  local-weights command it displaced became `yolop weights`.
## 2026-08-20, Logfire traces nest, and carry the Gen-AI conventions

- [Extensions](specs/extensions.md): the bundled `logfire` exporter now parents
  by `context.parent_span_id` instead of re-deriving a flat tree from
  `turn_id`. Verified against a live Anthropic run and a local OTLP collector:
  `turn` roots, `reason`/`act` hang off it, `llm.generation` sits under its
  `reason`, and `tool` under its `act`.
- Spans carry `gen_ai.*` attributes (operation, request/response model,
  provider, finish reasons, input/output/cache token counts, conversation and
  agent ids). They were empty because the values live in `data.metadata`, an
  object the generic `yolop.*` flattening skipped.
- The attribute names are copied from `everruns-core`'s `telemetry::gen_ai`
  with the source cited, not imported: that crate carries ~29 transitive
  dependencies, too much for a small extension binary. The copy can drift, so
  re-check it when bumping the everruns line.
- Cost has no convention and keeps the `yolop.` namespace.
- `reason.thinking.*` becomes its own span under its reason phase. Its phase
  (`thinking.started`) matched neither the started nor the terminal arm, so it
  had been dropped outright. It carries no span id, only the `exec_id` of the
  phase it ran in, so a phase now claims its exec id and a child without a
  parent id resolves through it. Only phases may claim it: an exec groups the
  phase and its children, so a child registering would displace the phase.
- The reasoning text itself is never exported. The Gen-AI conventions put
  content behind an opt-in this exporter does not have, so enabling the
  extension must not ship chain-of-thought to a tracing backend.

## 2026-08-20, The trace payload already carries the host's span tree

- [Extensions](specs/extensions.md): `trace/event` forwards the session event
  verbatim, so an extension sees exactly what an in-process `EventListener`
  sees, `context.parent_span_id` included. Confirmed by dumping a real run: all
  17 event types arrive, `llm.generation` parents to `reason`, `tool.*` to
  `act`, and `data.metadata` carries model, provider, token usage, and cost.
- `TraceEventParams` gained `span_id`/`parent_span_id`/`trace_id`/`turn_id`/
  `exec_id` plus `family`/`phase`. The tree was always on the wire; nothing
  named it, so the first exporter written against this facet re-derived a flat
  one from `turn_id` and every exported trace lost its nesting.
- `turn.*` is the root and carries no span id; its children name it by
  `turn_id`. An exporter mints the root span itself.
- Attribute names stay upstream in `everruns-core`'s `telemetry::gen_ai`. The
  SDK does not restate them and does not depend on that crate, so it stays
  serde-only; an exporter copies what it needs and cites the source.

## 2026-08-20, One extension name, however it is spelled

- [Extensions](specs/extensions.md): every by-name subcommand accepts the
  published crate name as well as the manifest name, matching what `install`
  already took. Installing `yolop-extension-logfire` and then enabling it by
  that same name had failed with "no extension named".
- The literal name always wins, so a package whose manifest really is
  `yolop-extension-foo` still resolves to itself, and an unknown name is passed
  through untouched so the error quotes what was typed.
- `disable` no longer reports success for a name that was never installed; it
  had written a persisted override for a package that does not exist. It stays
  more permissive than `enable` on purpose: a package whose manifest no longer
  parses is skipped by discovery and is exactly the one that needs switching
  off, so a directory on disk or an existing override is enough.

## 2026-08-20, Compact work keeps one mutable row per turn

- [Presentation](specs/presentation.md): `--compact-work` replaces live
  narration and tool transcript entries with one updating summary. The final
  assistant answer stays separate, and the session event log remains lossless.
- `Ctrl+O` expands or collapses retained details for the current or latest turn.
  Success, failure, and cancellation finalize to distinct summary markers.
- The mode is fullscreen-only. Split-footer rows become immutable native
  scrollback once published, so a real inline accordion could not reliably
  collapse historical details.

## 2026-08-20, One command per transcript line, and uninstall says uninstall

- [Extensions](specs/extensions.md): `remove` is aliased `uninstall`. Clap had
  answered `yolop extensions uninstall <name>` with "unrecognized subcommand"
  and a tip suggesting `install`, which does the opposite of what was asked.
- The transcript line is `{label}  {summary}`, and for `bash` both halves
  carried the command: the shell narration is already "Ran `<cmd>`", so the line
  read "Ran `cmd` `cmd` exit=0". The summary is now just `exit=<code>` when a
  narration carries the command, and keeps spelling it out when none does.
- The old test set narration to a bare "Ran Bash", a shape the real narrator
  never produces, so it could not see the duplication. It now builds its
  narration with `narrate_shell_exec`.

## 2026-08-20, Trace export keeps the root span

- [Extensions](specs/extensions.md): ending a session dropped the trace servers
  while `trace/event` notifications were still in flight, so `turn.completed`
  was lost. It closes the root span, so Logfire showed traces whose `reason`,
  `act`, `tool`, and `llm` children had no root, and a short run exported
  nothing at all.
- Every host path that ends a session flushes first. Stopping drains the events
  already buffered, then a `shutdown` request acts as the barrier: the stream is
  ordered, so its response proves the server handled everything queued before
  it. Bounded, so an unresponsive server cannot hold up exit.
- Draining on stop is the part that is easy to get wrong: signalling the
  forwarders without it drops exactly the events the flush exists to save.

## 2026-08-20, Install tells the truth about a package it cannot run

- [Extensions](specs/extensions.md): `install` resolves the declared
  `capabilityServer.command` and reports `server_command_found`. A crate
  published as source ships no binary, so the package installed cleanly and
  only `doctor` knew it could never spawn; the note now names the remedy.
- The bare-name shorthand prefixed unconditionally, so
  `install yolop-extension-logfire`, the name on crates.io, looked up
  `yolop-extension-yolop-extension-logfire`. It is idempotent now.
- Same parse: splitting `@<version>` before validating the name makes the
  documented `<name>@<version>` pin work. The guard had rejected `@` and the
  version's dots, leaving that branch's version split unreachable.
- `--acp` had lost its doc comment to `--config-dir`, so `--help` showed the
  flag blank and pasted the ACP paragraph in front of the config-dir text.
## 2026-08-19, Binary size becomes a maintenance surface

- [Maintenance](specs/maintenance.md) now owns binary size, with
  [`cargo-bsize`](https://github.com/Boshen/cargo-bsize) as the tool of record
  and an evidence bar of measured before and after on one target and profile.
- The shipped binary's shape decides which findings pay: about a third is
  tree-sitter parse tables from the 19 shared grammars, and yolop's own code is
  under 5%, so dependency features and profile settings are the levers, not
  `src/`.
- `panic = "abort"` and `strip = "symbols"` are rejected levers, not untried
  ones: the first breaks the crash path `join_worker` depends on, the second
  leaves crash-report backtraces unsymbolicated.
- The release profile moved to fat LTO at `opt-level = "s"`, measured at 70.2 MB
  from 89.8 MB (17.9 MB gzipped from 24.6 MB) for 19 seconds of build time.

## 2026-08-19, Extensions installed mid-session load without a restart

- [Extensions](specs/extensions.md): enabling a package installed during the
  session now registers it on the live runtime and activates it, so its tools,
  prompt, and hooks are usable on the next turn.
- Unblocked by upstream EVE-917 (`InProcessRuntime::register_capability` /
  `is_capability_registered`), which yolop requested after establishing that no
  host-side workaround existed.
- `BuildOptions::extensions_dir_override` lets a test inject an extensions
  directory instead of setting the process-wide `YOLOP_EXTENSIONS_DIR`, which a
  concurrently building session would otherwise pick up.

## 2026-08-19, everruns 0.20 cycle: live registration lands, A2A goes opt-in

- `everruns-host` 0.19.0 to 0.20.1, `everruns`, `everruns-platform`, and
  `everruns-llmsim` to 0.18.2.
- The breaking part of host 0.20 (`capability_registry()` returns an `Arc`
  snapshot, `capability_registry_mut()` removed) does not reach yolop: the one
  call site is the builder setter of the same name, not the getter.
- EVE-917 ships in this cycle, which is what lets an extension installed
  mid-session register on the live runtime.
- [Maintenance](specs/maintenance.md): outbound A2A delegation is now behind the
  `everruns` crate's opt-in `a2a` feature. Yolop has no outbound A2A path, so it
  stays off and the default build drops a second HTTP/TLS stack.

## 2026-08-19, Debug target size: one feature set, thin dependency debuginfo

- The routine checks ran `--all-features` while `cargo build`/`cargo run` ran
  the default set. Two feature sets in one `target/` are two graphs: 248 crates
  were compiled under both, and a directory carrying both reached 16 GB.
- [Local inference](specs/local-inference.md): records the debug-build cost the
  release table did not cover, and that no test is gated on `local-inference`,
  so running the suite with the engine on adds ~220 crates for no coverage.
- Routine commands now share `--features yolop-yep/schema`, which resolves to
  the same 519 crates as a default build.
- They also gained `--workspace`. Root `cargo test` covers the root package
  only, so the wire-schema drift guard in `yolop-yep` had never run outside
  CI's coverage job; the root commands now match what AGENTS.md claims of
  them, and run 1273 tests instead of 70.
- CI's `lint`, `test`, and `live-smoke` jobs share the `debug` cache and now
  agree on that set; `local-inference` became a clippy-only gate with its own
  cache key.
- `[profile.dev]` carries line tables instead of full DWARF, and build scripts
  carry none. Backtraces keep file and line, which with `RUST_LOG` is how this
  codebase is debugged; `--profile dev-debuginfo` opts back in to full DWARF
  for a debugger session.
- Together: a clean `cargo build --tests --workspace` went from 6.3 GB to
  3.7 GB, dependency rlibs from 2136 MiB to 1238 MiB, and the mixed-feature
  directory no longer happens at all.
## 2026-08-18, Mid-session installs report a restart, not a mystery

- [Extensions](specs/extensions.md): enabling an extension installed during the
  session surfaced the engine's "unknown capability: ext:<name>" plus "will load
  on the next session". Both the transcript and the `enable_extension` result now
  say the package arrived after startup and needs a restart.
- Registering a newly installed package into the live runtime is blocked
  upstream: `everruns-host` composes the capability registry once and offers no
  dynamic registration, and yolop holds the runtime as a shared `Arc`.
- Enabling with a cancelled required prompt no longer reports a bare success; the
  result names the unset fields.

## 2026-08-18, Eager bash schema, deferred LSP schemas

- [`tool_search`](specs/tool-search.md): `bash` joins the never-defer allowlist.
  Its deferred stub names no parameters while allowing extras, so a model fills
  the gap from other harnesses' shell schemas (`timeout`, `max_output_chars`)
  and argument validation rejects the call against the real
  `additionalProperties: false` schema, spending a round trip on a correction.
- LSP tools leave the allowlist and defer with every other opt-in surface. This
  reverses the profile an LSP adoption eval justified, so re-measure adoption
  before treating it as settled.

## 2026-08-18, Attached administration reports what it did

- [Extensions](specs/extensions.md): `yolop <subcommand> --help` failed inside a
  session, the parent parsed clap's help text as a control frame. A child that
  answers as an ordinary CLI is now relayed verbatim, which also covers
  `--version` and usage errors.
- A composed administration command (`... | cat`, redirection, quoting, `&`)
  now carries a notice that it ran detached and changed only global state. The
  attached and detached forms differ only by punctuation and their output was
  identical, so neither the agent nor the user could tell them apart.
- The notice rides the tool result, so one implementation serves the agent and
  the client transcript.

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

## 2026-08-18, Global directories are relocatable

- [Configuration](specs/configuration.md) now names two roots, config and data,
  and both move with `--config-dir` / `--data-dir` or `YOLOP_CONFIG_DIR` /
  `YOLOP_DATA_DIR`. `src/config/paths.rs` owns the resolution; every global path
  joins a leaf onto one of those roots instead of calling `dirs` itself.
- The reason for the change: isolating a run, or keeping several yolop
  identities side by side, previously meant moving `HOME`, which drags along
  everything else the process reads. An override names yolop's own directory, so
  no second `yolop` folder is appended to it.
- The flags are read from argv as the process's first act, not from clap
  matches: the crash reporter and the contributed-CLI registry both resolve
  paths before parsing finishes, so matches arrive too late to cover them.
  Other applications' directories are never redirected.

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

## 2026-08-19, Local inference gains GPU backends

- [Local inference](specs/local-inference.md) now defines `metal` and `cuda` as
  first-class features implying `local-inference`, replacing the earlier claim
  that accelerated backends were already opt-in; release binaries remain
  CPU-only.
- `--all-features` is now unusable on Linux at all, since it turns on both
  accelerated backends; every check names the features it wants.
- A macOS job compiles `metal` for both release targets so the accelerated
  backend cannot bitrot unnoticed; `cuda` has no runner and stays unbuilt.
- The engine's own CI job runs the ten feature-gated tests, which the coverage
  job cannot see because it never enables the feature.
- The default local model is a pre-quantized MoE GGUF. A default must be GGUF
  rather than safetensors, and its chat template must emit JSON tool calls
  inside `<tool_call>`; the engine cannot parse the `<function=…>` XML variant,
  which disqualifies Qwen3-Coder.

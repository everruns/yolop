---
type: Architecture Specification
title: Engine-owned execution
description: Defines the Engine adoption boundary, required upstream contracts, and the bar for retiring direct execution.
---

# Engine-owned execution

Status: target architecture. Yolop still constructs and drives the direct Core
runtime. This specification does not claim that execution has migrated.

## Decision

Yolop adopts the Everruns Framework `Engine` as the owner of session creation,
input admission, steering, execution, cancellation, recovery, and committed
outcomes. Every host, including linked child sessions, uses the same Engine
session boundary. The direct execution path is temporary and is removed when
the parity requirements below pass.

The migration cuts over directly to registered Engine sessions and coherent
SQLite hosting. New sessions and migrated sessions use that same execution
path. Yolop does not adopt the older local/custom-backend facade lane as an
intermediate runtime. The native authority owns admission, canonical events,
receipts, tasks, schedules and recovery from the first cutover.

Yolop owns decoding its saved format and orchestrating import under its session
lease. Everruns owns generic import and authority operations. Source adapters
may read old stores during migration, but cannot execute an old turn.

The reference is Everruns `main` at
[`310d402215897e3428ff026eed51e21a6ae4314e`](https://github.com/everruns/everruns/tree/310d402215897e3428ff026eed51e21a6ae4314e),
inspected on 2026-10-19 and locked in Cargo.lock. Its
[Engine-owned sessions decision](https://github.com/everruns/everruns/blob/310d402215897e3428ff026eed51e21a6ae4314e/knowledge/framework/engine-durable-sessions.md)
and [hosting implementation](https://github.com/everruns/everruns/blob/310d402215897e3428ff026eed51e21a6ae4314e/crates/everruns/src/engine/hosting.rs)
define the authority boundary. The earlier adoption probe used `93e72bca`.

Constructing an `Engine` does not establish coherent hosting: at the reference revision,
`Engine::create` selects the legacy facade actor when an Agent supplies
`backends` or `LocalConfig`. This is still Engine-managed execution. The stronger
durable profile uses registered behavior and a coherent hosting authority.
Durability claims identify the selected profile, including resumed sessions
and background callbacks.

A downstream executable probe against the earlier checkout verified registered
memory-hosted admission deduplication, conflicting-message rejection and a
single committed turn with an authoritative binding. The same probe verified
that registered hosting rejects Agent custom backends, while
`Engine::create(agent.backends(...))` executes without a hosting binding. This
qualifies the boundary distinction only, not Yolop adoption or process-loss
durability.

## Ownership

| Owner | Responsibility |
| --- | --- |
| Framework Agent and Harness | Trusted behavior, capability configuration, model and provider values, instructions and tool policy |
| Framework Engine and Session | Admission handles, active-turn steering, execution, lifecycle outcomes, cancellation and recovery |
| Coherent local hosting profile | Session bindings, canonical events, command receipts, runnable state and epoch-fenced writes in one authority |
| Framework Environment and workspace head | Workspace identity and access, filesystem, compute and containment services |
| Yolop | CLI, TUI and ACP presentation, configuration, approval interaction, coding capabilities, coordination and workspace checkpoint policy |

Yolop does not implement another planner, actor driver, turn runner or authority
to make the migration possible. Missing framework contracts land in Everruns.
Host-specific integration may remain below the application facade, but it
supplies services to Engine and cannot start a direct turn.

## Current integration boundaries

The current [runtime construction](../../src/runtime/mod.rs) owns a curated
`HostComposition`, `InProcessRuntimeBuilder`, JSONL event authority, local task
and schedule stores, provider selection, MCP authentication and filesystem
routing. `RuntimeHandles::run_checkpointed_turn` starts foreground turns;
the [wake runner](../../src/runtime/background_wake.rs) starts child turns.
The [session facade](../../src/runtime/session.rs), print host and ACP share
the foreground handles. Both execution entrypoints must migrate.

These are behavior requirements, rather than arguments for carrying the old
runtime into Engine:

| Existing behavior | Required Engine integration |
| --- | --- |
| Custom capability pre-tool hooks, model views, context contribution and commands | Supported capability/host-service composition with the same validation, prompt ordering and policy |
| Live provider/model/effort changes and deferred configuration persistence | Authority-ordered session configuration and host-owned credentials, with strict selection and no fallback |
| Workspace, private session files, scoped skills and repointable worktrees | Reopenable Environment preserving the current filesystem and containment boundaries |
| MCP OAuth and live server reload | Host-owned auth resolution and authority-ordered scoped configuration |
| Extension installation and activation during a session | Separate registration and session activation through supported Engine services |
| Background tools, monitors and subagents | Existing task/schedule semantics attached to Engine sessions, with durable delivery identities |
| Event streaming, context inspection, commands, completion review and tracing | Engine queries and observers over the same canonical history; observation cannot drive execution |
| App/version metadata and OpenRouter attribution | Safe host request metadata preserved across Engine runtime reconstruction |
| Bounded, tool-less completion review | Explicit review instructions, bounded evidence and purpose metadata using the Engine session's provider |
| Rewind, redo and compaction checkpoints | Authority-supported history branching and matching compaction state |

At the reference revision the coherent hosting path rejects Agent custom
backends and legacy local configuration. `HostServices` now composes Core
capability hooks/model views, host providers, recovery policy and MCP auth into
Engine-created runtimes. Session controls cover commands, capability overlays,
live model/effort/MCP changes and command-context queries. These APIs remove
the need to expose `Agent::runtime_builder` or a raw runtime handle.

HostServices now carries request metadata and attribution, and Session exposes
explicit tool-less completion requests. These preserve Yolop's completion review
instructions and bounded evidence without reconstructing a command host.

The remaining history contract is generic authority-owned branch selection.
The native importer currently reads a flat journal and the authority retains
one compaction replacement per provider/model/format. Importing Yolop's whole
log would revive abandoned messages; importing only its selected branch would
lose redo. The authority must retain complete history and atomically select the
conversation lineage and matching compaction state.

### Upstream contracts

Framework changes are tracked in the Everruns OSS project:

| Contract | Tracking issue | Milestone |
| --- | --- | --- |
| Capability/provider services and policy-preserving composition | [EVE-1296](https://linear.app/everruns/issue/EVE-1296) | Available on pinned main |
| Session command/query surface and live controls | [EVE-1297](https://linear.app/everruns/issue/EVE-1297) | Available on pinned main |
| Older facade session-overlay preservation | [EVE-1299](https://linear.app/everruns/issue/EVE-1299) | Historical bug fix, not a direct-cutover prerequisite |
| Host request metadata and provider attribution | [EVE-1302](https://linear.app/everruns/issue/EVE-1302) | Available on pinned main |
| Explicit tool-less completion request | [EVE-1303](https://linear.app/everruns/issue/EVE-1303) | Available on pinned main |
| Native authority import, local services and recovery | [EVE-1298](https://linear.app/everruns/issue/EVE-1298) | Available on pinned main |
| Conversation branch selection and compaction lineage | [EVE-1305](https://linear.app/everruns/issue/EVE-1305) | Direct cutover |

## Durable local state

For coherent durable hosting, use one transactional authority for admission, receipts, canonical events,
ownership and runnable work. Prefer extending and qualifying the Framework's
SQLite hosting profile over introducing a Yolop-specific authority. A startup
supervisor must rediscover accepted runnable work independently of a client
request. The existing task/schedule stores do not by themselves establish that
guarantee for session execution.

`events.jsonl` may remain an export and compatibility projection. It cannot
remain an independent canonical writer beside the Engine authority. Lossy
observer callbacks cannot be the sole source of resumable history. Rebuildable
exports track a committed cursor and recover any missing suffix from bounded
canonical reads.

The [Yolop source reader](../../src/runtime/migration.rs) decodes the complete
journal, timeline, workspace metadata, compaction replacements and compaction
attempts. It preserves abandoned branches, redo targets, checkpoint refs and
original event/message identities. A source fingerprint detects changed import
inputs. The source holds the existing session lease until dropped. Migration
reads refuse malformed, foreign, duplicate or out-of-order
records instead of applying ordinary replay's tolerant skipping. They never
repair or truncate the source. `yolop sessions migration-check` exposes a
bounded source preflight; it performs no ownership transfer.

When retained sessions migrate to the new authority, import preserves session
identity, active and abandoned conversation
branches, tool call/result associations, workspace metadata, task links and
compaction state. Import is idempotent and records its source boundary before
the new authority owns further admission. Retained sessions remain writable and
retain full rewind support; read-only archival is not the cutover policy.
Interrupted or conflicting imports
fail visibly and cannot start execution through the old path. The upstream
message-only seed operation is insufficient for this lossless import contract.

Conversation rewind is an authority operation performed while execution is
quiescent. It changes the active history and compatible compaction state
together, preserves abandoned history, and resolves outstanding admission and
wait state explicitly. Filtering a JSONL view below an unaware actor is unsafe.
Workspace snapshots and destructive restore confirmation retain the existing
[checkpointing](checkpointing.md) policy.

## Admission and host lifecycle

Each user input, automatic wake and child instruction has a retained command
identity. A successful send means admission under the hosting profile's declared
durability. A turn identity is used only after Engine reports its assignment;
acceptance and completion are separate UI states. Hosts cannot turn a lost
response into another input by inventing a replacement command identity.

Engine serializes input and classifies steering. Host locks may protect UI or
checkpoint work but cannot create a second scheduling authority. Disconnecting
a TUI or ACP frontend does not by itself cancel admitted work. Explicit cancel
uses Engine's targeted operation and waits for a committed result. Completion
review, recovery budgets and failed-turn handling remain shared across hosts.

Task handoffs retain the provenance, coalescing and already-observed suppression
defined by [background execution](background.md). Durable task and schedule
delivery reaches Engine admission with a stable identity. A child is another
registered Engine session with the inherited safety boundary, not a call back
into `InProcessRuntime::run_turn`. Coordination across processes retains its
existing task ownership and cancellation semantics.

## Adoption and retirement gate

Implementation order follows ownership dependencies:

1. Add the missing generic capability, command/query and local-service
   composition contracts in Everruns so Engine can own execution without
   dropping Yolop policy. The first cutover moves admission and storage into
   the native authority, including retained conversation lineage.
2. During this migration, the entire Everruns family consumes the upstream
   git `main` source at one Cargo.lock revision, explicitly authorized for this
   work. This avoids mixed registry/git type identities while using the merged
   Engine APIs before publication. Replace this work dependency with aligned
   published pins before preparing a crates.io release of Yolop.
3. Replace runtime construction with Agent, Harness, Engine and Session
   composition on registered SQLite hosting. Preserve capability policy,
   retained sessions and Environment boundaries.
4. Route TUI, inline, print, ACP, automatic wakes, monitors and child turns
   through Engine. Move steering, cancellation and interruption recovery with
   execution, rather than leaving those on the old runtime.
5. Remove direct turn calls, the direct runtime builder and the legacy execution
   selection once parity passes. Add a source/dependency guard against
   reintroducing a bypass. Core may remain a dependency for host services;
   retirement concerns execution ownership, not the crate name.

Qualify the direct transition under coherent SQLite hosting with real
process-loss and stale-writer probes before changing durability claims. The
older facade lane is not a runtime selection or a fallback in Yolop.

Parity requires evidence for every host and the following failure boundaries:

- Scripted and live model turns retain effective instructions, tool schemas,
  validation, approvals, containment, result artifacts and failure outcomes.
- Active-turn steering and cancellation target the correct admitted input;
  repeated IDs cannot authorize another tool call or another turn.
- Under coherent durable hosting, lost admission responses and process death recover without duplicate input
  or automatic replay of uncertain external side effects. Expired epochs
  cannot write. Unknown or unavailable retained builds fail visibly.
- Background completion, monitor firing, child results and coordination
  handoffs remain parent obligations and are delivered without duplicate turns.
- Resume, import retry, rewind, redo and compaction retain their current
  conversation/workspace semantics, including crash recovery during cutover.
- Model changes, MCP reload and extension activation affect the intended next
  operation without widening permissions or silently changing the model.
- Canonical history, exported JSONL, CLI output, TUI, ACP and trace consumers
  agree on accepted, running, waiting, canceled, failed and completed states.

An internal comparison switch may be used while qualifying the transition.
Automatic fallback on Engine failure is prohibited: it can admit or execute
work twice. Deprecating the direct path requires all parity evidence; deleting
it additionally requires a working retained-session policy. Retiring the older
Engine facade lane upstream does not require Yolop to run through it first.

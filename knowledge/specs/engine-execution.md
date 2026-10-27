---
type: Architecture Specification
title: Engine-owned execution
description: Defines native Engine execution, retained-session import, and local authority ownership.
---

# Engine-owned execution

Yolop is the primary coding-agent demonstration of Everruns. Framework adoption
preserves all product features while replacing application-owned execution
plumbing with public framework primitives.

Yolop executes registered Everruns `Engine` sessions on `SqliteHosting`.
Foreground TUI, inline, print, ACP, automatic inputs and linked subagents use
this authority. Runtime construction supplies Agent, Harness, HostServices and
a reopenable Environment. It does not construct or drive a Core runtime.

The Everruns family consumes git `main`, locked together at
[`2206d44e16c2eb5f4de8792c8b4c63dee00246cd`](https://github.com/everruns/everruns/tree/2206d44e16c2eb5f4de8792c8b4c63dee00246cd).
This source exception is authorized for Engine adoption. A crates.io release
must return all family dependencies to aligned published versions first.

## Ownership

| Owner | Responsibility |
| --- | --- |
| Framework Agent and Harness | Trusted instructions, capabilities, model and tool policy |
| Engine and Session | Admission, steering, execution, targeted cancellation and outcomes |
| SQLite hosting | Bindings, canonical events, receipts, runnable work, tasks, schedules and epoch fencing |
| Environment and workspace head | Stable workspace identity and access through Yolop's filesystem |
| Yolop | Frontends, credentials, approvals, coding capabilities, coordination and workspace checkpoints |

Frontends hold a native `Session` for execution, typed admission observation,
cancellation, capability activation, commands and context inspection. Native
`Turn` outcomes, including hook failures, reach every frontend unchanged except
for product-facing provider error detail. There is no global run registry or
Core runtime facade.

The application host in [engine.rs](../../src/runtime/engine.rs) owns model
selection policy, checkpoint hooks, supervisor lifetime and the presentation
cache. It contains no Core runtime handle or writable conversation store.
Command descriptors are translated only at the UI and capability boundaries
where Yolop composes native commands with local skill commands. Scoped MCP
configuration is supplied by its host capability and resolved again for each
model context.
Tool-less completion review uses the Framework's explicit completion request.
HostServices preserves capability hooks, provider recovery, MCP auth and request
metadata. Dynamic capability registration is distinct from session activation.

The Environment retains the logical workspace head across reconstruction and
exposes Yolop's repointable filesystem, scoped skills and private artifacts.
Linked children share that workspace and have separate conversation authority.
Workspace metadata is durable before foreground admission, so recovery does not
rely on the resuming process's working directory. Workspace policy is explicit.
Model-facing logical paths and frontend physical
display paths remain separate filesystem contracts.

## Local authority and process lifetime

`sessions/engine.sqlite` is the shared native authority. `engine.key` is its
stable private encryption key; losing it refuses startup rather than replacing
keys for an existing database. Concurrent first opens serialize key creation.
Back up both files together. Credentials stay in the existing host stores.

A retained native supervisor discovers runnable work independently of frontend
requests. The hosting adapter delegates consistency ports to SqliteHosting and
claims only the retained build and linked-session group belonging to this
process's workspace. Coordination threads belong to their separate hosts.
Unknown builds and unavailable capabilities fail visibly, without fallback.
The root binding retains its behavior descriptor and compatibility build ID;
a descendant resolves that descriptor through its parent bindings.

`events.jsonl` is a rebuildable raw authority export, including abandoned
branches. It is not a second canonical writer. Resume bootstraps its transcript
from active canonical history, even if the export is stale or empty. Frontend
event observation is lossy and catches up with bounded authority reads. Product budget diagnostics
live separately in `host-events.jsonl` without canonical sequence numbers and
are filtered against active turn lineage after rewind.

Frontend streams correlate inputs with proven Engine turn assignments. Cancel
waits for the selected run or admission's cancellation outcome before the next
request proceeds. Dropping an explicit foreground waiter cancels that run;
automatically admitted work is executed by the supervisor. UI wake channels
observe accepted work rather than authorizing it. Startup and lifecycle reads
recover pending admission notifications missed by a lossy observer.

## Saved-session cutover

Yolop decodes its old format under the same exclusive session file lease used
by execution. The [source reader](../../src/runtime/migration.rs) preserves event
and message identities, sequence gaps, checkpoint branches, redo and compatible
compaction replacements. Malformed, duplicate, out-of-order or unlinked foreign
records refuse import. No tolerant replay or source repair can silently discard
history during ownership transfer.

Older subagent events can share the root JSONL and sequence allocator. Retained
task links or successful subagent tool results establish their parentage. Import
separates registered child histories. Every imported journal retains its exact
source bytes in private `legacy-events.jsonl` before rebuilding the root export. Child imports are
retryable; the root moves after its children. Original compaction-attempt and
workspace files remain local product metadata.

Source-only legacy backend descriptors let the Framework read old tasks,
schedules and storage during import. They are constructed only for a legacy
source, never during fresh or native-session startup, and never execute a
legacy turn. Coordination retains its separate product database. Native
cutover records its boundary and ownership atomically. Repeating the same
import replays that decision. Live legacy workers or conflicting state refuse
cutover. Once imported, every new input uses the native authority.

`yolop sessions migration-check <session-id>` is a read-only file preflight. It
reports counts and a fingerprint without content or ownership transfer. Runtime
import additionally reads retained task links, including interrupted children
whose final tool result was never written.

Conversation rewind uses authority-owned history selection and compaction
lineage. Yolop owns Git snapshots, preview validation and the durable restore
intent coordinating the two stores. See [checkpointing](checkpointing.md).

## Upstream contracts and limits

The required host services, session controls, import, completion requests and
history lineage contracts are available on the pinned main. The earlier facade
session-overlay fix, EVE-1299, is historical and is not a cutover dependency.

The pinned framework includes both native service corrections:

- [EVE-1309](https://linear.app/everruns/issue/EVE-1309): schedule delivery retains
  a stable occurrence admission identity across delivery and acknowledgement
  retries.
- [EVE-1310](https://linear.app/everruns/issue/EVE-1310): native child waits report
  the actual terminal turn outcome and settle foreground subagent tasks. Yolop
  delegates this behavior directly to the framework.

Background completion admission uses a stable ID derived from session, task,
attempt and terminal state. Engine can steer a fast completion into an active
turn. Frontends then show its notice without starting another turn. See
[background execution](background.md) for notification and restart semantics.

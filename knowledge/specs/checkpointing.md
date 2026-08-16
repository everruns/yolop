---
type: Product Specification
title: Session checkpointing and rewind
description: Defines the session checkpointing and rewind contract for Yolop.
---

# Session checkpointing and rewind

Status: implemented (v1).

## Why

Yolop persists a resumable conversation and normally gives implementation
sessions an isolated Git worktree. Neither property is an undo mechanism. A bad
turn remains in the model's context, and its filesystem changes remain in the
worktree. Asking the model to reverse itself is lossy because it depends on the
same context that may be wrong and cannot reliably reconstruct deleted or
overwritten content.

Checkpointing makes every user-turn boundary a durable recovery point. A user
can discard an unproductive conversational branch, restore the corresponding
workspace, or do both, then edit and resend the original prompt.

## Product model

A checkpoint is the state immediately **before** a user or automatic turn:

- the active conversation head;
- an exact sequence boundary in the append-only session log;
- a workspace snapshot when the session owns an active Git worktree;
- the prompt that is about to run, for list labels and input restoration;
- enough metadata to preview and validate a restore.

Checkpoints are automatic. They are not commits in the user's branch and are
not a replacement for Git history.

### User actions

- `/rewind` lists checkpoints newest first. `/rewind <id> [mode]` previews:
  - **Conversation and workspace** (default when a complete workspace snapshot
    is available);
  - **Conversation only**;
  - **Workspace only**;
  - cancellation by simply not confirming.
- `/undo` is a shortcut for rewinding both states to before the most recent
  completed turn. It still shows a destructive preview when files would change.
- `/redo` restores the branch most recently abandoned by `/undo` or `/rewind`.
  It is available until a new turn starts. Starting a new turn clears the redo
  shortcut but does not delete the abandoned branch.
- Natural-language requests such as "undo the last turn" use the same controller
  through a model-facing tool. The tool may request a preview, but it must not
  bypass the same safety checks as the slash command.

After conversation rewind, the selected turn's original prompt is placed in the
input editor but is not submitted. The user may edit, discard, or resend it.
The active transcript is replaced with the selected branch and a restore notice.
Inline mode cannot erase bytes already emitted to the terminal's scrollback,
but subsequent rendering and model context use only the selected branch.
`/clear` remains display-only and is unrelated to rewind.

## Semantics

Conversation and workspace are independent axes. Restoring one never silently
implies the other. The UI always names the state it will change.

| Action | Model-visible history | Transcript | Workspace files |
|---|---|---|---|
| Conversation only | selected ancestor branch | selected ancestor branch | unchanged |
| Workspace only | unchanged | unchanged plus restore notice | selected snapshot |
| Both | selected ancestor branch | selected ancestor branch | selected snapshot |

Rewind does not delete history. It moves the active head to an ancestor and
preserves the abandoned suffix as a branch. This makes redo and crash-safe
resume possible without rewriting `events.jsonl`.

Workspace restore means file contents, paths, executable bits, and symlinks in
the captured worktree. It deliberately does **not** rewrite the worktree branch,
`HEAD`, commits, user refs, reflogs, submodule repositories, ignored files,
write-blocklisted build/dependency directories, or external
side effects such as databases, services, deployed resources, and commands run
outside the active root. Empty directories are not captured. These exclusions
are a stable part of the workspace snapshot contract.

If commits were created after a checkpoint, restoring files leaves those commits
intact and produces ordinary working-tree differences against the current
`HEAD`. The user can recover either state with Git or `/redo`.

## Storage

The existing per-session directory gains:

```text
<session>/
  events.jsonl                 # immutable event payloads, as today
  timeline.jsonl               # append-only checkpoint/head/restore records
  compaction-checkpoints.jsonl # append-only model-context replacements
  workspace.json               # existing session/worktree metadata
```

Workspace snapshots use Git objects in the session worktree's repository,
anchored by private refs:

```text
refs/yolop/checkpoints/<session-id>/<checkpoint-id>
```

Creating a snapshot uses a temporary index and plumbing commands (`add -A`,
`write-tree`, `update-ref`). It does not change the real index, working tree,
current branch, or `HEAD`. Each checkpoint ref points directly to a tree object;
Git's object store deduplicates content. Ordinary Git machinery calculates
previews. Untracked non-ignored files are included; ignored and write-blocklisted
paths are not. Checkpoint refs are removed when their worktree is pruned, leaving
normal Git GC to reclaim unreachable objects.

Each timeline record is JSONL and owner-only. Relevant record types are:

- `checkpoint.created`: checkpoint id, parent checkpoint, event sequence before
  the turn, prompt, snapshot tree/ref, and any capture error;
- `turn.completed`: checkpoint id and final event sequence/status;
- `head.moved`: old head, new head, abandoned head, and recovery snapshot;
- `redo.cleared`: the abandoned head is no longer the implicit redo target.

The journal is append-only and flushed before the turn begins or a restore is
reported successful. A compact derived index may be added later, but it is never
the source of truth.

### Model-context checkpoints

Model-context compaction is a separate, derived projection over the lossless
event log. A successful native compaction appends the provider/model, source
event sequence, format version, and provider-opaque replacement to
`compaction-checkpoints.jsonl`. Subsequent calls reconstruct model input as the
latest compatible checkpoint on the active timeline plus raw messages after its
source boundary. The raw events are never rewritten or deleted.

Proactive replacement is triggered by either context-window pressure or prompt
cost pressure. Cost pressure combines bounded cumulative uncached input with
the saturating byte count of raw tool results accumulated since the compatible
checkpoint, and requires a non-trivial current prompt so short follow-ups do not
compact unnecessarily. Both triggers use the same provider-native replacement,
lineage-aware retry watermark, suffix re-arming, and atomic failure fallback;
there is no cost-only checkpoint format or replay path. Cold bulky evidence can
therefore leave the live model view while remaining lossless and retrievable
through `query_history`, with the active raw suffix retaining recent asks,
decisions, edits, and validation.

A durable model-context checkpoint may use an event boundary already persisted
inside the currently open turn. That pending turn is the active head extension
until completion, but the timeline rejects boundaries beyond the event log's
current maximum; finishing, rewinding, and redoing then apply the ordinary
closed-node lineage rules.

Rewind and redo select checkpoints by the active timeline. A checkpoint on an
abandoned suffix remains append-only data but cannot shadow the latest surviving
ancestor. Installation is monotonic for each provider/model/version, and a
failed or ineffective compact call leaves the prior checkpoint unchanged. On
Unix the file is owner-only (`0o600`) because provider-native replacements may
contain retained message text and encrypted opaque context; those payloads must
not enter public events or logs.

## Conversation replay

`events.jsonl` remains the durable event payload store. `timeline.jsonl`
describes which event ranges form the active branch.

On a new turn, Yolop:

1. validates the requested model when its provider exposes model discovery;
2. creates and flushes the checkpoint;
3. records its parent as the current conversation head;
4. runs the turn and appends events normally;
5. records the turn's final sequence and status;
6. advances the active head.

A failed model preflight does not create a checkpoint or append the ask. Once a
provider request begins, everruns-runtime owns bounded transient recovery inside
the active reason phase. Yolop installs a stall liveness window and a recovery
elapsed budget large enough to absorb full stall retries (upstream's default
elapsed budget is shorter than one stall window). Retries reuse the persisted ask
and completed tool outputs; they never create another checkpoint or re-run a
completed tool.

On resume, Yolop validates the timeline, walks parents from the active head, and
replays only those event ranges into the event bus, message store, and transcript.
Abandoned events remain in the raw session log and therefore remain discoverable
through session search, but never enter model context unless that branch is
restored.

The live runtime retains the concrete in-memory message store handle. A
conversation restore reseeds it from the materialized branch and replaces the
host's active transcript from the same event set. No process restart is
required. The event sequence remains globally monotonic across all branches.

Compaction and opaque provider reasoning artifacts belong to their turn range.
Rolling back past them removes them from active replay; Yolop must not reuse a
provider continuation id whose ancestor messages are no longer active. The next
turn starts a fresh provider continuation from the materialized messages.

## Workspace restore

Restore is a small transaction coordinated by a session-scoped lock:

1. Refuse while a foreground turn, shell command, scheduled wake, or background
   task can still write the workspace. The user may cancel those tasks first.
2. Capture the current workspace as a durable recovery checkpoint. If this
   fails, make no changes.
3. Compare current files with the target snapshot and show the paths that will
   change. Confirmation is required for every restore.
4. After confirmation, materialize through a private Git index and replace or
   delete captured paths under the active root. Reject path traversal and
   write-blocklisted paths.
5. Verify the resulting captured tree id. Only then append `head.moved` and
   update the conversation head when the selected mode includes conversation.
6. On failure, attempt to restore the recovery checkpoint and keep the journal
   head unchanged.

The preview and token confirmation are mandatory, including for a no-op restore.
The model-facing tool can only queue confirmation; mutation occurs after its
current turn has completed.

### Ownership boundary

Version 1 offers workspace snapshots only for a Yolop-owned worktree. This is
the key safety boundary: restoring the whole captured state cannot erase edits
from the user's primary checkout or another session.

When worktrees are off, checkpoint listings label entries conversation-only and
workspace restore explains why it is unavailable. A later version may add a shadow repository
and three-way conflict handling for shared or non-Git directories, but it must
not weaken version 1's guarantee or silently restore only edit-tool changes.

## Partial and failed checkpoints

A turn is allowed to continue when workspace capture fails, but its checkpoint
is labeled `conversation only` with the reason. Typical reasons include a missing
Git repository, repository lock, unsupported path encoding, or unreadable file.
Yolop never presents a partial snapshot as an exact workspace restore.

Interrupted/failed turns still have checkpoints and are rewindable. If a crash
occurs after the workspace snapshot but before the turn-completion record, resume
closes the turn as interrupted at the last persisted event sequence. If a crash
occurs during restore, the absence of `head.moved` leaves the prior conversation
head authoritative and the private recovery ref available for manual recovery.

Malformed timeline records are skipped only at the corrupt tail. Corruption in
the committed prefix disables mutation and leaves session replay read-only until
the user repairs or forks the session; silently guessing a branch is unsafe.

## Retention and privacy

- Version 1 retains automatic checkpoints for the life of the session worktree.
  `yolop worktree prune` removes their private refs; Git object cleanup follows
  repository GC policy.
- Prompts remain in the existing private event log and timeline, both protected
  with owner-only permissions on Unix.
- Snapshotting honors Git ignores. Yolop does not force-add ignored secrets.
- `yolop worktree prune --dry-run` reports checkpoint refs it would detach.

## Host surface

The controller is host-independent; presentation is not:

- TUI: `/rewind`, `/undo`, and `/redo` produce textual previews and confirmation
  tokens; a conversation restore puts the original prompt back in the composer.
- ACP: advertise system commands through the existing
  `available_commands_update` path. Because ACP has no Yolop-specific rewind
  picker, restoration is a two-step command: the first invocation returns a
  preview plus a confirmation token; the second supplies that token.
  Do not expose TUI-only effects over ACP.
- `--print`: the same model-facing tool is available. A restore still requires
  the explicit preview/confirmation sequence and is applied only after the turn.

The command palette remains sourced from capability descriptors. All front-ends
and the model-facing tool call one `CheckpointController`; restore logic must not
be duplicated in the UI.

## Fit with the current architecture

Version 1 is implemented inside Yolop without an Everruns protocol change:

- `src/runtime/session_log.rs` already owns append-only event persistence, replay
  filtering, sequence continuity, private permissions, tail repair, and the
  single-writer file lock. Timeline materialization belongs beside it.
- Runtime construction already creates a concrete `InMemoryMessageRetriever`
  and seeds it on resume. Retaining its `Arc` in `RuntimeHandles` is sufficient
  to clear/reseed live model history after rewind; no new message-store trait
  method is required.
- `App` owns the mutable transcript vector and replaces it from the active event
  set after a restore.
- `WorktreeManager::worktree_info()` identifies an active session worktree and
  exposes its path. Snapshot creation can use ordinary Git plumbing against that
  path without changing the runtime file-store abstraction.
- TUI and ACP foreground turns are serialized at host boundaries. The retained
  `SessionTaskRegistry` rejects restore while descendant tasks are active, and
  stale-preview verification rejects external edits made before confirmation.
- ACP already advertises and dispatches capability commands. Preview/confirm can
  therefore be expressed as command results without extending ACP.

The main implementation is Yolop state coordination, not a missing platform
capability. A future shared-checkout/non-Git restore mode may justify a shadow
repository, and richer native ACP UI would require ACP support.

## Acceptance criteria

- After three turns, rewinding to turn one and resuming sends the model no
  messages, tool results, or reasoning artifacts from turns two and three.
- Resume after process restart materializes the same active branch.
- Combined rewind restores tracked and untracked non-ignored,
  non-write-blocklisted files exactly,
  including creations, deletions, renames, binary content, symlinks, and modes,
  even when the changes came from shell commands.
- Restore never changes `HEAD`, branch refs, commits, or the user's real index.
- A recovery checkpoint makes every successful restore redoable.
- Workspace restore is unavailable outside the session-owned worktree in v1.
- Active writers block restore; a failed capture or preview causes no mutation.
- Abandoned branches remain durable but absent from model context.
- Session and worktree pruning remove checkpoint refs without touching user refs.

## Prior art

- [Claude Code checkpointing](https://code.claude.com/docs/en/checkpointing)
  exposes separate conversation/code/both restores, but documents that shell and
  external changes are not tracked.
- [Gemini CLI checkpointing](https://google-gemini.github.io/gemini-cli/docs/cli/checkpointing.html)
  stores a Git snapshot, conversation, and pending tool call in a shadow repo.
- [Cline checkpoints](https://www.mintlify.com/cline/cline/core-workflows/checkpoints)
  use shadow Git and expose files/task/both restore modes.
- [Cursor checkpoints](https://docs.cursor.com/en/agent/chat/checkpoints) are
  local, automatic snapshots of agent changes and explicitly not version control.
- [Aider Git integration](https://aider.chat/docs/git.html) auto-commits agent
  changes and implements `/undo` through Git history.
- [Codex app-server `thread/rollback`](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
  records a rollback marker and prunes model-visible turns without deleting the
  rollout; its documented endpoint is deprecated and does not restore files.

## Related

- [`session-history.md`](./session-history.md), local session discovery.
- [`worktrees.md`](./worktrees.md), session-owned workspace isolation.
- [`commands.md`](./commands.md), command registry and host routing.
- [`conversational-control.md`](./conversational-control.md), natural-language
  access to user-facing controls.

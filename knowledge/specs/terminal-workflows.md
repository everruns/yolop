---
type: Product Specification
title: Terminal workflow views
description: Review, recovery, history, draft context, and action discovery in the terminal host.
---

# Terminal workflow views

Workflow views compose tuika's existing Boxed, SelectList, Scroll, Image, and
focus routing. Yolop owns rows, actions, acquisition, and meaning. The
terminal-independent workflow presentation owns visible labels, errors,
previews, and choices. Toolkit behavior stays upstream.

## Inspection and navigation

`/diff`, `/timeline`, `/sessions`, `/attachments`, `/context`, `/actions`,
`/find`, and `/recover` are client capability commands, also reachable through
`run_command`. Read-only workflow views can open during a turn without adding
a separate queue mode. Runtime mutations remain unavailable from the palette
until the foreground turn settles. One ownership table routes both keys and
pastes. Setup, questions, and hard approval take priority over workflow views.

Changes dock at wide widths and become a focused sheet at narrow widths. Ctrl+X P
returns to the intact composer. A passive narrow panel is hidden, never a
zero-sized focused surface. Mouse input over a view cannot affect the transcript
underneath it.

Git queries are asynchronous, cancellable, time-limited, and output-limited.
External diff, text conversion, pagers, and filesystem monitors are disabled.
References resolve to verified object IDs; paths use literal pathspecs.
Diff paths and scope stay relative to the active workspace, including a
subdirectory of a larger repository.
Comparison-base changes validate asynchronously before replacing the previous
base. Unsupported filename encodings are counted and skipped without hiding
the remaining changes.
Working-tree comparisons include index, disk, and untracked changes. Session
and last-turn scopes follow surviving checkpoint lineage, including a pending
turn's snapshot. Missing anchors never silently mean HEAD.

## Restoration and session changes

The visual timeline shares CheckpointManager with textual commands and the
model tool. Conversation, workspace, and combined restores are separate choices.
A preview changes no active history or workspace. A second Enter confirms its
token; cancellation and view disposal invalidate it, including abandoned pending
loads and terminal exit. Writer fencing, owned-worktree restrictions,
recovery snapshots, external-effect exclusions, and redo retain the checkpoint
contract. Outcomes derive from recorded turn completions.

Session search reuses bounded read-only discovery and also matches saved titles,
workspace paths, and branches. It previews recorded messages and saved summaries
without equating historical failure with current process state. A target lease
check precedes handoff; failure preserves the current session. Handoff resumes
the target's saved workspace instead of carrying the current run's cwd override.

Composer text, paste payloads, and images form session-private `draft.json`,
atomically replaced with owner-only permissions and a 32 MiB bound. Normal exit
and switching save it; an empty draft removes it. External editing tears down
and re-enters the existing terminal lifecycle around the same App, never submits
text, and preserves the draft on editor failure.

## Context, approvals, and recovery

Explicit file snapshots, optionally selected by line range, are bounded and
workspace-contained. Symlinks and special files are refused. The tray previews
or removes the payload the composer actually submits. Images use tuika graphics
support and a readable fallback.

Activity phases derive from runtime event types. A quiet command remains running.
Retry/check times appear only when reported by the engine; the activity rail
retains scheduled-work cancellation. Recovery continuation instructs the agent
to inspect state before retrying, never automatically replays a command.

Hard approval names the action, reason, and one-request versus whole-scope
session grant. Only unmodified approval keys or Shift variants authorize.
Pasted text never does. Context occupancy, process token usage, cost, and
subscription allowance are distinct. Unknown values are labelled unknown.
Compaction changes the model projection while retaining authoritative history.

## Evidence, context and background inspection

`/checks` projects canonical completed validation calls, including bounded output
references. New foreground and background Bash checks stamp Git-visible content
before and after execution. Freshness requires both stamps and a matching current
workspace; changed or unavailable evidence never becomes a verified current pass.
Ignored files, submodules and external dependencies are outside that claim.
Evidence survives output-envelope truncation. Private artifact readers reject
symlinks and traversal and cap both reference traversal and text tails.

`/tasks` uses the existing task tree and native registry. Identity is owner session
plus task ID. Messages go through the native message-task service, cancellation
through the existing task cancel control, and inspection reads bounded child
transcripts, durable messages and private artifacts. Refresh preserves stable
selection and reading position. Output filtering, live pause and tail following
are host state, never a second task lifecycle.

Context inspection uses the runtime's assembled next-request context rather than
rescanning disk. It distinguishes the skill catalog from retained activated skill
bodies, instructions from source-tagged file contributions, filtered history from
unsent draft attachments, and estimates from provider-reported occupancy. Previews
are bounded and image payloads are omitted from text views.

`/files` discovers Git-visible or ignore-filtered workspace files with bounded
counts. Fuzzy matching and numeric range syntax choose an attachment; the same
safe snapshot reader serves preview and submit. `/btw` runs the native read-only
side query asynchronously in a cancellable overlay, without steering or changing
canonical history. Copy or attach are explicit host actions.

## Attention, access and dialog visibility

The host inbox is bounded and deduplicated from canonical task transitions and
foreground lifecycle events. Acknowledgement does not forget event identity.
Unread counts stay visible in composer guidance. Desktop alerts are opt-in per
process and generic, only new events with known lost focus notify. Tuika owns the
OSC encoders and focus-mode guard. Yolop owns consent, deduplication and privacy.
Focus reporting is suspended for terminal teardown and external editor handoff.

Session pin, rename and archive state lives in private atomic `ui.json` files,
not canonical history. Invalid labels do not hide other sessions. Live activity
uses the existing exclusive lease. Archive filters and project grouping are view
state. The permissions inspector reads the active provider and actual process
session grants; revocation applies to future requests only.

Dialog input ownership follows visible modal priority. Optional chrome yields to
body content at short sizes. Questions keep answer controls separate from the
scrollable prompt; setup choices use probed list geometry. Details, errors and
restore diffs remain scrollable. Tiny layouts refuse unseen answers, submissions
and authorization. Panels restore selection and scrolling; the action palette
starts clean. Help is derived from the real global keymap.

## Verification

Tests drive command dispatch, key/paste routing, Git projections, restore
preview/confirmation/redo, recovery, and attachments. Visible meanings are
asserted through presentation models. Buffer tests cover responsive geometry;
real PTY tests cover navigation, drafts, and the external-editor lifecycle.

## Related

- [Presentation](presentation.md)
- [Checkpoints](checkpointing.md)
- [Session discovery](session-history.md)
- [Steering](steering.md)
- [Tuika](tuika.md)

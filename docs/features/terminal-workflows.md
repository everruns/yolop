# Review and control work in the terminal

Press **Ctrl+P** to search actions. **Ctrl+X F** searches the transcript and **Ctrl+X D**
opens changes. **Esc** closes a view; **Ctrl+X P** moves between the changes panel
and the intact composer. Ordinary messages sent during work remain steering.

## Review changes

`/diff` shows changed files and navigable diffs. At 110 columns or wider it
docks beside the conversation. In narrower terminals it fills the viewport
while focused and hides when you return to the composer.

- **Up/Down** selects a file; **PageUp/PageDown** scrolls its diff.
- **Tab** cycles Working tree, Last turn, Session, and Versus base.
- `/diff-base origin/main` selects a comparison branch or commit.
  Empty, help, and invalid references preserve the previous comparison base.
- **h** selects the next hunk. **a** attaches that hunk to the draft;
  **A** attaches the full file diff. Add your question and send normally.

Working tree includes staged, unstaged, and untracked changes. Last turn and
Session require a captured Yolop worktree snapshot. An unavailable snapshot
is explained rather than replaced with another comparison. The panel refreshes
while open. Comparisons stay inside the active workspace, including repository
subdirectories. Git queries, file counts, and previews are bounded. Binary and
oversized content are identified when a text preview is unavailable.
Unsupported filename encodings are skipped with a visible count; other files
remain reviewable.

## Inspect context before sending

`/attach src/main.rs#20-45` attaches a snapshot of those lines. Omit the range
for the whole text file. Paths are workspace-relative. Symlinks, special files,
and files above 512 KiB are refused.
Only numeric `#line` or `#start-end` suffixes select ranges, so names such as
`notes#draft.md` remain literal paths.

`/attachments` previews attached text. **Delete** removes the selected payload
and placeholder. Select an image and press **Enter** to preview it on terminals
with graphics support. Other terminals retain the attachment with a text fallback.

Set `VISUAL` or `EDITOR` and press **Ctrl+G** to edit a longer prompt externally.
Yolop restores the same session with the edited text in the composer. Saving
does not submit it. An editor failure preserves the original draft.

## Rewind and resume

`/timeline` lists checkpoint prompts, times, outcomes, and snapshot availability.
Choose **c** for conversation, **w** for files, or **b** for both. **Enter**
prepares a preview; a second **Enter** confirms. **Esc** cancels the preview.
**r** prepares redo. Active writers still block restoration. Workspace restore
does not rewind Git commits, ignored files, or external effects. Textual
`/rewind`, `/undo`, and `/redo` remain available.

`/sessions` searches saved titles, workspace paths, branches, and messages.
Select a result to inspect its last matching message and saved recap; press
**Enter** to resume it in its saved workspace. Finish or cancel foreground work before switching.
Private drafts and attachments are restored when you return. A session already
open elsewhere cannot be taken over.

## Understand activity and recover

Compact summaries distinguish thinking, tool execution, client waits, writing,
compaction, and credit recovery. They include the operation, elapsed time,
action count, and running background count. **Ctrl+O** reveals retained details.
Credit-check times appear when supplied by the runtime. `/background` retains
the controls for scheduled work.

After a failed turn, `/recover` offers continuation, model selection, sign-in,
and work inspection. Continuation asks the agent to inspect current state and
completed work before retrying. It does not replay the last command. Your
unsent draft stays intact.

Hard approval shows the full action, reason, and permission scope. **y** permits
this request once. **a** permits all subsequent requests in that scope for the
current process session. **n**, **Esc**, or **Ctrl+C** denies. Only unmodified
approval keys and their Shift variants authorize. Pasted text never does.

`/context` separates latest-request context occupancy, cumulative tokens reported
during this process, and the compaction threshold. Monetary cost and subscription
allowance remain unknown when the host cannot report them.

## Inspect checks, tasks and active context

`/checks` lists recorded validation commands with actual exits, timing, output,
revision and freshness. New shell checks capture fingerprints before and after
execution. Later edits mark evidence stale. Fingerprints cover Git-visible files
in the active workspace, excluding ignored files, submodules and external
dependencies. Missing historical evidence is labelled unknown. Saved output is
previewed with explicit size limits; **Ctrl+R** refreshes the view.

`/tasks` opens background commands and agents. **Enter** loads the selected
work's messages, child transcript and private log tails. **x** requests
cancellation; **m** sends a message through an agent's task channel. **/** searches
output, with an empty search clearing the filter. **t** pauses or resumes the
2-second refresh; **End** follows the tail and paging keeps your reading position.
Cancellation requests remain visible until the runtime confirms the outcome.

`/context` also explores the effective next-request instructions, retained
instruction-file sources, model-visible tools, available skill descriptions,
loaded skill bodies and current filtered messages. Character/4 token counts
are estimates. Draft attachments are labelled unsent rather than treated as
already loaded context. The context browser shows at most 128 recent messages
and bounded text previews.

Type **@** at a word boundary, or open `/files`, to search files across the
workspace with fuzzy matching. Select a result to preview its actual content.
Add `#20-45` to attach a line range, then **Enter** adds the snapshot to the
attachment tray. Ordinary email addresses typed inside a word stay text.

## Ask alongside active work

`/btw your question` opens an independent asynchronous answer over the existing
conversation context. It uses no tools and does not add a steering message or
change the main conversation history. **c** requests a terminal clipboard copy,
**s** attaches the answer to your draft, and **r** retries an unanswered question.
Closing the view cancels an outstanding read-only request and preserves the draft.

## Keep attention and access visible

`/attention` collects questions, approvals, failed turns and completed background
work. **Enter** jumps to the conversation, task or recovery view. **Delete**
acknowledges an item; refreshing task state does not recreate it. The composer
shows the unread count. Resolved foreground questions and approvals clear their
waiting items automatically.

Desktop alerts start **Off**. In the inbox, **n** cycles Off, OSC 9 and OSC 777
for this process. They require terminal and OS support and notify only for a
new event after the terminal reports that focus is elsewhere. Unknown focus
suppresses alerts. Notifications contain a generic message, never prompts,
commands or results. The visible inbox remains available on every terminal.

`/permissions` explains the effective sandbox, network policy and write roots.
Select a session grant and press **Delete** to revoke it for future requests.
Already running work continues; grants do not replace the native sandbox policy.

## Manage sessions and navigate dialogs

In `/sessions`, **Ctrl+S** pins a session, **Alt+R** renames it, and
**Ctrl+Delete** archives or restores it. **Tab** cycles Active, Archived and All;
**Alt+G** groups projects. Archived work remains searchable in its view. Labels
are private UI metadata; canonical history stays authoritative. A live lease
marks sessions active elsewhere and prevents takeover.

Rewind previews include the actual current-to-target file diff before
confirmation. **PageUp/PageDown**, **Home** and **End** inspect it; **Esc**
cancels without restoring anything. If the diff cannot load, preparation fails
and the preview token is cancelled.

Dialogs own keyboard and mouse input over underlying panels. Text follows the
active light or dark palette. In setup, **Ctrl+C** arms a visible exit prompt and
a second press exits without clearing the draft behind the dialog. Questions keep the
answer field visible while their prompt scrolls. Setup lists keep the selected
option visible and use the rendered row bounds for clicks. **PageUp/PageDown**
reveals long descriptions and errors. Tiny terminals show a resize message and
refuse unseen submissions or approvals. Workflow panels remember selection and
scroll position; the action palette starts a fresh search each time. `/help`
uses the same global keymap as the running application.

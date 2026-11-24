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

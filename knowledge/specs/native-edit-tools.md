---
type: Product Specification
title: Native edit tools (optional)
description: Defines the opt-in native_edit_tools capability, which offers each model family the file-edit tool shape it was trained on.
---

# Native edit tools (optional)

Status: implemented in `src/capabilities/apply_patch.rs`
(`NativeEditToolsCapability`). **Off by default.**

## Why

Every model gets the same `edit_file` and `write_file`. Models complete tasks in
fewer calls with tools that are already in their training data, and the edit
tool is the one a coding agent calls most. OpenAI's GPT and Codex models were
trained on Codex's `apply_patch` tool and its `*** Begin Patch` envelope;
asking them to learn `edit_file`'s schema instead may cost retries that a
familiar shape would not.

That is a hypothesis, not a measurement. The capability exists so the A/B can
be run, and the default stays unchanged until it says otherwise.

## What

### Enablement

A catalog-registered capability, enabled like `ast_edit` or `lsp`:

```toml
[[capabilities]]
ref = "native_edit_tools"
```

It reuses the capability override rather than a dedicated settings key, so
`yolop config`, profiles, and the eval's harness variants handle it with no new
plumbing. `coding_harness_does_not_enable_native_edit_tools_by_default` guards
the opt-in.

### Per-model dispatch

The capability implements `Capability::resolve_for_model`, the mechanism
upstream `auto_tool_search` uses, so the choice follows the model each turn is
assembled for. A live model switch changes the tool set on the next turn
without rebuilding the session.

| Model family | Detection | Native shape |
| --- | --- | --- |
| OpenAI | id (after any `provider/` prefix) starts with `gpt-`, contains `codex`, or is an `o<digit>` reasoning model | `apply_patch` added beside `edit_file` and `write_file` |
| Anthropic and everything else | anything else, including an unknown model | unchanged |

`apply_patch` is added, not substituted: the default tools keep working, and
the eval measures which one the model reaches for.

### Claude stays unchanged

Claude's own text-editor tool (`str_replace_based_edit_tool`) replaces an exact,
unique `old_str` with `new_str` and fails on no match or several matches.
`edit_file` has the same contract: exact matching, uniqueness required, no
fuzzy fallback. The differences are framing (an `edits[]` array, and the
`expected_hash` that guards stale reads), not semantics. A second Claude-shaped
tool would add schema without adding a shape Claude does not already know, so
native mode leaves Claude models alone. A Claude-specific tool can be revisited
if the A/B shows `edit_file` retries on Claude that the framing explains.

### `apply_patch`

One string parameter, `input`, holding a patch in Codex's grammar:

```text
*** Begin Patch
*** Add File: path            (every following line starts with +)
*** Delete File: path
*** Update File: path
*** Move to: new/path         (optional, right after Update File)
@@ optional line to seek first
 context line
-removed line
+added line
*** End of File               (optional, anchors the hunk at the end)
*** End Patch
```

Behavior worth knowing:

- **Atomic planning.** The whole patch is parsed and every operation planned
  against one snapshot before anything is written. A bad envelope, a context
  line that is not found, or an update or delete of a missing file returns an
  error and leaves the workspace untouched. Updates commit with
  compare-and-swap, so a file changed underneath the patch is reported, not
  overwritten.
- **Context matching** follows Codex: exact first, then ignoring trailing
  whitespace, then ignoring surrounding whitespace. Matched context lines keep
  the file's own text, so loose matching never rewrites lines the patch did
  not change.
- **Line endings** follow the original file; a CRLF file stays CRLF.
- **Shell form.** A patch wrapped in Codex's heredoc form
  (`apply_patch <<'EOF' ... EOF`) is accepted.

### Same safety path as `edit_file`

- Writes go through the session file store, so `apply_patch` gets the same
  mount table, workspace containment, write blocklist, and read-only
  enforcement as `edit_file`.
- It declares `edit_file`'s scheduling hint (`session_workspace`), so it is
  serialized against other workspace writes and `bash`.
- A patch containing `*** Delete File:` is escalated to the destructive tier in
  `tool_approval`, the tier `delete_file` declares. An editing patch prompts no
  more than `edit_file` does.
- Checkpoints snapshot the worktree per turn regardless of which tool wrote it.
- `progress_guard` classifies `apply_patch` as a mutation.
- The schema is in the never-defer list: a deferred stub would hide the one
  parameter whose shape is the point of the capability.

No parser was reusable from the everruns crates, so the grammar lives in
yolop.

## Validation

`evals/harness_basic` carries the A/B: the `native-edit-tools` harness variant
and the `edit-tools-compare` preset run the `edit-tools` samples on
`openai/gpt-5.5` and `anthropic/claude-sonnet-5-5` against `default`. Read pass
rate next to `edit_tool_calls_failed` (edit retries) and
`apply_patch_tool_calls` (adoption). Claude is the control: its tools do not
change, so its numbers should not move. The default changes only on a measured
win.

## Non-goals

- Replacing `edit_file` for any model by default.
- A Claude-shaped edit tool (see above).
- Model-specific prompt text: the tool description carries the grammar.

## Related

- [System prompt composition](system-prompt.md), the same "already in the
  weights" argument applied to CLIs.
- [Tool calling](tool-calling.md), argument shape enforcement.
- [Approval](approval.md), the tiers `apply_patch` deletes escalate into.
- [AST editing](ast_edit.md), the other opt-in edit capability.

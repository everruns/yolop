---
type: Product Specification
title: `ast_edit`, previewed ast-grep rewrites (optional)
description: Defines the `ast_edit`, previewed ast-grep rewrites (optional) contract for Yolop.
---

# `ast_edit`, previewed ast-grep rewrites (optional)

Status: implemented in `src/capabilities/ast_grep.rs` (`AstEditCapability`).
**Off by default.**

## Why

Yolop's default navigation stack includes read-only `ast_grep` for structural
search. Bulk shape rewrites, rename every call of a pattern, strip debug
statements, swap an idiom, still fall to repeated `edit_file` calls or shell
one-liners. The ast-grep engine already compiles into the binary; exposing
pattern/replacement rewrites with a preview-first flow gives agents a single
tool for multi-file structural edits without new processes or external CLIs.

## What

A yolop-owned `ast_edit` capability (separate from read-only `ast_grep`) that
exposes one tool:

- `ast_edit`, scan workspace source with an ast-grep `pattern`, rewrite each
  match with `replacement`, return per-file `diff` and `replacements_detail`.
  **`dry_run` defaults to `true`** (preview only); call again with
  `dry_run=false` after the user accepts the preview.

Patterns and replacements use **ast-grep syntax** in the target language's
own grammar (not regex, not `sg` YAML rules). Metavariables: `$NAME` (one
node), `$$$ARGS` (zero or more nodes), `$_` / `$$$` (anonymous wildcards).
See the bundled `ast-grep` skill for recipes.

### Supported languages

Same grammars as `ast_grep` (compiled via `ast-grep-language`):

| `language` arg | Typical extensions |
| -------------- | ------------------ |
| `rust`         | `.rs`              |
| `python`       | `.py`, `.pyi`, …   |
| `typescript`   | `.ts`, `.mts`, `.cts` |
| `tsx`          | `.tsx`             |
| `javascript`   | `.js`, `.jsx`, `.mjs`, `.cjs` |
| `csharp`       | `.cs`              |
| `go`           | `.go`              |
| `css`          | `.css`, `.scss`    |
| `html`         | `.html`, `.htm`, … |
| `bash`         | `.sh`, `.bash`, …  |

Pass `language` when known; without it the scan tries every supported grammar.
Unsupported files are skipped and counted in the result.

### Enablement, off by default

`ast_grep` stays in the default harness; `ast_edit` is registered in the
catalog only. Enable per user in `settings.toml`:

```toml
[[capabilities]]
ref = "ast_edit"
```

or by asking yolop to update config (`set_config key=capabilities`). A runtime
test (`coding_harness_does_not_enable_ast_edit_by_default`) guards the opt-in
contract.

### Safety

- All paths are resolved against the workspace root through `WorkspaceHost`;
  writes use the same blocklist and bounds as other filesystem tools.
- `dry_run=true` (default) never writes disk; transcript renders the returned
  `diff` like `edit_file` previews.
- Scans skip oversized files (`max_file_bytes`, default 512 KiB, max 2 MiB),
  binary/unsupported extensions, and common vendor/build dirs (`.git`,
  `node_modules`, `target`, …).
- Replacement count is capped (`limit`, default 50, max 500); preview `diff`
  is truncated at 8 KiB with a flag when longer.
- Classified as a **mutation** in progress guard (same as `edit_file`).

## Design notes

- `ast_grep` and `ast_edit` are separate capabilities so read-only search stays
  on by default and rewrite tooling is an explicit opt-in.
- Preview/accept is modeled as two tool calls (`dry_run` true then false), not
  a separate UI card, matching how other write tools surface diffs in the
  transcript today.
- Confirm matches with `ast_grep` before rewriting; `pattern_error_languages`
  in the result reports compile failures per language (syntax slips, wrong
  `language` filter).

## Measuring it

[`evals/harness_basic/`](../../evals/harness_basic/) includes structural-rewrite
cases (`replace-console-log`, `strip-print-debug`, `unwrap-to-expect`) and a
`with-ast-edit` harness variant. Preset `ast-edit-compare` A/Bs default vs
enabled on tag `ast-edit`, reporting pass rate, `ast_edit_tool_calls`, and an
`ast_edit_used` adoption scorer.

## Non-goals

- No relational rules, `inside`/`has`, or YAML rule files, single pattern +
  replacement only.
- No languages beyond the ast-grep grammars shipped in the binary (e.g. Java,
  Kotlin, PHP are out of scope here; use `grep_files` / `edit_file` / `lsp`).
- No interactive accept UI beyond preview diff + explicit `dry_run=false`.
- Not enabled in the default harness until adoption and ergonomics are proven.

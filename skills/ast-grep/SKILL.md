---
name: ast-grep
description: Write effective ast-grep patterns for the built-in `ast_grep` structural-search tool. Use when searching code by shape rather than text — call expressions, function/struct declarations, wrappers, imports — or when `grep_files` is too noisy, and when a pattern returns nothing or the wrong nodes.
user-invocable: true
---

# ast-grep structural search

Yolop ships a built-in `ast_grep` tool: read-only, multi-language structural
code search backed by the `ast-grep` engine compiled into the binary. This is
**not** the `sg` command-line tool — there is no `sg run`/`sg scan`, no rule
YAML files, and no `--rewrite`. Drive the `ast_grep` tool directly with its
arguments. It never edits code; read matched files with the file tools before
changing anything.

## When to reach for it

Use `ast_grep` for code **shapes**, after `repo_map`/`grep_files` have narrowed
the area:

- Call sites of a function regardless of receiver or arguments.
- Declarations: functions, structs, classes, impls, interfaces.
- Wrappers and idioms: `unwrap()`, `try/except`, specific decorators/attributes.
- Anywhere lexical search drowns in comments, strings, or false matches.

Prefer `grep_files` for exact text, identifiers, log strings, or non-code files.
ast-grep only parses the supported languages below; everything else is skipped.

## Tool arguments

- `pattern` (required): an ast-grep pattern in the **target language's own
  syntax** (see metavariables below).
- `language` (recommended): one of `rust`, `python`, `typescript`, `tsx`,
  `javascript`, `csharp`, `go`, `css`, `html`, `bash`. Without it the pattern is
  tried against every language and only the ones that parse it contribute.
- `path` (optional): workspace-relative file or directory. Defaults to the
  workspace root. Must stay inside the workspace.
- `limit` (optional): max matches, default 50, max 500.
- `max_file_bytes` (optional): skip larger source files, default 512 KiB.

Always pass `language` when you know it: it scopes the scan, avoids cross-language
pattern-compile noise, and is faster.

## Metavariable syntax

Patterns are real code with metavariables standing in for sub-trees:

- `$NAME` — matches exactly one named node (an identifier, expression, etc.).
  Upper-case, may include digits/underscores. Reusing the same name requires the
  matches to be identical.
- `$$$ARGS` — matches zero or more nodes (argument lists, statement bodies,
  parameter lists). Use it wherever a variable-length sequence appears.
- `$_` / `$$$` — anonymous wildcards when you don't need the capture text back.

Captured names come back in each match's `captures` array; the matched node's
`kind`, `text`, and line/column come back too. Use `kind` to confirm you matched
the construct you intended (e.g. `function_item`, not `call_expression`).

## Pattern recipes

```text
# Rust — any call to a method named connect, any receiver/args
language=rust    pattern: $RECV.connect($$$ARGS)

# Rust — function definitions with no parameters
language=rust    pattern: fn $NAME() { $$$BODY }

# Rust — .unwrap() calls (find panics to harden)
language=rust    pattern: $X.unwrap()

# Python — calls to a specific function
language=python  pattern: requests.get($$$ARGS)

# Python — functions decorated with @app.route.
# The `pattern` value is one plain string with embedded newlines — the
# tool takes a string, not YAML; pass exactly these three lines:
#   @app.route($$$)
#   def $NAME($$$PARAMS):
#       $$$BODY
language=python  multiline pattern shown above

# TypeScript — console.log calls
language=typescript  pattern: console.log($$$ARGS)

# Go — error checks
language=go      pattern: if err != nil { $$$BODY }
```

## When a pattern returns nothing

A valid-but-wrong pattern matches zero nodes silently. Work it like this:

1. **Check `pattern_error_languages`** in the result — a non-empty entry means
   the pattern failed to *compile* for that language (usually a syntax slip or
   the wrong `language`), not that nothing matched.
2. **Make the pattern a complete, parseable fragment.** ast-grep parses the
   pattern with the same grammar as source, so it must stand on its own. Partial
   fragments like `fn $NAME(` won't parse — write `fn $NAME($$$A) { $$$B }`.
3. **Loosen with `$$$`.** Replace fixed argument/body content with `$$$` to stop
   over-constraining; tighten back once you get hits.
4. **Confirm `kind`.** If matches come back as the wrong node kind, your pattern
   is matching a sub-expression; add surrounding context to anchor it.
5. **Widen scope.** Drop `path`, or remove `language` to see which language
   actually parses the construct, then re-add the right one.
6. **Fall back to `grep_files`** for a quick identifier sanity check that the
   symbol exists where you expect.

## Limits

- Read-only: no rewrite/codemod. Make edits with the file tools after locating.
- Single-pattern only: no relational rules, `inside`/`has`, or YAML configs.
- Binary, oversized (> `max_file_bytes`), and unsupported-language files are
  skipped; the result reports those counts.

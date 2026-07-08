# `lsp` — language-server integration (optional)

Status: implemented in `src/capabilities/lsp/`. **Off by default.**

## Why

Yolop's navigation stack (grep, `repo_map`, `ast_grep`) is lexical and
structural. It cannot answer semantic questions — where a symbol is *defined*,
who *references* it across module boundaries, what *type* an expression has —
and a textual rename misses re-exports, barrels, and aliases. Real language
servers already solve all of this per ecosystem; wiring them in gives the
agent IDE-grade code intelligence without reimplementing any language.

## What

A yolop-owned `lsp` capability that speaks the Language Server Protocol over
stdio to real servers and exposes seven tools:

- `lsp_diagnostics` — errors/warnings for one file (pull diagnostics when the
  server supports them, merged and deduped with push diagnostics).
- `lsp_definition` — go to definition / type definition / implementation /
  declaration.
- `lsp_references` — semantic find-references across the workspace.
- `lsp_hover` — type signature and docs at a position.
- `lsp_rename` — workspace-wide rename; applies the server's `WorkspaceEdit`
  to disk (with `dry_run` preview).
- `lsp_symbols` — file outline (`documentSymbol`) or fuzzy workspace search
  (`workspace/symbol`).
- `lsp_code_actions` — list server quick fixes/refactorings at a position and
  apply one by exact title (workspace-edit actions only; command-only actions
  are reported but not executable).

Tool coordinates are 1-based `(line, column)` counted in characters, matching
`ast_grep` output; the capability converts to/from the server's negotiated
position encoding (UTF-8/UTF-16/UTF-32).

### Enablement — off by default

The capability is registered in the catalog but **not** in the default
harness, because it spawns external server processes. Enable it per user in
`settings.toml`:

```toml
[[capabilities]]
ref = "lsp"
```

or by asking yolop to update config (`set_config key=capabilities`). A
runtime test (`coding_harness_does_not_enable_lsp_by_default`) guards the
opt-in contract.

### Servers

Built-in specs (spawned lazily, per language, on first use; kept alive for
the session; restarted if they exit):

| key          | command                            | extensions                    |
| ------------ | ---------------------------------- | ----------------------------- |
| `rust`       | `rust-analyzer`                    | rs                            |
| `typescript` | `typescript-language-server --stdio` | ts, tsx, js, jsx, mts, cts, mjs, cjs |
| `python`     | `pyright-langserver --stdio`       | py, pyi                       |
| `go`         | `gopls`                            | go                            |
| `clangd`     | `clangd`                           | c, h, cc, cpp, cxx, hpp, hh   |

Config (validated against the capability's `config_schema`) can override a
built-in's `command`/`args`/`extensions`, disable one (`enabled = false`), or
add a new server key (requires `command` + `extensions`), plus
`request_timeout_ms` and `diagnostics_wait_ms`. A missing binary is a tool
error at call time with install/override guidance — never a startup failure.

### Safety

- Every queried path and every server-proposed edit is resolved against the
  workspace root; edits that escape it (lexically via `..` or through
  symlinks) are rejected. Locations outside the workspace (stdlib, deps) are
  returned read-only and flagged `external`.
- Server processes are `kill_on_drop`; a crashed server fails pending
  requests with a clear message and respawns on the next call.

## Design notes

- The client (`client.rs`) is transport-generic (`AsyncRead`/`AsyncWrite`),
  so tests drive the full tool stack against an in-process fake server over
  `tokio::io::duplex`; only `manager.rs` knows about processes. An ignored
  `rust_analyzer_smoke` test exercises the real-process path.
- Server-initiated requests (`workspace/configuration`,
  `client/registerCapability`, progress creation) get benign default replies
  so servers that block on them keep working.
- Documents are synced from disk on every tool call (`didOpen` first touch,
  full-text `didChange` when content moved), so edits made by other tools are
  always visible to the server.

## Measuring it

[`evals/lsp_integration/`](../evals/lsp_integration/) is the dedicated A/B
study: semantic-navigation traps (decoy renames, re-export chains, no-build
diagnostics) run with the capability off vs on, reporting pass rate, `lsp_*`
adoption, turns, tokens, and cost.

## Non-goals

- No always-on background indexing or diagnostics streaming into the prompt;
  everything is on-demand through tools.
- No `workspace/executeCommand`; code actions that only carry a command are
  listed but not applied.
- No LSP extensions (semantic tokens, inlay hints, call hierarchy) until a
  concrete need shows up.

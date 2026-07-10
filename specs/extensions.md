# Extensions — user-installable capability contributions

Status: PoC in `src/extensions/`. **Illustrated with LSP.**

## Why

Yolop capabilities (LSP, MCP, providers, UI, …) are compiled into the
binary today. Users can opt in via `settings.toml`, but adding a new language
server, MCP server, or provider still requires a yolop release. A declarative
extension format lets users install feature packs without recompilation —
similar to how VS Code extensions contribute language servers and MCP configs.

## What

An **extension** is a directory with an `extension.toml` manifest, installed
under:

- **global**: `<config_dir>/yolop/extensions/<id>/`
- **workspace**: `<workspace>/.yolop/extensions/<id>/`

Workspace extensions with the same `id` override global ones.

### Manifest

```toml
id = "yaml-lsp"
name = "YAML Language Support"
version = "0.1.0"
description = "Optional description"
publisher = "optional"

# Capability contributions — merged like [[capabilities]] config
[contributions.capabilities.lsp.servers.yaml]
command = "yaml-language-server"
args = ["--stdio"]
extensions = ["yaml", "yml"]

# MCP contributions (parsed; merged into session MCP config)
# [contributions.mcp.my-server]
# type = "stdio"
# command = "my-mcp-server"
```

### CLI

```bash
yolop ext list
yolop ext install ./extensions/yaml-lsp          # workspace scope (default)
yolop ext install ./extensions/yaml-lsp --global
yolop ext uninstall yaml-lsp
```

### Runtime behavior

At session build:

1. Discover extensions (global, then workspace).
2. Merge `contributions.capabilities` into the harness. Extensions
   **auto-enable** opt-in capabilities they contribute to (e.g. installing
   `yaml-lsp` enables `lsp` without editing `settings.toml`).
3. Merge `contributions.mcp` into the session MCP server map.

Contributions are deep-merged (nested maps like `servers` combine per key).
Invalid contributions are warned and skipped.

## Design notes

- **Contract mirrors capabilities** — extensions contribute config fragments
  keyed by capability id, not arbitrary code. Future kinds (`providers`, `ui`,
  everruns plugins) get their own contribution tables.
- **No recompilation** — install copies files; runtime reads manifests.
- **Trust** — extensions run with the same trust model as `settings.toml` and
  `.mcp.json`: the user explicitly installs them. No sandbox in PoC.
- **Example** — `extensions/yaml-lsp/` in the repo demonstrates LSP server
  contribution.

## Non-goals (PoC)

- Extension marketplace / remote install
- Cryptographic signing or sandboxing
- Dynamic Rust/WASM plugin loading
- UI contribution rendering

## Follow-ups

- Provider and UI contribution wiring
- Extension-aware `get_config` / catalog listing
- Remote install (`yolop ext install <url>`)

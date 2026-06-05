# MCP — Model Context Protocol client support

Status: v1 implemented (HTTP + stdio, workspace + global config, approval gating).

## Why

[MCP](https://modelcontextprotocol.io) is the open standard for giving an agent
extra tools — local (filesystem, git, sqlite) or remote (docs, issue trackers).
Supporting it lets yolop use the same `.mcp.json` server catalog that every
other MCP client understands, with no bespoke per-tool integration.

The MCP **client** lives upstream in `everruns-mcp` (transport-agnostic) and is
wired into the in-process `everruns-runtime`; see the upstream
`specs/runtime-mcp.md` decision record. Yolop does not implement the protocol
itself — it configures servers and consumes the runtime's discovery + execution
path, so MCP tools flow through the same agent loop as the built-in tools.

## What — scope of the layer

- **Transports**: remote **Streamable HTTP** (always available) and local
  **stdio** (child process). stdio rides the runtime's `mcp-stdio` cargo
  feature, which yolop enables; the hosted everruns product compiles it out.
- **Configuration**: a `.mcp.json` file using the `mcpServers` object shape.
  Two scopes are read and merged (`merge_scoped_mcp_servers`):
  - **global**: `<config_dir>/yolop/mcp.json` (e.g. `~/.config/yolop/mcp.json`)
  - **workspace**: `<workspace_root>/.mcp.json` — overrides global by name.
  A malformed file warns and is skipped rather than failing startup.
- **Secrets via env**: string fields support `${VAR}` expansion from the
  environment (`"Authorization": "Bearer ${DOCS_TOKEN}"`), so tokens stay out of
  the file. Unset placeholders are left intact so the gap is debuggable.
- **Discovery + execution**: the runtime discovers each server's tools live
  (`tools/list`) and routes `mcp_*` tool calls to the MCP executor. Tool names
  are prefixed (`mcp_<server>__<tool>`) by the runtime to avoid collisions.
- **Visibility**: `/mcp` lists the configured servers; configured server names
  also appear in `StartupInfo`.
- **Approval gating**: `McpApprovalCapability` contributes a `PreToolUseHook`
  (via the `Capability::pre_tool_use_hooks` seam, everruns 0.8.38+) that routes
  every non-readonly `mcp_*` call through the same `ApprovalGate` as `bash`,
  honoring the readonly hint the runtime derives from MCP tool annotations.
  Registered only when MCP servers are configured; a no-op when the gate is
  `Auto` (no `--ask`, and `--print`). Non-MCP tools keep their own gates and
  are not double-prompted.

Config shape:

```json
{
  "mcpServers": {
    "docs": { "type": "http", "url": "https://example.com/mcp",
              "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } },
    "fs":   { "type": "stdio", "command": "mcp-server-filesystem",
              "args": ["${WORKSPACE}"], "env": { "RUST_LOG": "info" } }
  }
}
```

`type` defaults to `http`; for HTTP, `url` is required.

## Trust model

- **HTTP** keeps the runtime's DNS-pinned SSRF protection — no relaxation.
- **stdio** spawns local processes the user explicitly listed in their own
  `.mcp.json`. Authoring that file is the act of consent, mirroring how other
  MCP clients treat a project-scoped server list.
- **Per-call approval**: with `--ask`, every non-readonly MCP tool call is
  gated through the `ApprovalGate` (see "Approval gating" above), the same as
  `bash` and file writes. Without `--ask` (and in `--print`) the gate
  auto-approves, consistent with yolop acting autonomously by default.

## Non-goals (for now)

- OAuth (browser/device-code) for remote servers. API-key/bearer via `headers`
  (with `${VAR}` expansion) covers the common case; the runtime exposes an
  `mcp_auth_provider()` seam for a future env/device-code provider.
- MCP **resources** and **prompts** (tools are the 90% case).
- ACP MCP pass-through: `mcpServers` supplied by an ACP client is still
  accepted-and-ignored (see `src/acp/protocol.rs`); only yolop's own
  `.mcp.json` is honored.

## Where it lives

| Concern | Location |
|---------|----------|
| Config loading (scopes, merge, `${VAR}`) | `src/mcp_config.rs` |
| Wiring into the session | `src/runtime.rs` (`session_mcp_servers`, `StartupInfo.mcp_server_names`) |
| `/mcp` command | `src/capabilities/client_commands.rs`, `src/host_ui.rs`, `src/app.rs` |
| Approval gating | `McpApprovalCapability` in `src/capabilities/host.rs`; `ApprovalRequest::McpTool` in `src/approval.rs` |
| Client / transports / executor | upstream `everruns-mcp`, `everruns-runtime` (`mcp-stdio` feature) |

# MCP — Model Context Protocol client support

Status: v1 implemented (HTTP + stdio, workspace + global config).

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
- **Live reload**: server changes apply to the running session without a
  restart. `/mcp enable|disable|remove` mutate config and immediately re-apply;
  `/mcp reload` re-reads config from disk (picking up `yolop mcp add`, hand
  edits, or the agent's own config tools). The runtime resolves a session's
  scoped MCP servers per turn from `session.mcp_servers` and never negatively
  caches a failed or empty discovery, so swapping that field
  (`RuntimeHandles::reload_mcp_servers`) is enough: added servers are discovered
  cold on the next turn and removed ones drop out of the tool set. A server
  whose config changes but keeps the same name is served from the ≤1h discovery
  cache until it revalidates (stale-while-revalidate) — enable/disable/add/
  remove are unaffected.
- **Execution model**: MCP tool calls run autonomously, like every other yolop
  tool — there is no per-call approval gate.

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

## Authentication

Credentials for a server are resolved per request by the runtime's
`McpAuthProvider`, in this order:

1. **User-scoped OAuth token** minted by `/mcp login <name>` and stored in the
   connection store (`mcp-oauth:<provider>`). Tokens are refreshed
   automatically when they near expiry.
2. **Environment bearer** — `<PROVIDER>_ACCESS_TOKEN`/`_API_KEY`/`_TOKEN` (by
   `oauth_provider_id`) or `MCP_<SERVER>_TOKEN` — for headless/CI use.
3. Literal `headers` in the config (with `${VAR}` expansion), applied by the
   transport regardless of the provider.

**OAuth login** (`/mcp login <name>`, remote HTTP servers) is discovery-based:
protected-resource metadata (RFC 9728) → authorization-server metadata
(RFC 8414 / OpenID discovery) → dynamic client registration (RFC 7591) when the
server offers it → authorization code + PKCE (RFC 7636) through the browser with
a loopback redirect. The token endpoint and client id are persisted alongside
the tokens so refresh is self-contained. Because credentials are resolved per
turn, a fresh login takes effect on the next message — no restart (composes with
live reload above).

## Trust model

- **HTTP** keeps the runtime's DNS-pinned SSRF protection — no relaxation.
- **stdio** spawns local processes the user explicitly listed in their own
  `.mcp.json`. Authoring that file is the act of consent, mirroring how other
  MCP clients treat a project-scoped server list.
- **OAuth** discovery/token calls go to the authorization server advertised by
  the (user-configured) MCP server. Discovered endpoints must be `https`
  (loopback may use `http`), bounding downgrade to plaintext. The user
  configuring the server URL is the act of consent; yolop is a local CLI on the
  user's own network.
- **No per-call approval**: MCP tools run autonomously like the rest of yolop's
  tools; the standing guardrail is the write blocklist on filesystem writes.

## Non-goals (for now)

- OAuth **device-code** flow (browser + loopback covers the CLI case); a
  configured `client_id` for servers without dynamic registration (DCR-only for
  now).
- MCP **resources** and **prompts** (tools are the 90% case).
- ACP MCP pass-through: `mcpServers` supplied by an ACP client is still
  accepted-and-ignored (see `src/acp/protocol.rs`); only yolop's own
  `.mcp.json` is honored.

## Where it lives

| Concern | Location |
|---------|----------|
| Config loading (scopes, merge, `${VAR}`) | `src/mcp_config.rs` |
| Wiring into the session | `src/runtime.rs` (`session_mcp_servers`, `StartupInfo.mcp_server_names`) |
| `/mcp` command (list/reload/enable/disable/remove) | `src/capabilities/client_commands.rs`, `src/host_ui.rs`, `src/app/mod.rs` |
| Live reload seam | `src/runtime.rs` (`RuntimeHandles::reload_mcp_servers`), `src/session.rs` |
| OAuth login (discovery, DCR, PKCE) | `src/mcp_oauth_login.rs` |
| OAuth token storage | `src/mcp_oauth.rs` (connection store) |
| Auth provider (stored token + refresh, env fallback) | `src/runtime.rs` (`StoredMcpAuthProvider`) |
| Client / transports / executor | upstream `everruns-mcp`, `everruns-runtime` (`mcp-stdio` feature) |

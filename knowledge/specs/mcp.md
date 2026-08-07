---
type: Product Specification
title: MCP — Model Context Protocol client support
description: Defines the mcp — model context protocol client support contract for Yolop.
---

# MCP — Model Context Protocol client support

Status: v1 implemented (HTTP + stdio, workspace + global config).

## Why

[MCP](https://modelcontextprotocol.io) is the open standard for giving an agent
extra tools — local (filesystem, git, sqlite) or remote (docs, issue trackers).
Supporting it lets yolop use the same `.mcp.json` server catalog that every
other MCP client understands, with no bespoke per-tool integration.

The MCP **client** lives upstream in `everruns-mcp` (transport-agnostic) and is
wired into the in-process `everruns-runtime`; see the upstream
`knowledge/specs/runtime-mcp.md` decision record. Yolop does not implement the protocol
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
  - **ACP client**: servers passed in `session/new` `mcpServers` (see
    `knowledge/specs/acp.md`) overlay both file scopes for that session, so a
    client-configured server wins on a name collision.
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
a loopback redirect. The authorize URL is printed in the transcript before the
host waits on the callback (so fullscreen and inline both stay usable if the
browser is invisible), and the wait runs in the background so the event loop is
not blocked. The token is bound to the MCP server through the OAuth `resource`
indicator, and callback issuer validation prevents mix-up attacks. The token
endpoint and client id are persisted alongside the tokens so refresh is
self-contained; refreshes are serialized and re-read under the lock so rotated
refresh tokens cannot be spent concurrently. Because credentials are resolved
per turn, a fresh login takes effect on the next message — no restart (composes
with live reload above). Agent-driven `/mcp` and `/tools` via `run_command` return the
host's response text in the tool result; `/tools` includes live discovered
`mcp_*` names from the session's scoped servers.

## Trust model

- **HTTP** keeps the runtime's DNS-pinned SSRF protection — no relaxation.
- **stdio** spawns local processes the user explicitly listed in their own
  `.mcp.json`. Authoring that file is the act of consent, mirroring how other
  MCP clients treat a project-scoped server list.
- **OAuth** discovery/token calls use `everruns-core`'s egress-bound OAuth
  client. Public endpoints require DNS-pinned SSRF validation and discovered
  endpoints must be `https`. Literal loopback endpoints may use `http` because
  the user explicitly configured the local MCP server; Yolop's transport
  adapter limits that exception to loopback hosts.
- **No per-call approval**: MCP tools run autonomously like the rest of yolop's
  tools; the standing guardrail is the write blocklist on filesystem writes.

## Non-goals (for now)

- OAuth **device-code** flow (browser + loopback covers the CLI case); a
  configured `client_id` for servers without dynamic registration (DCR-only for
  now).
- MCP **resources** and **prompts** (tools are the 90% case).

## Where it lives

| Concern | Location |
|---------|----------|
| Config loading (scopes, merge, `${VAR}`) | `src/config/mcp.rs` |
| Wiring into the session | `src/runtime/mod.rs` (`session_mcp_servers`, `StartupInfo.mcp_server_names`) |
| `/mcp` command (list/reload/enable/disable/remove) | `src/capabilities/client_commands.rs`, `src/tui/host_ui.rs`, `src/tui/mod.rs` |
| Live reload seam | `src/runtime/mod.rs` (`RuntimeHandles::reload_mcp_servers`), `src/runtime/session.rs` |
| OAuth protocol (discovery, DCR, PKCE, exchange, refresh) | upstream `everruns-core::oauth`, `everruns-mcp::oauth` |
| OAuth loopback host, token storage, egress adapter | `src/auth/mcp_oauth_login.rs`, `src/auth/mcp_oauth.rs` |
| Auth policy (stored token, env fallback) | `src/runtime/mod.rs` (`StoredMcpAuthProvider`) |
| Client / transports / executor | upstream `everruns-mcp`, `everruns-runtime` (`mcp-stdio` feature) |

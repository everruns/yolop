---
type: Product Specification
title: ACP — Agent Client Protocol support
description: Defines the acp — agent client protocol support contract for Yolop.
---

# ACP — Agent Client Protocol support

Status: v1 implemented (agent side, stdio transport).

## Why

Yolop is a terminal agent, but the same agent loop is useful *inside* an
editor. The [Agent Client Protocol](https://agentclientprotocol.com) (ACP) is
the open, editor-neutral protocol — created by Zed — that lets a code editor
(the **client**) drive an external coding agent (the **agent**) over stdio.
Implementing the agent side means yolop drops into Zed (and any other ACP
client) with no bespoke integration: the editor renders the conversation, tool
calls, plans, and diffs in its own UI while yolop does the work.

This is a promotion target for the same runtime that powers the TUI and
`--print` mode — one agent, three front ends (TUI, one-shot, ACP).

## What — scope of the layer

`yolop --acp` turns the process into an ACP agent speaking **newline-delimited
JSON-RPC 2.0** over stdin/stdout (one compact JSON object per line, no embedded
newlines). Tracing still goes to stderr, so stdout stays a clean protocol
channel.

ACP protocol version: **1** (integer).

### Lifecycle

| Method | Direction | Behaviour |
|--------|-----------|-----------|
| `initialize` | client → agent | Negotiates protocol version and advertises agent capabilities. Echoes the client's version when supported, else advertises v1. |
| `authenticate` | client → agent | Runs an advertised agent-handled login. Yolop currently advertises browser-based ChatGPT/Codex OAuth and OpenRouter PKCE login; other API-key providers continue to use inherited environment variables or `/setup token`. |
| `session/new` | client → agent | Builds a fresh runtime rooted at the client-supplied `cwd`; returns the everruns session id as the ACP `sessionId`. |
| `session/load` | client → agent | Rehydrates an existing yolop JSONL session for the supplied `sessionId` and `cwd`, replays persisted conversation history as `session/update` notifications, and then returns success. |
| `session/prompt` | client → agent | Runs one turn, or executes a recognised `/command`; streams `session/update`s, and resolves a `stopReason`. |
| `session/cancel` | client → agent | Notification. Abandons the in-flight turn for that session and resolves the prompt with `stopReason: "cancelled"`. |
| `session/set_mode` | client → agent | Sets the approval level (session mode). See below. |
| `session/request_permission` | agent → client | Issued before a tool the current level gates; the turn suspends on the answer. See Permissions. |
| `session/update` | agent → client | Notification. Streams the turn (see below), including `current_mode_update` when the level changes out of band. |

`loadSession` is advertised as `true`. `session/load` uses the same JSONL
replay path as CLI `--session`: prior user and assistant messages are streamed
back to the editor before the response, and the loaded runtime then continues
appending to the same session folder.

### Per-session model selection

Yolop uses ACP's standard session configuration mechanism for model selection. `session/new` and `session/load` responses include `configOptions` containing:

- a `model` select option in the standard `model` category;
- a `reasoning_effort` select option in the standard `thought_level` category when the selected model supports reasoning levels.

The model list contains only currently usable providers; a stale preference is
never presented as a connected model. If the preferred provider is unusable,
ACP starts with another usable provider and falls back to local `llmsim` when
none is connected. Session creation therefore never fails only because a saved
provider lost its credentials, while one-shot print mode remains fail-fast.

Clients change either option with `session/set_config_option`. Yolop validates the selected provider, model, and reasoning effort, applies the change only to that live ACP session, and returns the complete refreshed `configOptions` list. After authentication or a `/setup` command changes provider connectivity or selection, Yolop also pushes ACP's standard `config_option_update` to every affected open session. Invalid option IDs and unsupported values return ACP `InvalidParams`.
When the process was launched with `--profile`, that profile supplies the
initial model before any live ACP selection.

### Authentication

`initialize.authMethods` advertises `codex_browser` and `openrouter_browser`,
agent-handled methods that open a browser OAuth flow. Codex stores refreshable
ChatGPT credentials in Yolop settings; OpenRouter exchanges the PKCE code for a
user-controlled API key stored as `tokens.openrouter`. `authenticate` rejects
unknown method ids. A successful login refreshes open sessions' model options,
so clients can expose the newly connected provider's models without restarting
the ACP process. Stable ACP does not define secure API-key entry; remaining
key-based providers stay connectable only through the agent process
environment. Yolop rejects `/setup token` over ACP without echoing or persisting
the supplied value. Plain `/setup` and `/setup login` run the active provider's
advertised login, or ask the user to choose a method when the active provider
has none. This gives failed Codex and OpenRouter sessions an in-conversation
recovery path even when the client does not expose its own authentication
control. Codex authentication failures point to that command instead of generic
support copy, while other API-key failures explain ACP's secure-input boundary
and the environment/restart path. Invalid Codex credentials are cleared before
reauthentication.

### Prompt content

`promptCapabilities` advertises `image: true` and `embeddedContext: true`
(`audio: false`). Inbound content blocks map to the model input as:

- **Text** — passed through.
- **Image** — forwarded as an image content part.
- **Resource** (embedded context) — folded into the model's prompt text: text
  contents are inlined inside a `<resource uri="…">` block; binary contents are
  referenced by URI and MIME type rather than dumped as base64.
- **ResourceLink** — folded in as a one-line `[linked resource: name (uri)]`
  reference so the model knows it was attached.

Resource folding targets the *model* text only; the text used for slash-command
detection and the checkpoint label stays the pure user text, so a resource
attached to a `/command` never derails parsing. Audio is dropped (see
Non-goals).

### MCP servers

The `initialize` response advertises `mcpCapabilities.http: true`; the `stdio`
transport is mandatory for every agent and needs no flag. `sse` is not
advertised — the runtime has no SSE transport.

`session/new` and `session/load` honour the `mcpServers` list: each `http` or
`stdio` entry is translated into the runtime's scoped MCP config and merged over
the file-based `.mcp.json`/global config for that session, so a
client-configured server wins on a name collision (see `knowledge/specs/mcp.md`). Values
pass through literally — the client has already resolved its own placeholders.
An `sse` (or otherwise unsupported) transport is rejected with `InvalidParams`
rather than silently dropped, so a client that ignored the advertised
capabilities gets a clear error instead of a server that quietly went missing.

### Session modes

Yolop surfaces its approval level (`ApprovalMode`: `protective` / `normal` /
`off`, see `src/capabilities/approval.rs`) as ACP session modes, so an editor
can switch it from the same mode picker it uses for other agents — one
vocabulary across every front end instead of an ACP-only taxonomy.

- `session/new` and `session/load` responses carry a `modes` block listing the
  three levels (strictest first) with the current one selected.
- `session/set_mode` maps the mode id back to a level and persists it. It writes
  the selected profile when `--profile` is active and global settings otherwise,
  exactly like `/setup approval`. An unknown mode id is rejected with
  `InvalidParams`.
- When the level changes out of band (the `set_approval_mode` tool, `/setup
  approval`), yolop emits a `current_mode_update` after the turn so the picker
  stays in sync.

The level drives both the soft-approval prompt block
(`src/capabilities/approval.rs`) and the hard permission gate (see Permissions
below): it selects which tools require an interactive
`session/request_permission`.

### Streaming a turn

While a turn runs, runtime events are translated into `session/update`
notifications. The mapping is a pure, per-turn state machine
(`acp::bridge::Translator`) so it is fully unit-testable:

| Runtime event | ACP update |
|---------------|-----------|
| assistant text delta | `agent_message_chunk` (incremental) |
| completed assistant message, when no deltas streamed | `agent_message_chunk` (whole text) — covers providers that don't stream; provider-authentication failures use ACP `/setup` recovery copy |
| extended-thinking delta | `agent_thought_chunk` |
| provider-curated reasoning summary | `agent_thought_chunk` (displayable reasoning; segments separated by blank lines) |
| completed assistant commentary with tool calls, when no deltas streamed | `agent_message_chunk` before the tool activity |
| tool started | `tool_call` (`status: in_progress`, `rawInput`, semantic `kind`) |
| tool completed | `tool_call_update` (`status: completed`/`failed`, summary `content`) |
| `write_todos` tool | `plan` (entries with status) instead of a raw tool call |

Yolop classifies runtime tools into ACP's semantic kinds (for example `read`,
`search`, `fetch`, `edit`, or `execute`). This gives clients a stable tool label
that is distinct from Yolop's narrated title.

To avoid duplicating streamed text, a completed assistant message is only
emitted as a chunk when no deltas streamed for it during the turn.

After `session/new`, yolop sends `available_commands_update` with
capability-sourced slash commands such as `/setup` and user-invocable skill
commands. ACP clients run commands by sending their literal text in
`session/prompt` (for example, `/setup status`). `!<command>` and
`!shell <command>` are accepted shortcuts for `/shell <command>`. System
commands execute through `runtime.execute_command` and stream a command-shaped
`tool_call` / `tool_call_update` pair with structured `rawInput`, `rawOutput`,
and text `content`; skill commands are forwarded as prompt text so the model
can activate the skill.

ACP v1 command input only standardises an unstructured `input.hint`. yolop also
adds compatible extension metadata under `_meta["yolop.dev/command"]` so richer
clients can render command argument suggestions (for example `/setup login`,
`/setup status`, `/setup provider openai`, or `/setup effort high`). Standard clients ignore
this metadata and still see the command name, description, and hint. After a
system command runs, yolop re-emits `available_commands_update` so clients can
refresh any state-sensitive command UI.

### Stop reasons

`session/prompt` resolves with:

- `end_turn` — the tracked user ask reached achieved, blocked, failed,
  waiting-on-background, or the bounded continuation budget. In-progress turns
  are continued and streamed before the original prompt resolves. Recoverable
  failure text is streamed as an `agent_message_chunk`.
- `cancelled` — a `session/cancel` arrived, or the turn task was dropped.

The runtime does not expose token-limit or refusal outcomes distinctly, so
`max_tokens`, `max_turn_requests`, and `refusal` are not currently produced.

### Permissions

The session mode (approval level) drives a hard gate. Before a tool runs, the
upstream `everruns-core` tool-approval capability checks whether the current
level requires approval for that tool, classified by the runtime's own
`ToolHints`. Yolop's thin adapter supplies the live central setting on every
call so `session/set_mode`, `/setup approval`, and `set_approval_mode` take
effect without rebuilding the session:

- `off` — never asks; tools run autonomously (unchanged behaviour).
- `normal` — asks before `destructive` or `open_world` (outward-facing) tools.
- `protective` — asks before any tool that is not `readonly`.

When approval is required, yolop issues `session/request_permission` with four
options (allow once / always, reject once / always) and the turn genuinely
suspends until the client answers — this is safe because the turn already runs
in its own task, off the SDK event loop. "Always" answers are remembered per
tool for the session; a rejection blocks the tool with an error the model sees.

The gate only runs when the host can service an interactive prompt: the ACP
server wires a client-backed approver, while the TUI and `--print` keep the
soft-approval guidance alone. If the client cannot answer (no permission UI, or
the connection is closing), the gate falls back to allowing so an editor without
`session/request_permission` keeps working — the write blocklist on filesystem
writes (see `knowledge/specs/maintenance.md`) remains the standing guardrail either way.

## Architecture

```
src/editor/acp/
  mod.rs        # module root: production RuntimeFactory, run_stdio entry, e2e tests
  protocol.rs   # SDK-backed ACP schema shim plus yolop helpers
  bridge.rs     # pure runtime-event → session/update translation (Translator)
  server.rs     # SDK transport/dispatch wiring, session map, turn streaming
```

Concurrency model in `server::serve`:

- The upstream Rust SDK owns newline JSON-RPC parsing, serialization, typed
  request dispatch, and response correlation.
- `session/prompt` runs in its own Tokio task so SDK dispatch keeps processing
  `session/cancel` notifications during a turn.
- `serve` uses the SDK `Lines` transport with an EOF signal so a client
  disconnect still winds the agent process down even mid-turn.

`serve` is generic over the byte streams and a `RuntimeFactory`, so the binary
wires it to real stdin/stdout with a provider-backed factory while tests drive
it over `tokio::io::duplex` pipes with a scripted llmsim runtime.

## Testing

Three layers, all offline (no API key):

1. **Wire types** (`protocol.rs`) — SDK schema round-trips assert exact field
   casing and discriminator values against the published schema.
2. **Translation** (`bridge.rs`) — the `Translator` is exercised per event type
   (deltas, tool lifecycle, todos→plan, dedup, streamed-vs-completed).
3. **End-to-end** (`mod.rs`) — a real `serve` loop over duplex pipes driven by
   an in-memory ACP client: the full `initialize` → `session/new` →
   `session/prompt` handshake, `session/load` history replay across an ACP
   server restart, unknown-method and unknown-session errors, scripted tool
   calls (asserting `tool_call`/`tool_call_update`), `write_todos` → `plan`,
   and command advertisement/execution.

The binary itself is smoke-tested over real OS pipes in
`tests/integration.rs` (`acp_stdio_handshake_smoke`), and a real-provider test
(`acp_openai_handshake_smoke`, which skips itself when no API key is present)
documents the live path.

### Real-life testing in an editor

Configure Zed to launch yolop as a custom agent:

```bash
yolop into zed
```

This writes the equivalent `agent_servers.yolop` entry to
`~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "yolop": {
      "type": "custom",
      "command": "yolop",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Then pick **yolop** in Zed's agent panel. Any ACP-compatible client works the
same way.

## Non-goals (for now)

- Client-provided filesystem (`fs/read_text_file`, `fs/write_text_file`):
  yolop's runtime reads and writes the host disk directly under the workspace
  root, so it does not route file ops back through the client.
- Terminals (`terminal/*`): deliberately unsupported. ACP terminals have the
  **client** execute the command in its own environment, which would bypass
  yolop's Landlock sandbox and write blocklist — the core of its shell-safety
  model — and split execution (shell client-side, file edits agent-side). The
  gain over the status quo is a live-terminal widget rather than the command
  output yolop already streams as tool-call content, which does not justify the
  loss of the sandbox. Revisit only behind an explicit opt-in that the user
  accepts the trade-off for.
- Audio prompt content (`audio: false`): low value for a coding agent, and the
  runtime's `ContentPart` has no audio variant, so there is no path to forward
  it to the model even if advertised. Embedded/linked **resource** context and
  image content are supported (see Prompt content); MCP-server pass-through too.
- Provider calls stop when their turn task is aborted. Active tools additionally
  receive the runtime's cooperative `ToolContext` cancellation token; Yolop
  awaits task teardown before returning ACP's `cancelled` stop reason so
  detached child work observes cancellation before the client continues.

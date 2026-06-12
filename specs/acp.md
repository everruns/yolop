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
| `authenticate` | client → agent | No-op success: credentials come from the environment/settings the process already inherits, so `authMethods` is empty. |
| `session/new` | client → agent | Builds a fresh runtime rooted at the client-supplied `cwd`; returns the everruns session id as the ACP `sessionId`. |
| `session/load` | client → agent | Rehydrates an existing yolop JSONL session for the supplied `sessionId` and `cwd`, replays persisted conversation history as `session/update` notifications, and then returns success. |
| `session/prompt` | client → agent | Runs one turn, or executes a recognised `/command`; streams `session/update`s, and resolves a `stopReason`. |
| `session/cancel` | client → agent | Notification. Abandons the in-flight turn for that session and resolves the prompt with `stopReason: "cancelled"`. |
| `session/update` | agent → client | Notification. Streams the turn (see below). |

`loadSession` is advertised as `true`. `session/load` uses the same JSONL
replay path as CLI `--session`: prior user and assistant messages are streamed
back to the editor before the response, and the loaded runtime then continues
appending to the same session folder.

### Streaming a turn

While a turn runs, runtime events are translated into `session/update`
notifications. The mapping is a pure, per-turn state machine
(`acp::bridge::Translator`) so it is fully unit-testable:

| Runtime event | ACP update |
|---------------|-----------|
| assistant text delta | `agent_message_chunk` (incremental) |
| completed assistant message, when no deltas streamed | `agent_message_chunk` (whole text) — covers providers that don't stream |
| thinking delta / reasoning summary | `agent_thought_chunk` |
| tool started | `tool_call` (`status: in_progress`, `rawInput`, no approval-oriented `kind`) |
| tool completed | `tool_call_update` (`status: completed`/`failed`, summary `content`) |
| `write_todos` tool | `plan` (entries with status) instead of a raw tool call |

Yolop omits ACP `kind` for runtime tools because tools run autonomously and some
clients attach approval-looking affordances to execute/edit/delete categories.
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
clients can render command argument suggestions (for example `/setup status`,
`/setup provider openai`, or `/setup effort high`). Standard clients ignore
this metadata and still see the command name, description, and hint. After a
system command runs, yolop re-emits `available_commands_update` so clients can
refresh any state-sensitive command UI.

### Stop reasons

`session/prompt` resolves with:

- `end_turn` — the turn completed (success, or a recoverable failure whose
  error text is also streamed as an `agent_message_chunk`).
- `cancelled` — a `session/cancel` arrived, or the turn task was dropped.

The runtime does not expose token-limit or refusal outcomes distinctly, so
`max_tokens`, `max_turn_requests`, and `refusal` are not currently produced.

### Permissions

yolop runs tools autonomously: file writes, deletes, and `bash` execute without
a per-call approval gate, so the agent never issues `session/request_permission`.
The standing guardrail is the write blocklist on filesystem writes (see
`specs/maintenance.md`).

## Architecture

```
src/acp/
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
- Terminals, MCP-server pass-through, and audio/image prompt content.
- In-flight turn interruption beyond abandoning the task — the runtime has no
  mid-turn cancellation hook yet.

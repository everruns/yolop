# yolop-extension-computer-use

A [yolop](https://github.com/everruns/yolop) extension that exposes Apple's
**official signed macOS Computer Use** as direct tools — a native-Rust port of
[`codex-computer-use-mcp`](https://github.com/tmustier/codex-computer-use-mcp)
onto the [yolop extension protocol (YEP)](../../specs/extensions.md).

The calling model chooses every tool and argument itself. There is **no nested
model call, no action planner, and no Codex model turn** — each tool call spawns
one ephemeral, zero-turn `codex app-server` (from the installed ChatGPT.app),
dispatches a single official MCP tool call, and tears the process tree down.

Unlike the upstream package this needs **no Node runtime** — the capability
server is a single Rust binary depending only on `serde_json` and `yolop-yep`.

## Tools

Ten official methods plus a status tool, YEP-namespaced with `computer_use_`:

| Tool | Upstream method | Purpose |
|---|---|---|
| `computer_use_list_apps` | `list_apps` | List running / recently used apps |
| `computer_use_get_app_state` | `get_app_state` | Key-window screenshot + accessibility tree |
| `computer_use_click` | `click` | Click an element or screenshot coordinates |
| `computer_use_perform_secondary_action` | `perform_secondary_action` | Invoke a named accessibility action |
| `computer_use_set_value` | `set_value` | Assign an accessibility value |
| `computer_use_select_text` | `select_text` | Select text / place the cursor |
| `computer_use_scroll` | `scroll` | Scroll an element |
| `computer_use_drag` | `drag` | Drag between coordinates |
| `computer_use_press_key` | `press_key` | Send a key or combination (xdotool syntax) |
| `computer_use_type_text` | `type_text` | Type literal text |
| `computer_use_status` | — | Report policy + whether the signed broker verifies here |

`list_apps`, `get_app_state`, and `status` are `never_defer` (schemas always
loaded); the eight interaction tools defer behind `tool_search`, mirroring the
upstream "progressive activation" of inspection-before-interaction.

## Requirements

- **macOS only.** The tools drive the real desktop through the signed helper.
- The official **ChatGPT.app** installed at `/Applications/ChatGPT.app` with the
  Computer Use component, signed by OpenAI's Team ID (verified at call time).
- macOS Screen Recording / Accessibility (TCC) permission for the yolop process.

Off macOS — or without the signed bundle — the tools return a clear error and
`computer_use_status` reports `brokerVerified: false`.

## Install

```
/extensions install <path-to-this-crate>       # dev, after building the binary
```

Build the server binary into the package `bin/` (or put it on `PATH`) so
`yolop.capabilityServer.command` resolves:

```
cargo build --release -p yolop-extension-computer-use
mkdir -p bin && cp ../../target/release/yolop-extension-computer-use bin/
```

Then `/extensions doctor computer-use` grades the handshake, and
`/extensions enable computer-use` turns it on.

## Ported vs. deferred

**Ported (the functional + trust core):** signed-binary + OpenAI-Team-ID
verification; the exact `app-server --stdio` config that disables every
model/provider/plugin path; the `initialize → thread/start` (ephemeral,
zero-turn, Full access) `→ mcpServerStatus/list` (schema drift check)
`→ mcpServer/tool/call` sequence; failing closed on any `turn/*`/`item/*`
model activity; per-call process-group teardown; `press_key` alias
normalization.

**Deferred follow-ups** (macOS trust extras the upstream *service* layered on
top of the broker; called out here rather than silently dropped): the JSONL
audit log, the per-app `lockf` lease, the frontmost-focus telemetry /
background-preservation enforcement, and osascript app-identity
canonicalization. The zero-model-turn guarantee and signature verification —
what the security story actually rests on — are ported.

## License

MIT. Independent project; not an OpenAI product and not endorsed by OpenAI.

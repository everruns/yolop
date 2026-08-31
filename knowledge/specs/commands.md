---
type: Product Specification
title: Commands Specification
description: Defines the commands specification contract for Yolop.
---

# Commands Specification

## Abstract

yolop exposes user actions as **commands**. Most are slash commands; the TUI
also accepts `!<command>` and `!shell <command>` as terminal-local shell
aliases for the same capability command as `/shell`. Every command is
contributed by a **capability** (`Capability::commands()`), so each host's
command surface
is sourced solely from `runtime.list_commands(session_id)`, the source of
truth for that host's palette, `/help`, and completion. There is no hard-coded
command table on any host.

The *set* of commands differs by host, because capabilities are gated per
session (see [Host gating](#required-behavior)). The TUI registers the
terminal-side commands and so lists them; the ACP server and `--print` do not,
so those commands never appear in their advertised command lists. What is
common is the mechanism: whatever a host advertises comes from its registry,
never a parallel hard-coded list.

A command's `CommandSource` declares *who executes it*. yolop uses three
execution targets; the third (client/terminal) is a yolop convention layered on
top of the runtime's two, not a separate `CommandSource` variant.

## Execution targets

1. **System**: the **runtime** executes it via `runtime.execute_command`,
   returning a `CommandResult { success, message }` the host renders inline.
   Example: `/setup` opens guided setup, while `/setup status`, `/setup login <provider>`,
   and `/setup reauthenticate <provider>` inspect or start provider authentication;
   `/shell <command>` runs the existing bounded bash tool; `/undo`, `/redo`, and
   `/rewind` preview and confirm durable session restores; `/goal <condition>`
   starts an autonomous completion loop (see [`goal.md`](./goal.md)).

2. **Skill**: the **LLM** executes it. The literal `/name args` text is
   forwarded as a chat turn so the model activates the skill. Skill commands are
   contributed by the skills capability (see [`skills.md`](./skills.md)).

3. **Client (terminal-side)**: the **host** executes it, because the effect is
   on the terminal surface the runtime cannot reach (open an overlay, clear the
   transcript, quit, print local info). These are declared as ordinary `System`
   commands; on execute, their capability emits a typed `UiCommand` through an
   injected host UI port instead of returning text. The host's event loop drains
   the port and applies the effect. Commands: `/help`, `/tools`, `/mcp`,
   `/cwd`, `/model`, `/effort`, `!<command>` / `!shell <command>` (also
   accepted as `/shell`), `/clear`, `/quit`.

## Why client commands use a host port, not a new `CommandSource`

The runtime's `execute_command` can only return a `CommandResult` string; it has
no way to clear a transcript or open an overlay. Rather than add a second,
non-capability command path in the host, yolop injects a host UI port
(`HostUi`) into the capability at construction, the same dependency-injection
pattern `ModelsCapability` already uses for its settings/provider stores. The
capability requests an effect (`UiCommand`); the host, the only thing that can
, performs it. This keeps all commands in one registry, keeps them pluggable
(remove the capability and its commands disappear from the UI; swap it and they
reroute), and requires **no `everruns-core` change**.

A *portable, plugin-contributed* client command, one that arbitrary hosts honor
without each implementing a shared port, would instead need a first-class
`CommandSource::External` upstream. That was proposed (Linear EVE-520) and
**canceled** as unnecessary for yolop, whose terminal commands are
host-intrinsic. The note is kept here so the rationale is not lost if the
portable case ever arises.

## Required behavior

1. **Single registry.** The palette, `/help`, and completion read only
   `runtime.list_commands`. Removing a capability removes its commands; no host
   keeps a parallel list.
2. **Uniform dispatch.** The host looks a typed command up in the registry and
   routes by `CommandSource`: `System`/client → `runtime.execute_command`;
   `Skill` → forward as a chat turn. `/exit` is an accepted alias for `/quit`.
   Interactive hosts also accept `!<command>` and `!shell <command>` as
   shortcuts for `/shell <command>`.
3. **Client effects are host-applied.** A client command's `execute_command`
   returns an empty `CommandResult` and emits a `UiCommand`; the host applies
   every queued `UiCommand` before the next render. The `UiCommand` vocabulary
   is the shared contract between client capabilities and the host, a genuinely
   new on-screen effect is a host change, not a drop-in.
4. **Natural-language dispatch.** The `agent_commands` capability exposes a
   model-facing `run_command` tool and a prompt contribution describing it.
   When the user asks for a command in ordinary prose (for example, "exit" or
   "re-authenticate my provider"), the model invokes that tool instead of
   telling the user to type the slash command. `run_command` covers the
   **whole registry** of the host it runs on, not a curated subset, and holds no
   allowlist of its own: a name is resolved against `runtime.list_commands` and
   run through `runtime.execute_command`, exactly as typing it would, so client
   commands reach their capability and emit the same `UiCommand` as the typed
   path. `command: help` returns the live list, and an unknown name answers with
   it too. Two commands stay out by design: `Skill` commands, which activate by
   prompt rather than by a runtime effect, and `/shell`, which is typed-only
   because the agent already has the `bash` tool.

   The tool is registered on **every host**, because every host has a registry;
   what differs is what that registry holds (host gating below). The only
   host-specific detail is read-back: where a `HostUi` exists, the
   informational client commands (`/mcp`, `/tools`, `/help`, `/cwd`) are
   dispatched through it so the tool result carries the transcript lines the
   host printed instead of the empty `CommandResult` they return by design.
   `run_command` does not create a second command registry.
5. **Host gating.** Client commands are enabled only for a host that can apply
   them. `BuildOptions::client_commands` registers `ClientCommandsCapability`
   and enables its harness id; the interactive TUI sets it, while ACP and
   `--print` leave it off and therefore neither advertise nor dispatch terminal
   commands. `AgentCommandsCapability` is not gated that way: it is registered
   and enabled everywhere, so a `--print` or ACP session can run the commands
   its own registry holds (`/setup`, `/background`, `/undo`, …) and simply has
   no terminal commands to find. See [`acp.md`](./acp.md) for how the remaining
   `System`/`Skill` commands surface over ACP.

## Ownership boundary

- `CommandDescriptor`, `CommandSource` (`System`/`Skill`), `CommandResult`, and
  `execute_command` are owned by `everruns-core`.
- This spec owns yolop's command surface: the single-registry contract, the
  client/terminal execution target, the `HostUi`/`UiCommand` port, the
  `CommandDispatch` registry port `run_command` uses, and the host gating. The
  terminal commands live in `src/capabilities/client_commands.rs`; `run_command`
  and its registry port live in `src/capabilities/agent_commands.rs`; the host
  port lives in `src/tui/host_ui.rs` and the registry port is implemented by
  `RuntimeCommandDispatch` in `src/runtime/mod.rs`.

## Related

- [`skills.md`](./skills.md), `Skill`-source commands.
- [`acp.md`](./acp.md), how commands surface over the ACP transport.

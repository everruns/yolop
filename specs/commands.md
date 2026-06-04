# Commands Specification

## Abstract

yolop exposes user actions as **slash commands**. Every command is contributed
by a **capability** (`Capability::commands()`), so a single registry —
`runtime.list_commands(session_id)` — is the source of truth for the command
palette, `/help`, and completion across every host (the TUI and the ACP
server). There is no hard-coded command table.

A command's `CommandSource` declares *who executes it*. yolop uses three
execution targets; the third (client/terminal) is a yolop convention layered on
top of the runtime's two, not a separate `CommandSource` variant.

## Execution targets

1. **System** — the **runtime** executes it via `runtime.execute_command`,
   returning a `CommandResult { success, message }` the host renders inline.
   Example: `/setup` and its subcommands mutate provider/model/token settings.

2. **Skill** — the **LLM** executes it. The literal `/name args` text is
   forwarded as a chat turn so the model activates the skill. Skill commands are
   contributed by the skills capability (see [`skills.md`](./skills.md)).

3. **Client (terminal-side)** — the **host** executes it, because the effect is
   on the terminal surface the runtime cannot reach (open an overlay, clear the
   transcript, quit, print local info). These are declared as ordinary `System`
   commands; on execute, their capability emits a typed `UiCommand` through an
   injected host UI port instead of returning text. The host's event loop drains
   the port and applies the effect. Commands: `/help`, `/tools`, `/cwd`,
   `/model`, `/effort`, `/clear`, `/quit`.

## Why client commands use a host port, not a new `CommandSource`

The runtime's `execute_command` can only return a `CommandResult` string; it has
no way to clear a transcript or open an overlay. Rather than add a second,
non-capability command path in the host, yolop injects a host UI port
(`HostUi`) into the capability at construction — the same dependency-injection
pattern `SetupCapability` already uses for its settings/provider stores. The
capability requests an effect (`UiCommand`); the host — the only thing that can
— performs it. This keeps all commands in one registry, keeps them pluggable
(remove the capability and its commands disappear from the UI; swap it and they
reroute), and requires **no `everruns-core` change**.

A *portable, plugin-contributed* client command — one that arbitrary hosts honor
without each implementing a shared port — would instead need a first-class
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
3. **Client effects are host-applied.** A client command's `execute_command`
   returns an empty `CommandResult` and emits a `UiCommand`; the host applies
   every queued `UiCommand` before the next render. The `UiCommand` vocabulary
   is the shared contract between client capabilities and the host — a genuinely
   new on-screen effect is a host change, not a drop-in.
4. **Host gating.** Client commands are enabled only for a host that can apply
   them. `BuildOptions::client_commands` registers `ClientCommandsCapability`
   and enables its harness id; the interactive TUI sets it, while ACP and
   `--print` leave it off and therefore neither advertise nor dispatch terminal
   commands. See [`acp.md`](./acp.md) for how the remaining `System`/`Skill`
   commands surface over ACP.

## Ownership boundary

- `CommandDescriptor`, `CommandSource` (`System`/`Skill`), `CommandResult`, and
  `execute_command` are owned by `everruns-core`.
- This spec owns yolop's command surface: the single-registry contract, the
  client/terminal execution target, the `HostUi`/`UiCommand` port, and the host
  gating. The terminal commands themselves live in
  `src/capabilities/client_commands.rs`; the port lives in `src/host_ui.rs`.

## Related

- [`skills.md`](./skills.md) — `Skill`-source commands.
- [`acp.md`](./acp.md) — how commands surface over the ACP transport.

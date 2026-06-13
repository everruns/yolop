# `btw` — ephemeral side questions

Status: implemented upstream; enabled in yolop.

## Why

Mid-task, users often have a question *about* the session — "what did that
error mean?", "which files have you touched so far?" — that should not become
part of the conversation. Sending it as a chat turn derails the main task,
pollutes history, and can trigger tool use. `/btw` answers such questions
out-of-band, like Claude Code's ephemeral overlay answer.

## What

`/btw <question>` is a `System` command (see [`commands.md`](./commands.md)):
the host dispatches it through `runtime.execute_command` and renders the
returned `CommandResult` inline. It is available on every host surface (TUI,
`--print`, ACP) because dispatch is registry-driven.

Required behavior:

1. **Same context as the main turn.** The side answer is produced from the
   session's assembled turn context (merged system prompt — capability
   additions, `AGENTS.md`, memory — plus the message history after capability
   filters), so it can genuinely answer questions about the session.
2. **Tool-less, answers exactly once.** Tools and tool search are disabled
   for the side call; a dedicated system instruction tells the model to
   answer once and ask no follow-ups.
3. **Ephemeral.** Nothing is persisted: no message, no event, no history
   change. The next main turn is byte-identical to one where `/btw` never
   happened.
4. **Live model.** The call uses the session's currently resolved
   model/provider; reasoning effort comes from per-invocation controls.
5. **Classified failures.** Provider errors come back as
   `CommandResult { success: false }` with a stable `error_code`, not as a
   hard error. A missing/empty question is rejected before any LLM call.

## Ownership boundary

As of everruns `0.11.0` the entire `/btw` flow lives upstream:
`everruns_core::capabilities::BtwCapability` implements `execute_command`
end to end against a new `CommandHost` abstraction (the EVE-543 contract
extension). The host supplies two facilities — the assembled turn context
(`turn_context`) and a session-scoped, tool-less completion (`completion`) —
so the one capability implementation serves every host. The embedded runtime
wires a store-backed `CommandHost` (`StoreCommandHost`) into
`InProcessRuntime::execute_command` automatically; the server uses its own
host. Dangling tool calls are patched with synthetic "cancelled" results
inside the host's completion path.

yolop therefore owns no `/btw` logic. It only **registers** the upstream
`BtwCapability` in the capability registry and **enables**
`BTW_CAPABILITY_ID` on the coding harness (both in `src/runtime.rs`). There
is no longer a vendored executor.

## Related

- [`commands.md`](./commands.md) — command registry, dispatch, host gating.

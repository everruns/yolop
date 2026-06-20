# User ask — request tracking and turn-end validation

Status: implemented in yolop (`yolop_user_ask` capability). **Disabled by default** —
enable via `[[capabilities]]` in `settings.toml` or `set_config key=capabilities`.

## Why

Users state what they want in natural language and often refine or change direction
over several turns. The agent needs a durable record of the current request,
independent of `/goal` completion loops, so it can stay aligned and the host can
judge whether the latest turn actually addressed what was asked.

`/goal` is for autonomous multi-turn work toward a verifiable end state with
auto-continuation. User ask is lighter: one tracked request per session, revision
history when the user pivots, and a post-turn evaluator that reports achieved,
blocked, or in progress — without starting another turn automatically.

## Behavior

| Surface | Effect |
| --- | --- |
| User message (TUI / `--print`) | Host records the message as the active user ask (superseding the previous ask in revision history). |
| `set_user_ask` tool | Agent records or replaces the tracked ask (for refinement or when the user pivots in conversation). |
| `clear_user_ask` tool | Clears the tracked ask when the user abandons the request. |
| `/ask <text>` | Set or replace the tracked ask manually. |
| `/ask` | Show status: current ask, evaluated turns, last outcome, revision history. |
| `/ask clear` | Clear the tracked ask (`stop`, `off`, `reset`, `none`, `cancel` are aliases). |
| `/clear` | Also clears the tracked user ask (new conversation). |

After each agent turn while a user ask is active:

1. The host calls an internal evaluator through `CommandHost::completion` — no
   tools, nothing extra persisted beyond the evaluation result.
2. The evaluator returns JSON
   `{"outcome": "achieved"|"blocked"|"in_progress", "reason": "..."}` judged only
   from the conversation transcript.
3. The host surfaces the result as a system message (`user ask achieved`, `user ask
   blocked`, or `user ask in progress`).
4. Achieved and blocked outcomes deactivate the ask; a new user message starts a
   fresh ask. In-progress keeps the ask active for the next turn.

Unlike `/goal`, there is no auto-continuation loop.

## Enabling

Off the default harness. Opt in with:

```toml
[[capabilities]]
ref = "yolop_user_ask"
```

Or conversationally via `set_config key=capabilities json={"ref":"yolop_user_ask"}`.
Takes effect on the next run.

## System prompt

The capability contributes a `<capability id="yolop_user_ask">` block each turn
with the current ask, optional revision history, and last evaluation. It instructs
the model to call `set_user_ask` when the user changes direction.

## Hosts

| Host | Support |
| --- | --- |
| Interactive TUI | Records user prompts, end-of-turn evaluation, `? ask (N)` status indicator. |
| `--print` | Records the prompt, evaluates after the turn. |
| ACP | Tools and `/ask` are listed; end-of-turn evaluation requires the TUI/print host today. |

## Persistence

Active asks are stored in `<session_dir>/user_ask.json` so a resumed session
(`--session`) restores the ask. Evaluated-turn counters reset on resume.

## Independence from `/goal`

- Separate store, capability id, tools, and evaluator.
- Either capability can be disabled in `[[capabilities]]` without affecting the other.
- `/goal` may auto-continue turns; user ask only records and evaluates.

## Ownership

- `UserAskCapability` and `UserAskStore` live in yolop (`src/capabilities/user_ask.rs`,
  `src/user_ask.rs`).
- Evaluator calls use upstream `CommandHost::completion` (everruns-core).

## Related

- [`goal.md`](./goal.md) — autonomous completion loops.
- [`conversational-control.md`](./conversational-control.md) — `set_user_ask` /
  `clear_user_ask` tools.
- [`commands.md`](./commands.md) — `/ask` in the command registry.

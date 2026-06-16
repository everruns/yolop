# `/goal` — autonomous completion loops

Status: implemented in yolop (`yolop_goal` capability).

## Why

Multi-step coding work often has a verifiable end state — all tests pass, a
refactor is complete, a checklist is satisfied — but the agent stops after each
turn and waits for another prompt. Claude Code's `/goal` command (v2.1.139+)
showed that a session-scoped completion condition plus a lightweight evaluator
after every turn removes that bottleneck without giving the working model
authority to judge its own completion.

yolop mirrors that pattern: one active condition per session, automatic
continuation until a separate tool-less evaluator confirms the condition from
the transcript.

## Behavior

| Invocation | Effect |
| --- | --- |
| `/goal <condition>` | Replace any active goal, persist it, and start a turn immediately with the condition as the directive. |
| `/goal` | Show status: condition, elapsed time, evaluated turns, session tokens, last evaluator reason. |
| `/goal clear` | Clear the active goal (`stop`, `off`, `reset`, `none`, `cancel` are aliases). |
| `/clear` | Also clears an active goal (new conversation). |

After each agent turn while a goal is active:

1. The host calls an internal evaluator through the same `CommandHost`
   completion path as `/btw` — no tools, nothing extra persisted.
2. The evaluator returns JSON `{"met": bool, "reason": "..."}` judged only from
   the conversation transcript.
3. If `met` is true, the goal clears and the host prints `goal achieved`.
4. If `met` is false, the host starts another turn with the evaluator reason
   as guidance.

Conditions may be up to 4,000 characters. Users can bound runs in the condition
itself (for example `or stop after 20 turns`); the evaluator judges that clause
from the transcript, matching the Claude Code contract.

## Hosts

| Host | Support |
| --- | --- |
| Interactive TUI | Full loop, `◎ goal (N)` indicator in the status bar. |
| `--print` | `yolop -p "/goal <condition>"` runs the loop to completion. |
| ACP | Command is listed; loop continuation requires the TUI/print host today. |

## Persistence

Active goals are stored in `<session_dir>/goal.json` so a resumed session
(`--session`) restores the condition. Timer and evaluated-turn counters reset
on resume; an already-achieved goal is not restored as active.

## Ownership

- `GoalCapability` and `GoalStore` live in yolop (`src/capabilities/goal.rs`,
  `src/goal.rs`).
- Evaluator calls use upstream `CommandHost::completion` (everruns-core); no
  `everruns-core` API changes are required.

## Related

- [`commands.md`](./commands.md) — single command registry and dispatch.
- [Claude Code `/goal` docs](https://code.claude.com/docs/en/goal) — upstream
  UX reference.

# Trajectory export (ATIF)

Status: implemented.

## Why

Benchmark harnesses and trajectory tooling (Harbor, Mira eval studies,
trajectory viewers) consume agent runs in the Agent Trajectory Interchange
Format ([ATIF, harbor-framework RFC 0001](https://github.com/harbor-framework/harbor/blob/main/rfcs/0001-trajectory-format.md))
rather than yolop's internal JSONL event log. A first-class export lets any
yolop run feed those tools without a bespoke converter per consumer.

## What

`--trajectory-out <path>` writes the whole session — including events replayed
on `--session` resume — as a single ATIF-v1.7 JSON document when the run ends.
It works in the interactive TUI and in headless `-p/--print` mode (including
failed turns and `/goal` runs); `--acp` ignores it because the editor owns the
session lifecycle there.

`src/atif.rs` owns the serializer: it folds the runtime event log into steps
(user input → user step, each reason/act iteration → one agent step with
reasoning, tool calls, and tool results keyed by `source_call_id`, token usage
into per-step and final metrics). Export is best-effort and never fails the
run.

## Non-goals

- Not a replay or resume format — the per-session JSONL log stays the local
  source of truth (see `src/session_log.rs`).
- No image/multimodal export yet; text content only.

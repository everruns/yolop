"""Extract run metrics from a yolop ``events.jsonl`` session log.

yolop persists one serialized everruns ``Event`` per line. We only need a few
event types (see ``src/session_log.rs`` and everruns-core ``events.rs``):

* ``output.message.completed`` — one per assistant LLM completion; carries
  ``data.usage`` with token counts (incl. cache read/creation).
* ``input.message``           — user/tool-feedback messages.
* ``tool.completed``          — one per finished tool call, with ``tool_name``
  and ``success``.
* ``turn.completed``          — one per agent turn; carries ``duration_ms`` and
  an aggregate ``usage`` we use as a fallback / cross-check.

Token totals are summed from ``output.message.completed`` because that is the
most granular and avoids double counting if a turn's aggregate overlaps.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Iterator

from .models import RunMetrics, TokenUsage


def _iter_events(path: Path) -> Iterator[dict]:
    with path.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                # A partially written trailing line (crash/SIGKILL) is skipped
                # rather than failing the whole run's metrics.
                continue


def _add_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    tokens.input_tokens += int(usage.get("input_tokens") or 0)
    tokens.output_tokens += int(usage.get("output_tokens") or 0)
    tokens.cache_read_tokens += int(usage.get("cache_read_tokens") or 0)
    tokens.cache_creation_tokens += int(usage.get("cache_creation_tokens") or 0)


def _usage_cost(usage: dict | None) -> float:
    """Cost of one completion: provider actual if present, else yolop estimate."""
    if not usage:
        return 0.0
    actual = usage.get("actual_cost_usd")
    if actual is not None:
        return float(actual)
    return float(usage.get("estimated_cost_usd") or 0.0)


def running_cost(session_log_path: str | Path) -> float:
    """Cheap incremental cost read for in-flight budget enforcement.

    Sums per-completion cost from ``output.message.completed`` events written so
    far. Cheap enough to call on a poll loop (the log is small and grows slowly).
    """
    path = Path(session_log_path)
    if not path.exists():
        return 0.0
    total = 0.0
    for ev in _iter_events(path):
        if ev.get("type") == "output.message.completed":
            total += _usage_cost((ev.get("data") or {}).get("usage"))
    return total


def extract_metrics(session_log_path: str | Path) -> RunMetrics:
    """Parse ``events.jsonl`` into a :class:`RunMetrics`.

    Wall time (``wall_time_s``) is set by the runner, not here. This function
    fills ``agent_reported_time_s`` from summed turn durations, which excludes
    harness/setup overhead.
    """

    path = Path(session_log_path)
    m = RunMetrics()
    if not path.exists():
        return m

    # yolop's one-shot (`--print`) mode does not emit turn.completed; it emits
    # one reason.completed per agentic iteration (each carries duration_ms).
    # Prefer turn.completed when present (interactive/multi-turn), otherwise use
    # reason.completed as the iteration/turn count and duration source.
    turn_duration_ms = 0
    reason_duration_ms = 0
    saw_turn = False

    for ev in _iter_events(path):
        etype = ev.get("type", "")
        data = ev.get("data") or {}

        if etype == "output.message.completed":
            m.assistant_messages += 1
            m.llm_calls += 1
            _add_usage(m.tokens, data.get("usage"))
            m.cost_usd = round(m.cost_usd + _usage_cost(data.get("usage")), 6)
            # Actual effort applied for this completion (yolop records it in the
            # message metadata); take the latest seen.
            meta = (data.get("message") or {}).get("metadata") or {}
            if meta.get("reasoning_effort"):
                m.reasoning_effort = meta["reasoning_effort"]
        elif etype == "input.message":
            m.user_messages += 1
        elif etype == "tool.completed":
            m.tool_calls += 1
            name = data.get("tool_name") or "unknown"
            m.tools_used[name] = m.tools_used.get(name, 0) + 1
            if data.get("success") is False:
                m.tool_calls_failed += 1
        elif etype == "reason.completed":
            m.iterations += 1
            dur = data.get("duration_ms")
            if dur is not None:
                reason_duration_ms += int(dur)
        elif etype == "turn.completed":
            saw_turn = True
            m.turns += 1
            dur = data.get("duration_ms")
            if dur is not None:
                turn_duration_ms += int(dur)

    if not saw_turn:
        # Fall back to agentic iterations as the turn count for one-shot runs.
        m.turns = m.iterations
        if reason_duration_ms:
            m.agent_reported_time_s = round(reason_duration_ms / 1000.0, 3)
    elif turn_duration_ms:
        m.agent_reported_time_s = round(turn_duration_ms / 1000.0, 3)
    return m

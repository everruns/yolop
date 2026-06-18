"""Metrics extraction for non-yolop agents.

Each CLI streams a different newline-delimited JSON event log; these parsers map
each to the common :class:`RunMetrics`. They are deliberately tolerant — a run
that is killed (timeout/budget) leaves a truncated log, and external schemas
drift between tool versions, so missing fields degrade to zeros rather than
raising. ``cost_usd`` prefers the tool's own reported cost and falls back to a
``price``-based estimate (see :mod:`.pricing`).

Field references:
* claude-code ``--output-format stream-json``: ``assistant``/``user``/``result``
  events; ``result`` carries ``total_cost_usd``, ``num_turns``, ``duration_ms``
  and authoritative cumulative ``usage``.
* codex ``exec --json``: ``item.completed``/``turn.completed`` events; usage on
  ``turn.completed`` (``cached_input_tokens`` = cache read); no cost reported.
* pi ``--mode json``: ``message_end``/``tool_execution_end``/``turn_end``;
  ``message_end.usage`` has camelCase counts and ``usage.cost.total``.
"""

from __future__ import annotations

from pathlib import Path

from .metrics import _iter_events
from .models import RunMetrics, TokenUsage
from .pricing import cost_from_usage


def _finalize_cost(m: RunMetrics, reported: float | None, price: dict | None) -> None:
    if reported is not None:
        m.cost_usd = round(reported, 6)
    else:
        est = cost_from_usage(m.tokens, price)
        if est is not None:
            m.cost_usd = est


# --------------------------------------------------------------------------- #
# claude-code
# --------------------------------------------------------------------------- #
def _claude_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    tokens.input_tokens += int(usage.get("input_tokens") or 0)
    tokens.output_tokens += int(usage.get("output_tokens") or 0)
    tokens.cache_read_tokens += int(usage.get("cache_read_input_tokens") or 0)
    tokens.cache_creation_tokens += int(usage.get("cache_creation_input_tokens") or 0)


def extract_claude_code(log_path: str | Path, price: dict | None = None) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m

    summed = TokenUsage()
    final_usage: dict | None = None
    reported_cost: float | None = None

    for ev in _iter_events(path):
        etype = ev.get("type")
        if etype == "assistant":
            msg = ev.get("message") or {}
            m.assistant_messages += 1
            m.llm_calls += 1
            _claude_usage(summed, msg.get("usage"))
            for block in msg.get("content") or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    m.tool_calls += 1
                    name = block.get("name") or "unknown"
                    m.tools_used[name] = m.tools_used.get(name, 0) + 1
        elif etype == "user":
            content = (ev.get("message") or {}).get("content")
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "tool_result" \
                            and block.get("is_error"):
                        m.tool_calls_failed += 1
        elif etype == "result":
            if ev.get("num_turns") is not None:
                m.turns = int(ev["num_turns"])
            if ev.get("duration_ms") is not None:
                m.agent_reported_time_s = round(int(ev["duration_ms"]) / 1000.0, 3)
            if ev.get("total_cost_usd") is not None:
                reported_cost = float(ev["total_cost_usd"])
            if isinstance(ev.get("usage"), dict):
                final_usage = ev["usage"]

    # Prefer the result event's authoritative cumulative usage; fall back to the
    # per-assistant sum if the run ended before a result was emitted.
    if final_usage is not None:
        _claude_usage(m.tokens, final_usage)
    else:
        m.tokens = summed
    m.iterations = m.llm_calls
    if not m.turns:
        m.turns = m.iterations
    _finalize_cost(m, reported_cost, price)
    return m


# --------------------------------------------------------------------------- #
# codex
# --------------------------------------------------------------------------- #
def _codex_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    # Codex's input_tokens is the full prompt count *including* the cached
    # portion, so subtract it to get the non-cached input (and avoid double
    # counting cached tokens in cost), matching how claude/pi report.
    cached = int(usage.get("cached_input_tokens") or usage.get("cache_read_tokens") or 0)
    tokens.input_tokens += max(int(usage.get("input_tokens") or 0) - cached, 0)
    tokens.output_tokens += int(usage.get("output_tokens") or 0)
    tokens.cache_read_tokens += cached


def extract_codex(log_path: str | Path, price: dict | None = None) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m

    summed = TokenUsage()
    cumulative: dict | None = None  # older schema's total_token_usage, if present

    for ev in _iter_events(path):
        msg = ev.get("msg") or {}
        etype = ev.get("type") or msg.get("type")
        item = ev.get("item") or {}
        itype = item.get("type")

        if etype == "item.completed" and itype == "command_execution":
            m.tool_calls += 1
            m.tools_used["command"] = m.tools_used.get("command", 0) + 1
            if item.get("exit_code") not in (0, None):
                m.tool_calls_failed += 1
        elif etype == "item.completed" and itype == "file_change":
            m.tool_calls += 1
            m.tools_used["file_change"] = m.tools_used.get("file_change", 0) + 1
        elif etype == "item.completed" and itype == "agent_message":
            m.assistant_messages += 1
        elif etype == "exec_command_end":  # older schema
            m.tool_calls += 1
            m.tools_used["command"] = m.tools_used.get("command", 0) + 1
            if msg.get("exit_code") not in (0, None):
                m.tool_calls_failed += 1
        elif etype == "agent_message":  # older schema
            m.assistant_messages += 1
        elif etype == "token_count":  # older schema, cumulative
            info = msg.get("info") or {}
            cumulative = info.get("total_token_usage") or cumulative

        if etype == "turn.completed":
            m.turns += 1
            if isinstance(ev.get("usage"), dict):
                _codex_usage(summed, ev["usage"])

    if cumulative is not None:
        _codex_usage(m.tokens, cumulative)
    else:
        m.tokens = summed
    m.llm_calls = m.assistant_messages
    m.iterations = m.turns or m.assistant_messages
    if not m.turns:
        m.turns = m.iterations
    _finalize_cost(m, None, price)  # codex reports no cost
    return m


# --------------------------------------------------------------------------- #
# pi
# --------------------------------------------------------------------------- #
def _pi_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    tokens.input_tokens += int(usage.get("input") or 0)
    tokens.output_tokens += int(usage.get("output") or 0)
    tokens.cache_read_tokens += int(usage.get("cacheRead") or 0)
    tokens.cache_creation_tokens += int(usage.get("cacheWrite") or 0)


def extract_pi(log_path: str | Path, price: dict | None = None) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m

    reported_cost = 0.0
    saw_cost = False

    for ev in _iter_events(path):
        etype = ev.get("type")
        if etype == "message_end":
            usage = (ev.get("message") or {}).get("usage") or ev.get("usage") or {}
            m.assistant_messages += 1
            m.llm_calls += 1
            _pi_usage(m.tokens, usage)
            total = (usage.get("cost") or {}).get("total")
            if total is not None:
                reported_cost += float(total)
                saw_cost = True
        elif etype == "tool_execution_end":
            m.tool_calls += 1
            name = ev.get("toolName") or "unknown"
            m.tools_used[name] = m.tools_used.get(name, 0) + 1
            result = ev.get("result")
            if isinstance(result, dict) and (result.get("isError") or result.get("is_error")):
                m.tool_calls_failed += 1
        elif etype == "turn_end":
            m.turns += 1

    m.iterations = m.turns or m.llm_calls
    if not m.turns:
        m.turns = m.iterations
    _finalize_cost(m, reported_cost if saw_cost else None, price)
    return m

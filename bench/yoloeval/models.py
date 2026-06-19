"""Shared data structures passed between datasets, agents, and the runner.

Kept deliberately small and serializable: every field here ends up in the
result JSON committed to the repo, so they double as the on-disk schema.
"""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass, field
from typing import Any, Optional


@dataclass
class Instance:
    """A single benchmark task, normalized across benchmarks.

    ``extra`` carries benchmark-specific fields (e.g. SWE-bench's
    ``FAIL_TO_PASS``) so the evaluator can recover everything it needs without
    the agent having to know about them.
    """

    instance_id: str
    problem_statement: str
    repo: Optional[str] = None
    base_commit: Optional[str] = None
    extra: dict[str, Any] = field(default_factory=dict)


@dataclass
class TokenUsage:
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0

    @property
    def total_tokens(self) -> int:
        return self.input_tokens + self.output_tokens

    def to_dict(self) -> dict[str, int]:
        d = dataclasses.asdict(self)
        d["total_tokens"] = self.total_tokens
        return d


@dataclass
class RunMetrics:
    """Operational metrics for one agent run, extracted from the session log."""

    wall_time_s: float = 0.0
    agent_reported_time_s: Optional[float] = None
    turns: int = 0
    # Agentic iterations (reason.completed); equals turns for one-shot runs.
    iterations: int = 0
    assistant_messages: int = 0
    user_messages: int = 0
    llm_calls: int = 0
    tool_calls: int = 0
    tool_calls_failed: int = 0
    tools_used: dict[str, int] = field(default_factory=dict)
    tokens: TokenUsage = field(default_factory=TokenUsage)
    # Reasoning effort actually applied per request (from the agent's log), which
    # may differ from the configured value: when unset, yolop picks a per-model
    # default. ``None`` if the agent doesn't expose it (claude-code/codex/pi).
    reasoning_effort: Optional[str] = None
    # USD cost summed from per-completion usage: provider-reported
    # ``actual_cost_usd`` when present, else yolop's ``estimated_cost_usd``.
    cost_usd: float = 0.0
    # Why the agent run ended: "completed", "timeout", "budget", or "error".
    stop_reason: str = "completed"

    def to_dict(self) -> dict[str, Any]:
        d = dataclasses.asdict(self)
        d["tokens"] = self.tokens.to_dict()
        return d


@dataclass
class AgentRun:
    """What an agent produces for one instance."""

    patch: str
    metrics: RunMetrics
    # Path to the raw session log (e.g. yolop events.jsonl). Not committed —
    # uploaded out of band; see bench/README.md.
    session_log_path: Optional[str] = None
    error: Optional[str] = None


@dataclass
class EvalResult:
    """What a benchmark evaluator reports for one prediction."""

    resolved: bool
    # Free-form per-benchmark detail (e.g. FAIL_TO_PASS/PASS_TO_PASS status).
    report: dict[str, Any] = field(default_factory=dict)
    error: Optional[str] = None

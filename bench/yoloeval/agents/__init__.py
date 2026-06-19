from .base import Agent
from .claude_code import ClaudeCodeAgent
from .codex import CodexAgent
from .pi import PiAgent
from .yolop import YolopAgent

__all__ = [
    "Agent",
    "YolopAgent",
    "ClaudeCodeAgent",
    "CodexAgent",
    "PiAgent",
    "build_agent",
]

_AGENTS = {
    "yolop": YolopAgent,
    "claude-code": ClaudeCodeAgent,
    "codex": CodexAgent,
    "pi": PiAgent,
}


def build_agent(config_name: str, spec: dict, defaults: dict | None = None) -> Agent:
    """Build an agent from a config-matrix entry.

    ``spec`` is one entry under ``configs:`` in the matrix YAML. ``agent``
    selects the adapter (default ``yolop``); remaining keys are passed through to
    the adapter constructor.
    """
    merged = dict(defaults or {})
    merged.update(spec or {})
    agent_type = merged.pop("agent", "yolop")
    try:
        cls = _AGENTS[agent_type]
    except KeyError:
        raise ValueError(f"unknown agent type: {agent_type!r}") from None
    return cls(config_name=config_name, **merged)

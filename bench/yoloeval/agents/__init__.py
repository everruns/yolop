from .base import Agent
from .yolop import YolopAgent

__all__ = ["Agent", "YolopAgent", "build_agent"]


def build_agent(config_name: str, spec: dict, defaults: dict | None = None) -> Agent:
    """Build an agent from a config-matrix entry.

    ``spec`` is one entry under ``configs:`` in the matrix YAML. ``agent``
    selects the adapter (default ``yolop``); remaining keys are passed through.
    """
    merged = dict(defaults or {})
    merged.update(spec or {})
    agent_type = merged.pop("agent", "yolop")
    if agent_type == "yolop":
        return YolopAgent(config_name=config_name, **merged)
    raise ValueError(f"unknown agent type: {agent_type!r}")

"""Pi Coding Agent CLI adapter.

Runs ``pi -p --mode json`` non-interactively in the checkout (pi operates on the
current working directory), with ``-a`` to auto-approve project-local edits. Pi
reports per-message cost in its JSON events, so ``cost_usd`` is taken directly.
Uses provider keys (``OPENAI_API_KEY`` / ``ANTHROPIC_API_KEY``) from the env.
"""

from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Any

from ..agent_metrics import extract_pi
from ..models import AgentRun, Instance
from .base import PROMPT_TEMPLATE, Agent
from ._proc import run_agent_process, tool_version


class PiAgent(Agent):
    name = "pi"

    def __init__(
        self,
        config_name: str,
        binary: str = "pi",
        provider: str | None = None,
        model: str | None = None,
        timeout: int = 1800,
        max_cost_usd: float | None = 5.0,
        price: dict | None = None,
        extra_args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ):
        self.config_name = config_name
        self.binary = binary
        self.provider = provider
        self.model = model
        self.timeout = timeout
        self.max_cost_usd = max_cost_usd
        self.price = price
        self.extra_args = extra_args or []
        self.env_overrides = env or {}
        self.config: dict[str, Any] = {
            "agent": self.name,
            "config_name": config_name,
            "provider": provider,
            "model": model,
            "max_cost_usd": max_cost_usd,
            "extra_args": self.extra_args,
            "pi_version": tool_version([binary, "--version"]),
        }

    def run(self, instance: Instance, workdir: str, session_dir: str) -> AgentRun:
        prompt = PROMPT_TEMPLATE.format(
            repo=instance.repo or "the", problem_statement=instance.problem_statement
        )
        cmd = [self.binary, "-p", prompt, "--mode", "json", "-a"]
        if self.provider:
            cmd += ["--provider", self.provider]
        if self.model:
            cmd += ["--model", self.model]
        cmd += self.extra_args

        env = dict(os.environ)
        env.update(self.env_overrides)
        log_path = Path(session_dir) / "pi.jsonl"

        start = time.monotonic()
        res = run_agent_process(
            cmd, env=env, name=self.name, cwd=workdir, timeout=self.timeout,
            stdout_log=log_path, max_cost_usd=self.max_cost_usd,
            cost_probe=lambda: extract_pi(log_path, self.price).cost_usd,
        )
        metrics = extract_pi(log_path, self.price)
        metrics.wall_time_s = round(time.monotonic() - start, 3)
        metrics.stop_reason = res.stop_reason
        return AgentRun(
            patch="", metrics=metrics, session_log_path=str(log_path), error=res.error
        )

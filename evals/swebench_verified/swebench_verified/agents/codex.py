"""OpenAI Codex CLI adapter.

Runs ``codex exec --json`` non-interactively against the checkout in
``--full-auto`` mode (workspace-write sandbox, no approval prompts) so edits land
in place. Codex reports token usage but no cost, so ``cost_usd`` is estimated
from the config's ``price`` block. Uses ``OPENAI_API_KEY`` from the environment.
"""

from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Any

from ..agent_metrics import extract_codex
from ..models import AgentRun, Instance
from .base import PROMPT_TEMPLATE, Agent
from ._proc import run_agent_process, tool_version


class CodexAgent(Agent):
    name = "codex"

    def __init__(
        self,
        config_name: str,
        binary: str = "codex",
        model: str | None = None,
        timeout: int = 1800,
        max_cost_usd: float | None = 5.0,
        price: dict | None = None,
        sandbox: str = "full-auto",
        extra_args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ):
        self.config_name = config_name
        self.binary = binary
        self.model = model
        self.timeout = timeout
        self.max_cost_usd = max_cost_usd
        self.price = price
        # "full-auto" = workspace-write sandbox; "bypass" fully disables the
        # sandbox (the checkout is already isolated) if the sandbox misbehaves.
        self.sandbox = sandbox
        self.extra_args = extra_args or []
        self.env_overrides = env or {}
        self.config: dict[str, Any] = {
            "agent": self.name,
            "config_name": config_name,
            "model": model,
            "max_cost_usd": max_cost_usd,
            "price": price,
            "sandbox": sandbox,
            "extra_args": self.extra_args,
            "codex_version": tool_version([binary, "--version"]),
        }

    def run(self, instance: Instance, workdir: str, session_dir: str) -> AgentRun:
        prompt = PROMPT_TEMPLATE.format(
            repo=instance.repo or "the", problem_statement=instance.problem_statement
        )
        cmd = [self.binary, "exec", prompt, "--json", "--cd", workdir]
        if self.model:
            cmd += ["--model", self.model]
        if self.sandbox == "bypass":
            cmd += ["--dangerously-bypass-approvals-and-sandbox"]
        else:
            cmd += ["--full-auto"]
        cmd += self.extra_args

        env = dict(os.environ)
        env.update(self.env_overrides)
        log_path = Path(session_dir) / "codex.jsonl"

        start = time.monotonic()
        res = run_agent_process(
            cmd, env=env, name=self.name, cwd=workdir, timeout=self.timeout,
            stdout_log=log_path, max_cost_usd=self.max_cost_usd,
            cost_probe=lambda: extract_codex(log_path, self.price).cost_usd,
        )
        metrics = extract_codex(log_path, self.price)
        metrics.wall_time_s = round(time.monotonic() - start, 3)
        metrics.stop_reason = res.stop_reason
        return AgentRun(
            patch="", metrics=metrics, session_log_path=str(log_path), error=res.error
        )

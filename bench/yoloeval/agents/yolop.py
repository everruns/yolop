"""yolop agent adapter.

Runs the yolop binary non-interactively (``-p``) inside the instance checkout,
pointing ``--session-dir`` at a scratch dir so we get a self-contained
``events.jsonl`` to mine for metrics. Provider/model/reasoning come from the
config matrix entry.
"""

from __future__ import annotations

import os
import re
import subprocess
import time
import uuid
from pathlib import Path
from typing import Any

from ..metrics import extract_metrics, running_cost
from ..models import AgentRun, Instance, RunMetrics
from .base import Agent

PROMPT_TEMPLATE = """\
You are fixing a bug in the {repo} repository. The working directory is a clean \
checkout at the relevant base commit.

Resolve the following issue by editing the source code. Do not write new tests; \
the project's own hidden test suite will be used to verify your fix. Make the \
smallest change that correctly fixes the problem.

--- ISSUE ---
{problem_statement}
--- END ISSUE ---

When you are done, ensure the change is saved to the files on disk."""


class YolopAgent(Agent):
    name = "yolop"

    def __init__(
        self,
        config_name: str,
        binary: str,
        provider: str | None = None,
        model: str | None = None,
        reasoning_effort: str | None = None,
        timeout: int = 1800,
        max_cost_usd: float | None = 5.0,
        extra_args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ):
        self.config_name = config_name
        self.binary = binary
        self.provider = provider
        self.model = model
        self.reasoning_effort = reasoning_effort
        self.timeout = timeout
        # Per-run USD cap. yolop has no native cost cap, so the harness watches
        # the session log's running cost and kills the run when it's exceeded.
        self.max_cost_usd = max_cost_usd
        self.extra_args = extra_args or []
        self.env_overrides = env or {}
        self.config: dict[str, Any] = {
            "agent": self.name,
            "config_name": config_name,
            "provider": provider,
            "model": model,
            "reasoning_effort": reasoning_effort,
            "max_cost_usd": max_cost_usd,
            "extra_args": self.extra_args,
            **_yolop_version(binary),
        }

    def run(self, instance: Instance, workdir: str, session_dir: str) -> AgentRun:
        session_id = "session_" + uuid.uuid4().hex
        prompt = PROMPT_TEMPLATE.format(
            repo=instance.repo or "the",
            problem_statement=instance.problem_statement,
        )

        # yolop defaults to WorktreesMode::Auto and would move implementation
        # work into a linked git worktree on a new branch — our git diff of the
        # checkout would then see nothing. Force worktrees off via an isolated
        # config dir (dirs::config_dir() honors XDG_CONFIG_HOME) so edits land
        # in `workdir`.
        config_home = Path(session_dir) / "xdg_config"
        cfg_dir = config_home / "yolop"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "settings.toml").write_text('worktrees = "off"\n', encoding="utf-8")

        cmd = [self.binary, "-C", workdir]
        if self.provider:
            cmd += ["--provider", self.provider]
        if self.model:
            cmd += ["--model", self.model]
        if self.reasoning_effort:
            cmd += ["--reasoning-effort", self.reasoning_effort]
        cmd += self.extra_args
        cmd += ["--session", session_id, "--session-dir", session_dir, "-p", prompt]

        env = dict(os.environ)
        env["XDG_CONFIG_HOME"] = str(config_home)
        env.update(self.env_overrides)

        log_path = Path(session_dir) / session_id / "events.jsonl"
        error: str | None = None
        stop_reason = "completed"
        start = time.monotonic()

        proc = subprocess.Popen(
            cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        # Poll the process while watching wall-clock and the session log's
        # running cost; terminate on timeout or budget breach.
        while True:
            try:
                proc.wait(timeout=2)
                break
            except subprocess.TimeoutExpired:
                pass
            elapsed = time.monotonic() - start
            if elapsed > self.timeout:
                _kill(proc)
                error = f"yolop timed out after {self.timeout}s"
                stop_reason = "timeout"
                break
            if self.max_cost_usd is not None:
                cost = running_cost(log_path)
                if cost > self.max_cost_usd:
                    _kill(proc)
                    error = f"budget cap hit: ${cost:.2f} > ${self.max_cost_usd:.2f}"
                    stop_reason = "budget"
                    break

        stderr = (proc.stderr.read() if proc.stderr else "") or ""
        if stop_reason == "completed" and proc.returncode not in (0, None):
            error = f"yolop exited {proc.returncode}: {stderr[-2000:]}"
            stop_reason = "error"
        wall = time.monotonic() - start

        if log_path.exists():
            metrics = extract_metrics(log_path)
        else:
            metrics = RunMetrics()
            if error is None:
                error = f"no session log at {log_path}"
                stop_reason = "error"
        metrics.wall_time_s = round(wall, 3)
        metrics.stop_reason = stop_reason

        return AgentRun(
            patch="",  # runner computes the diff from the working tree
            metrics=metrics,
            session_log_path=str(log_path),
            error=error,
        )


def _kill(proc: subprocess.Popen) -> None:
    """Terminate a yolop process, escalating to SIGKILL if it lingers."""
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


def _yolop_version(binary: str) -> dict[str, str | None]:
    """Parse `yolop version` into structured metadata.

    The binary path itself is deliberately not recorded — it is machine-specific
    and not reproducibility-relevant. The version line looks like:
    ``yolop 0.3.0 (commit e6c0c9046607, everruns-runtime 0.14.0)``.
    """
    out = {"yolop_version": None, "yolop_commit": None, "everruns_runtime_version": None}
    try:
        proc = subprocess.run(
            [binary, "version"], capture_output=True, text=True, timeout=30
        )
        line = (proc.stdout or proc.stderr).strip().splitlines()[0]
    except Exception:
        return out
    if m := re.search(r"yolop\s+(\S+)", line):
        out["yolop_version"] = m.group(1)
    if m := re.search(r"commit\s+([0-9a-f]+)", line):
        out["yolop_commit"] = m.group(1)
    if m := re.search(r"everruns-runtime\s+(\S+?)\)?$", line):
        out["everruns_runtime_version"] = m.group(1)
    return out

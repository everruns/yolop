"""Shared subprocess driver for CLI coding agents.

Every adapter runs an external CLI to completion while the harness enforces two
bounds the tools don't all provide themselves: a wall-clock ``timeout`` and a
per-run USD ``max_cost_usd`` cap. Cost is observed through a ``cost_probe``
callback that reads the agent's own (incrementally written) session log, so the
cap mechanism is identical across agents even though each log format differs.
"""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence


@dataclass
class ProcResult:
    stop_reason: str  # "completed" | "timeout" | "budget" | "error"
    error: str | None
    wall_s: float
    returncode: int | None
    stderr_tail: str


def run_agent_process(
    cmd: Sequence[str],
    *,
    env: dict[str, str],
    name: str,
    cwd: str | Path | None = None,
    timeout: float = 1800,
    stdout_log: str | Path | None = None,
    max_cost_usd: float | None = None,
    cost_probe: Callable[[], float | None] | None = None,
    poll_s: float = 2.0,
) -> ProcResult:
    """Run ``cmd`` to completion, enforcing timeout and cost cap.

    If ``stdout_log`` is set the child's stdout is redirected there (for agents
    that stream their JSON event log to stdout); otherwise stdout is discarded
    (for agents like yolop that write their own log file). stderr is always
    captured so its tail can be surfaced on failure.
    """
    start = time.monotonic()
    stdout_f = open(stdout_log, "wb") if stdout_log else None
    try:
        proc = subprocess.Popen(
            list(cmd),
            env=env,
            cwd=str(cwd) if cwd else None,
            stdin=subprocess.DEVNULL,
            stdout=stdout_f or subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    finally:
        # The child holds its own dup of the fd; the parent handle isn't needed.
        if stdout_f:
            stdout_f.close()

    error: str | None = None
    stop_reason = "completed"
    while True:
        try:
            proc.wait(timeout=poll_s)
            break
        except subprocess.TimeoutExpired:
            # Still running at the poll interval — fall through to the
            # wall-clock and budget checks below, then keep polling.
            pass
        if time.monotonic() - start > timeout:
            _kill(proc)
            stop_reason, error = "timeout", f"{name} timed out after {timeout}s"
            break
        if max_cost_usd is not None and cost_probe is not None:
            try:
                cost = cost_probe()
            except Exception:
                cost = None
            if cost is not None and cost > max_cost_usd:
                _kill(proc)
                stop_reason = "budget"
                error = f"budget cap hit: ${cost:.2f} > ${max_cost_usd:.2f}"
                break

    stderr_tail = ""
    if proc.stderr:
        try:
            stderr_tail = proc.stderr.read().decode("utf-8", "replace")[-2000:]
        except Exception:
            # stderr tail is best-effort diagnostics; never fail the run over it.
            pass
    if stop_reason == "completed" and proc.returncode not in (0, None):
        stop_reason = "error"
        error = f"{name} exited {proc.returncode}: {stderr_tail}"
    return ProcResult(
        stop_reason=stop_reason,
        error=error,
        wall_s=round(time.monotonic() - start, 3),
        returncode=proc.returncode,
        stderr_tail=stderr_tail,
    )


def _kill(proc: subprocess.Popen) -> None:
    """Terminate a process, escalating to SIGKILL if it lingers."""
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


def tool_version(cmd: Sequence[str]) -> str | None:
    """Best-effort ``<binary> --version`` capture; ``None`` if unavailable."""
    try:
        out = subprocess.run(list(cmd), capture_output=True, text=True, timeout=30)
        line = (out.stdout or out.stderr).strip().splitlines()
        return line[0] if line else None
    except Exception:
        return None

"""Orchestrate the (instance x config) matrix.

For each instance and each agent config:

1. Check out the repo at ``base_commit`` into a scratch worktree.
2. Run the agent inside it (it edits files in place).
3. Capture the working-tree diff as the model patch.

After all agent runs for a config, hand the patches to the benchmark's
evaluator (Docker), then persist one result JSON per run plus a config summary.
"""

from __future__ import annotations

import shutil
import subprocess
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .agents import build_agent
from .datasets.base import Benchmark
from .models import AgentRun, Instance, RunMetrics
from . import results

CACHE_DIR = Path(__file__).resolve().parents[1] / ".cache"


def _git(args: list[str], cwd: str | Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args], cwd=str(cwd) if cwd else None, capture_output=True, text=True
    )


def checkout(instance: Instance, dest: Path) -> None:
    """Materialize ``instance.repo`` at ``base_commit`` into ``dest``.

    Uses a SHA-targeted shallow fetch so we don't clone full history.
    """
    dest.mkdir(parents=True, exist_ok=True)
    url = f"https://github.com/{instance.repo}.git"
    assert _git(["init", "-q"], dest).returncode == 0
    _git(["remote", "add", "origin", url], dest)
    fetch = _git(["fetch", "-q", "--depth", "1", "origin", instance.base_commit], dest)
    if fetch.returncode != 0:
        raise RuntimeError(f"fetch failed for {instance.instance_id}: {fetch.stderr.strip()}")
    co = _git(["checkout", "-q", "FETCH_HEAD"], dest)
    if co.returncode != 0:
        raise RuntimeError(f"checkout failed for {instance.instance_id}: {co.stderr.strip()}")


def capture_patch(workdir: Path) -> str:
    """Return the unified diff of all working-tree changes vs the base commit."""
    _git(["add", "-A"], workdir)
    diff = _git(["diff", "--cached", "--no-color"], workdir)
    return diff.stdout


def run_matrix(
    benchmark: Benchmark,
    defaults: dict[str, Any],
    configs: dict[str, dict],
    instance_ids: list[str] | None,
    run_id: str | None = None,
    do_eval: bool = True,
    keep_workdirs: bool = False,
) -> dict[str, Any]:
    run_id = run_id or f"yoloeval-{datetime.now(timezone.utc):%Y%m%d-%H%M%S}-{uuid.uuid4().hex[:6]}"
    instances = benchmark.load(instance_ids)
    if not instances:
        raise SystemExit("no instances matched the requested ids")

    print(f"[runner] run_id={run_id} benchmark={benchmark.name} "
          f"instances={len(instances)} configs={list(configs)}", flush=True)

    overall: dict[str, Any] = {"run_id": run_id, "configs": {}}

    for config_name, spec in configs.items():
        agent = build_agent(config_name, spec, defaults)
        print(f"\n[runner] === config {config_name} === {agent.config}", flush=True)

        runs: dict[str, AgentRun] = {}
        patches: dict[str, str] = {}
        started_at: dict[str, str] = {}

        for inst in instances:
            print(f"[runner] {config_name} :: {inst.instance_id} :: agent run", flush=True)
            workdir = CACHE_DIR / "work" / run_id / config_name / inst.instance_id
            session_dir = CACHE_DIR / "sessions" / run_id / config_name / inst.instance_id
            session_dir.mkdir(parents=True, exist_ok=True)
            started_at[inst.instance_id] = datetime.now(timezone.utc).isoformat()
            try:
                checkout(inst, workdir)
                run = agent.run(inst, str(workdir), str(session_dir))
                patch = capture_patch(workdir)
                run.patch = patch
            except Exception as e:  # keep the matrix going on a single failure
                run = AgentRun(patch="", metrics=RunMetrics(), error=f"runner: {e}")
                patch = ""
            runs[inst.instance_id] = run
            patches[inst.instance_id] = patch
            print(f"[runner] {config_name} :: {inst.instance_id} :: "
                  f"patch={len(patch)}B err={run.error!r}", flush=True)
            if not keep_workdirs:
                shutil.rmtree(workdir, ignore_errors=True)

        # Evaluate the whole config's predictions in one harness invocation.
        if do_eval:
            eval_results = benchmark.evaluate(patches, instances, f"{run_id}--{config_name}")
        else:
            eval_results = {}

        for inst in instances:
            run = runs[inst.instance_id]
            ev = eval_results.get(inst.instance_id)
            resolved = bool(ev.resolved) if ev else False
            eval_report = ev.report if ev else {}
            err = run.error or (ev.error if ev else None)
            record = results.build_record(
                benchmark=benchmark.name,
                config_name=config_name,
                instance_id=inst.instance_id,
                agent_config=agent.config,
                resolved=resolved,
                metrics=run.metrics.to_dict(),
                patch=run.patch,
                session_log_path=run.session_log_path,
                eval_report=eval_report,
                error=err,
                run_id=run_id,
                started_at=started_at[inst.instance_id],
            )
            path = results.save_result(record)
            print(f"[runner] saved {path} resolved={resolved}", flush=True)

        summary = results.summarize(benchmark.name, config_name)
        overall["configs"][config_name] = summary
        print(f"[runner] summary {config_name}: "
              f"{summary['resolved']}/{summary['instances']} resolved", flush=True)

    return overall

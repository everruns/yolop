"""Persist and summarize benchmark results in the repo.

Layout (committed, à la everruns/bashkit ``--save``)::

    bench/results/<benchmark>/<config>/<instance_id>.json   # one run
    bench/results/<benchmark>/<config>/summary.json         # rolled up

The per-run JSON is the durable record: config metadata + metrics + the model
patch + resolved flag. Raw session logs (``events.jsonl``) are referenced by
path only and live outside git (see ``bench/.gitignore``); uploading them is a
separate concern (see ``bench/README.md``).
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
RESULTS_DIR = Path(__file__).resolve().parents[1] / "results"
# Repo root, used to store machine-independent (relative) paths in results.
_REPO_ROOT = Path(__file__).resolve().parents[2]


def _portable_path(p: str | None) -> str | None:
    """Make a path repo-relative so committed results don't leak local prefixes."""
    if not p:
        return p
    try:
        return str(Path(p).resolve().relative_to(_REPO_ROOT))
    except ValueError:
        return p  # outside the repo: leave as-is


def result_path(benchmark: str, config_name: str, instance_id: str) -> Path:
    return RESULTS_DIR / benchmark / config_name / f"{instance_id}.json"


def save_result(record: dict[str, Any]) -> Path:
    path = result_path(record["benchmark"], record["config_name"], record["instance_id"])
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(record, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    return path


def build_record(
    *,
    benchmark: str,
    config_name: str,
    instance_id: str,
    agent_config: dict[str, Any],
    resolved: bool,
    metrics: dict[str, Any],
    patch: str,
    session_log_path: str | None,
    eval_report: dict[str, Any],
    error: str | None,
    run_id: str,
    started_at: str,
    evaluated: bool = True,
) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "benchmark": benchmark,
        "instance_id": instance_id,
        "config_name": config_name,
        "run_id": run_id,
        "resolved": resolved,
        # False until the patch has been scored. Records persisted right after
        # the agent run start unevaluated; `yoloeval eval` can score them later.
        "evaluated": evaluated,
        "error": error,
        "agent": agent_config,
        "metrics": metrics,
        "eval_report": eval_report,
        "patch": patch,
        "session_log_path": _portable_path(session_log_path),
        "timing": {
            "started_at": started_at,
            "finished_at": datetime.now(timezone.utc).isoformat(),
        },
    }


def summarize(benchmark: str, config_name: str) -> dict[str, Any]:
    """Aggregate every per-instance result for a config into ``summary.json``."""
    config_dir = RESULTS_DIR / benchmark / config_name
    runs = []
    for p in sorted(config_dir.glob("*.json")):
        if p.name == "summary.json":
            continue
        runs.append(json.loads(p.read_text(encoding="utf-8")))

    n = len(runs)
    resolved = sum(1 for r in runs if r.get("resolved"))
    errored = sum(1 for r in runs if r.get("error"))
    stop_reasons: dict[str, int] = {}
    for r in runs:
        sr = (r.get("metrics") or {}).get("stop_reason", "unknown")
        stop_reasons[sr] = stop_reasons.get(sr, 0) + 1

    def _sum(path: list[str]) -> float:
        total = 0.0
        for r in runs:
            cur: Any = r
            for k in path:
                cur = (cur or {}).get(k) if isinstance(cur, dict) else None
            if isinstance(cur, (int, float)):
                total += cur
        return total

    summary = {
        "benchmark": benchmark,
        "config_name": config_name,
        "instances": n,
        "resolved": resolved,
        "resolved_rate": round(resolved / n, 4) if n else 0.0,
        "errored": errored,
        "stop_reasons": stop_reasons,
        "totals": {
            "wall_time_s": round(_sum(["metrics", "wall_time_s"]), 1),
            "cost_usd": round(_sum(["metrics", "cost_usd"]), 4),
            "turns": int(_sum(["metrics", "turns"])),
            "tool_calls": int(_sum(["metrics", "tool_calls"])),
            "input_tokens": int(_sum(["metrics", "tokens", "input_tokens"])),
            "output_tokens": int(_sum(["metrics", "tokens", "output_tokens"])),
            "cache_read_tokens": int(_sum(["metrics", "tokens", "cache_read_tokens"])),
            "cache_creation_tokens": int(_sum(["metrics", "tokens", "cache_creation_tokens"])),
            "total_tokens": int(_sum(["metrics", "tokens", "total_tokens"])),
        },
        "generated_at": datetime.now(timezone.utc).isoformat(),
    }
    if n:
        summary["averages"] = {
            "wall_time_s": round(summary["totals"]["wall_time_s"] / n, 1),
            "cost_usd": round(summary["totals"]["cost_usd"] / n, 4),
            "turns": round(summary["totals"]["turns"] / n, 2),
            "tool_calls": round(summary["totals"]["tool_calls"] / n, 2),
            "total_tokens": round(summary["totals"]["total_tokens"] / n, 1),
        }

    (config_dir / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )
    return summary

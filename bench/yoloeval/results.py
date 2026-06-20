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
        # False until the patch has been scored. The runner persists a record as
        # soon as the agent run finishes (before the batched eval) with
        # evaluated=False; `yoloeval eval` can score it later and flip this true.
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
        # Cost efficiency — the "performance per dollar" headline. ``cost_usd``
        # is cache-aware (it comes from the driver's actual/estimated cost, which
        # prices cache reads/writes), so this is a fair cross-model comparison.
        total_cost = summary["totals"]["cost_usd"]
        summary["efficiency"] = {
            # Resolved instances per USD spent — higher is better (the score/$).
            "resolved_per_usd": round(resolved / total_cost, 4) if total_cost else 0.0,
            # Average USD to land one resolved instance — lower is better.
            # ``None`` when nothing resolved (avoids a divide-by-zero headline).
            "cost_per_resolved_usd": round(total_cost / resolved, 4) if resolved else None,
        }

    (config_dir / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )
    return summary


def leaderboard(benchmark: str, baseline: str | None = None) -> dict[str, Any]:
    """Rank every config for ``benchmark`` by cost efficiency (resolved per USD).

    Reads each ``<config>/summary.json`` and normalizes resolved-per-USD to a
    baseline config, so the output reads like a "performance per dollar"
    comparison (e.g. "2.6x value" vs a "1.0x baseline"). The baseline defaults to
    the least cost-efficient config with a *positive* resolved-per-USD (a config
    that resolved nothing, or has no recorded cost, would zero out every
    multiple, so it is never auto-selected); pass ``baseline`` to pin it to a
    specific config (e.g. the most capable/expensive one). Configs with no
    recorded cost (``cost_usd == 0``) are listed but unpriced — their
    ``value_multiple`` is ``None`` since value per dollar is undefined.
    """
    bench_dir = RESULTS_DIR / benchmark
    rows: list[dict[str, Any]] = []
    for summ in sorted(bench_dir.glob("*/summary.json")) if bench_dir.exists() else []:
        s = json.loads(summ.read_text(encoding="utf-8"))
        n = s.get("instances", 0)
        if not n:
            continue
        cost = (s.get("totals") or {}).get("cost_usd", 0.0)
        resolved = s.get("resolved", 0)
        rows.append(
            {
                "config_name": s.get("config_name", summ.parent.name),
                "instances": n,
                "resolved": resolved,
                "resolved_rate": s.get("resolved_rate", round(resolved / n, 4)),
                "cost_usd": round(cost, 4),
                "resolved_per_usd": round(resolved / cost, 4) if cost else 0.0,
            }
        )

    priced = [r for r in rows if r["resolved_per_usd"] > 0]
    if baseline:
        base = next((r for r in rows if r["config_name"] == baseline), None)
    else:
        base = min(priced, key=lambda r: r["resolved_per_usd"]) if priced else None
    base_eff = base["resolved_per_usd"] if base else 0.0
    for r in rows:
        # Unpriced configs (no recorded cost) have undefined value per dollar →
        # ``None`` ("—" in the table), not a misleading "0.00x". A priced config
        # that resolved nothing is a real 0.00x (zero value for the spend).
        r["value_multiple"] = (
            round(r["resolved_per_usd"] / base_eff, 2)
            if base_eff and r["cost_usd"] > 0
            else None
        )

    # Most cost-efficient first; unpriced configs (eff 0) sink to the bottom.
    rows.sort(key=lambda r: r["resolved_per_usd"], reverse=True)
    return {
        "benchmark": benchmark,
        "baseline": base["config_name"] if base else None,
        "rows": rows,
    }


def format_leaderboard(board: dict[str, Any]) -> str:
    """Render :func:`leaderboard` as a fixed-width text table for the CLI."""
    rows = board["rows"]
    if not rows:
        return f"no results to rank for benchmark {board['benchmark']!r}"
    name_w = max(len("config"), *(len(r["config_name"]) for r in rows))
    header = (
        f"{'config':<{name_w}}  {'resolved':>10}  {'score%':>7}  "
        f"{'cost_usd':>9}  {'resolved/$':>11}  {'value':>6}"
    )
    lines = [
        f"performance per dollar — {board['benchmark']} "
        f"(baseline: {board['baseline'] or 'n/a'})",
        header,
        "-" * len(header),
    ]
    for r in rows:
        resolved = f"{r['resolved']}/{r['instances']}"
        score = f"{r['resolved_rate'] * 100:.0f}%"
        mult = "—" if r["value_multiple"] is None else f"{r['value_multiple']:.2f}x"
        lines.append(
            f"{r['config_name']:<{name_w}}  {resolved:>10}  {score:>7}  "
            f"{r['cost_usd']:>9.4f}  {r['resolved_per_usd']:>11.4f}  {mult:>6}"
        )
    return "\n".join(lines)

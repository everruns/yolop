"""Select a stable, representative SWE-bench Verified tracking subset.

Goal: a small fixed set for tracking yolop quality over time. It must be
representative of the full benchmark and deterministic, so the resolve-rate is
comparable across yolop versions.

Method (no randomness):
1. Repo quotas by largest-remainder (Hamilton) apportionment of N seats
   proportional to each repo's share of the 500 instances — so repo coverage
   mirrors the real benchmark.
2. Fill each repo's quota to also track the benchmark's overall *difficulty*
   mix (also Hamilton): a greedy that, per pick, takes the difficulty currently
   furthest below its global target, breaking ties by difficulty order then
   instance_id.

Run: ``bench/.venv/bin/python bench/suites/select_tracking.py`` (rewrites the
JSON next to this file). Committed output is the source of truth; re-running on
the same dataset reproduces it exactly.
"""

from __future__ import annotations

import json
from pathlib import Path

import pandas as pd

N = 20
PARQUET = Path(__file__).resolve().parents[1] / ".cache" / "swebench_verified.parquet"
OUT = Path(__file__).resolve().parent / "tracking-v1.json"

# Easy -> hard; also the deterministic tie-break order.
DIFF_ORDER = ["<15 min fix", "15 min - 1 hour", "1-4 hours", ">4 hours"]


def hamilton(counts: dict[str, int], seats: int) -> dict[str, int]:
    """Largest-remainder apportionment of ``seats`` proportional to ``counts``."""
    total = sum(counts.values())
    exact = {k: seats * v / total for k, v in counts.items()}
    alloc = {k: int(v) for k, v in exact.items()}
    remaining = seats - sum(alloc.values())
    # Distribute leftovers by largest fractional remainder; ties by key.
    order = sorted(counts, key=lambda k: (-(exact[k] - alloc[k]), k))
    for k in order[:remaining]:
        alloc[k] += 1
    return alloc


def select(df: pd.DataFrame, n: int = N) -> list[dict]:
    repo_counts = df["repo"].value_counts().to_dict()
    repo_quota = hamilton(repo_counts, n)

    diff_counts = df["difficulty"].value_counts().to_dict()
    diff_target = hamilton(diff_counts, n)

    picks: list[dict] = []
    # Largest repos first so the scarce difficulty buckets are spent where there
    # is the most room to honor them.
    for repo in sorted(repo_quota, key=lambda r: (-repo_counts[r], r)):
        quota = repo_quota[repo]
        if quota == 0:
            continue
        pool: dict[str, list[str]] = {}
        sub = df[df["repo"] == repo]
        for d in DIFF_ORDER:
            ids = sorted(sub[sub["difficulty"] == d]["instance_id"].tolist())
            if ids:
                pool[d] = ids
        for _ in range(quota):
            # Difficulty currently furthest below its global target, among those
            # this repo can still supply.
            avail = [d for d in DIFF_ORDER if pool.get(d)]
            d = max(avail, key=lambda d: (diff_target[d], -DIFF_ORDER.index(d)))
            iid = pool[d].pop(0)
            diff_target[d] -= 1
            picks.append({"instance_id": iid, "repo": repo, "difficulty": d})
    return sorted(picks, key=lambda p: p["instance_id"])


def main() -> None:
    df = pd.read_parquet(PARQUET)
    picks = select(df)
    from collections import Counter
    suite = {
        "name": "tracking-v1",
        "benchmark": "swebench_verified",
        "description": (
            "Stable, representative 20-instance subset of SWE-bench Verified for "
            "tracking yolop quality over time. Deterministic stratified selection "
            "by repo (proportional) and difficulty (proportional). See "
            "select_tracking.py."
        ),
        "size": len(picks),
        "by_repo": dict(Counter(p["repo"] for p in picks)),
        "by_difficulty": dict(Counter(p["difficulty"] for p in picks)),
        "instances": picks,
    }
    OUT.write_text(json.dumps(suite, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(picks)} instances)")
    print("by repo:", suite["by_repo"])
    print("by difficulty:", suite["by_difficulty"])
    for p in picks:
        print(f"  {p['instance_id']:40} {p['difficulty']}")


if __name__ == "__main__":
    main()

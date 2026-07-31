# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Select (and verify) the `control-v1` suite — the periodic regression set.

Terminal-Bench is 89 tasks and hours of wall clock; the control suite is the
small, fixed subset run on a cadence to notice yolop moving. It is chosen by a
deterministic rule, not by hand, so it can be re-derived and audited:

1. **Eligible** tasks need no GPU and at most 4 GiB of memory, so the suite runs
   concurrently on an ordinary box rather than serializing on the 8 GiB tasks.
2. Within each difficulty tier, tasks are ordered by `(agent timeout, name)` —
   cheapest first. A control suite is run often, so its cost is the constraint.
3. Walking that order, a task is taken only if its **category is not already in
   the suite**, until the tier's quota is met. Eight tasks cannot be
   representative of 89; spreading them over distinct categories at least keeps
   the suite from being a referendum on one kind of work.
4. If diversity cannot fill a quota (there are only four `easy` tasks, three of
   them `software-engineering`), the remainder is filled from the same order
   with the category rule dropped.

Tiers are ordered easy -> medium -> hard, and the "already in the suite" set is
shared across tiers, so the harder tiers pick up categories the easy ones missed.

    uv run suites/select_control.py            # rewrite control-v1.json
    uv run suites/select_control.py --check    # verify the committed file

Re-running on the same dataset reproduces the committed set exactly. Bump to
`control-v2` rather than editing v1, so historical numbers stay comparable.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
STUDY_DIR = HERE.parent
SUITE_NAME = "control-v1"
SUITE_PATH = HERE / f"{SUITE_NAME}.json"

QUOTAS = {"easy": 3, "medium": 3, "hard": 2}
MAX_MEMORY_MB = 4096


@dataclass(frozen=True)
class Candidate:
    name: str
    difficulty: str
    category: str
    timeout_sec: float
    memory_mb: int
    gpus: int

    @property
    def eligible(self) -> bool:
        return self.gpus == 0 and self.memory_mb <= MAX_MEMORY_MB

    @property
    def order(self) -> tuple[float, str]:
        return (self.timeout_sec, self.name)


def load_candidates(dataset_dir: Path) -> list[Candidate]:
    candidates: list[Candidate] = []
    for path in sorted(dataset_dir.iterdir()):
        config_path = path / "task.toml"
        if not config_path.is_file():
            continue
        config = tomllib.loads(config_path.read_text(encoding="utf-8"))
        metadata = config.get("metadata") or {}
        environment = config.get("environment") or {}
        candidates.append(Candidate(
            name=path.name,
            difficulty=str(metadata.get("difficulty") or ""),
            category=str(metadata.get("category") or ""),
            timeout_sec=float((config.get("agent") or {}).get("timeout_sec") or 0.0),
            memory_mb=int(environment.get("memory_mb") or 0),
            gpus=int(environment.get("gpus") or 0),
        ))
    return candidates


def select(candidates: list[Candidate]) -> list[Candidate]:
    chosen: list[Candidate] = []
    categories: set[str] = set()
    for difficulty, quota in QUOTAS.items():
        tier = sorted(
            (c for c in candidates if c.difficulty == difficulty and c.eligible),
            key=lambda c: c.order,
        )
        picked = [c for c in _take_distinct_categories(tier, quota, categories)]
        if len(picked) < quota:  # only `easy` hits this: 4 tasks, 3 of one category
            already = {c.name for c in picked}
            picked += [c for c in tier if c.name not in already][: quota - len(picked)]
        categories.update(c.category for c in picked)
        chosen.extend(picked)
    return chosen


def _take_distinct_categories(tier: list[Candidate], quota: int,
                              used: set[str]) -> list[Candidate]:
    picked: list[Candidate] = []
    seen = set(used)
    for candidate in tier:
        if len(picked) == quota:
            break
        if candidate.category in seen:
            continue
        picked.append(candidate)
        seen.add(candidate.category)
    return picked


def build_suite(chosen: list[Candidate]) -> dict:
    return {
        "name": SUITE_NAME,
        "description": (
            "Periodic regression set: 3 easy / 3 medium / 2 hard Terminal-Bench 2.1 "
            "tasks, chosen deterministically by suites/select_control.py. Cheapest "
            "eligible task per tier, one per category, no GPU and <= 4 GiB."
        ),
        "quotas": QUOTAS,
        "tasks": [
            {
                "name": c.name,
                "difficulty": c.difficulty,
                "category": c.category,
                "agent_timeout_sec": c.timeout_sec,
            }
            for c in chosen
        ],
    }


def dataset_dir() -> Path:
    # Same resolution the study uses, so the selector sees the same tasks.
    import os

    override = os.environ.get("TB_DATASET_DIR")
    if override:
        return Path(override).expanduser().resolve()
    return STUDY_DIR / ".cache" / "dataset" / "terminal-bench-2-1"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="verify the committed suite instead of rewriting it")
    args = parser.parse_args()

    root = dataset_dir()
    if not root.is_dir():
        print(f"dataset not found at {root}; run bootstrap.sh first", file=sys.stderr)
        return 2

    suite = build_suite(select(load_candidates(root)))
    rendered = json.dumps(suite, indent=2) + "\n"

    if args.check:
        if not SUITE_PATH.exists():
            print(f"{SUITE_PATH} is missing", file=sys.stderr)
            return 1
        if SUITE_PATH.read_text(encoding="utf-8") != rendered:
            print(f"{SUITE_PATH} is stale; re-run without --check", file=sys.stderr)
            return 1
        print(f"{SUITE_PATH.name} reproduces exactly ({len(suite['tasks'])} tasks)")
        return 0

    SUITE_PATH.write_text(rendered, encoding="utf-8")
    print(f"wrote {SUITE_PATH} ({len(suite['tasks'])} tasks)")
    for task in suite["tasks"]:
        print(f"  {task['difficulty']:<7} {task['category']:<22} {task['name']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Named instance suites — curated subsets stored as ``bench/suites/<name>.json``.

A suite pins a fixed list of instances (e.g. the representative ``tracking-v1``
set) so the same cases can be re-run across yolop versions for quality tracking.
The JSON records selection metadata (by_repo/by_difficulty) plus an
``instances`` list of ``{instance_id, repo, difficulty}``.
"""

from __future__ import annotations

import json
from pathlib import Path

SUITES_DIR = Path(__file__).resolve().parents[1] / "suites"


def load_suite(name: str) -> dict:
    path = SUITES_DIR / f"{name}.json"
    if not path.exists():
        available = sorted(p.stem for p in SUITES_DIR.glob("*.json"))
        raise SystemExit(f"unknown suite {name!r}; available: {available}")
    return json.loads(path.read_text(encoding="utf-8"))


def suite_instance_ids(name: str) -> list[str]:
    return [i["instance_id"] for i in load_suite(name)["instances"]]

"""Named instance suites — curated subsets stored as ``bench/suites/<name>.json``.

A suite pins a fixed list of instances (e.g. the representative ``tracking-v1``
set) so the same cases can be re-run across yolop versions for quality tracking.
The JSON records selection metadata (by_repo/by_difficulty) plus an
``instances`` list of ``{instance_id, repo, difficulty}``.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

SUITES_DIR = Path(__file__).resolve().parents[1] / "suites"

# Suite names are used as filenames; keep them simple to prevent path traversal.
_SAFE_NAME = re.compile(r"^[A-Za-z0-9._-]+$")


def _check_name(name: str) -> None:
    if not name or name in (".", "..") or not _SAFE_NAME.match(name):
        raise SystemExit(
            f"invalid name {name!r}: use only letters, digits, '.', '_', '-'"
        )


def load_suite(name: str) -> dict:
    _check_name(name)
    path = SUITES_DIR / f"{name}.json"
    if not path.exists():
        available = sorted(p.stem for p in SUITES_DIR.glob("*.json"))
        raise SystemExit(f"unknown suite {name!r}; available: {available}")
    return json.loads(path.read_text(encoding="utf-8"))


def suite_instance_ids(name: str) -> list[str]:
    return [i["instance_id"] for i in load_suite(name)["instances"]]


def available_suites() -> list[str]:
    """Names of every suite stored under ``bench/suites/``."""
    return sorted(p.stem for p in SUITES_DIR.glob("*.json"))


def suite_membership() -> dict[str, list[str]]:
    """Map ``instance_id -> [suite names it belongs to]``.

    Used by the mira study to tag each sample with its suites, so a curated set
    can be selected with ``mira run --tag <suite>`` (e.g. ``--tag tracking-v1``).
    """
    membership: dict[str, list[str]] = {}
    for name in available_suites():
        for iid in suite_instance_ids(name):
            membership.setdefault(iid, []).append(name)
    return membership

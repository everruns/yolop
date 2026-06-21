"""Git workspace plumbing shared by the eval study.

A benchmark instance is solved inside a real checkout of its repository at
``base_commit``; the agent edits files in place and we capture the working-tree
diff as the model patch. These two helpers used to live in the bespoke runner;
the mira study now owns the per-cell run, so they live here.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

from .models import Instance


def _git(args: list[str], cwd: str | Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args], cwd=str(cwd) if cwd else None, capture_output=True, text=True
    )


def checkout(instance: Instance, dest: Path) -> None:
    """Materialize ``instance.repo`` at ``base_commit`` into ``dest``.

    Uses a SHA-targeted shallow fetch so we don't clone full history. Starts from
    a clean ``dest`` and checks every git step explicitly (no ``assert``, which
    ``python -O`` would strip) so failures surface here, not downstream.
    """
    if not instance.repo or not instance.base_commit:
        raise RuntimeError(f"{instance.instance_id}: missing repo/base_commit")
    # A reused dest could carry a stale remote/repo; start fresh.
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True, exist_ok=True)
    url = f"https://github.com/{instance.repo}.git"
    for step in (["init", "-q"], ["remote", "add", "origin", url]):
        r = _git(step, dest)
        if r.returncode != 0:
            raise RuntimeError(
                f"git {step[0]} failed for {instance.instance_id}: {r.stderr.strip()}"
            )
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

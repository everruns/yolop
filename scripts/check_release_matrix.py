#!/usr/bin/env python3
"""Assert the release workflow builds every extension server it promises.

`cli-binaries.yml` builds the CLI for a set of targets and the bundled
extension servers for what should be the same set. The two matrices are written
separately, and a GitHub Actions matrix fails quietly when it is wrong: an
`include` entry naming no existing dimension is merged into every combination
rather than creating one, so three of them collapse into a single job. v0.17.0
shipped Linux-only extension servers that way, with a green workflow, and the
manifest's binaries URL 404s on macOS.

This expands both matrices the way Actions does and compares the targets.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import yaml

WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/cli-binaries.yml"


def expand(matrix: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Expand a matrix into its job combinations, following Actions' rules.

    Dimensions form the cross product. An `include` entry that matches on the
    dimensions it names extends those combinations; one that names none extends
    every combination, which is the failure mode this script exists to catch.
    """
    dimensions = {k: v for k, v in matrix.items() if k != "include"}
    combinations: list[dict[str, Any]] = [{}]
    for key, values in dimensions.items():
        combinations = [
            {**combination, key: value} for combination in combinations for value in values
        ]

    includes: Sequence[Mapping[str, Any]] = matrix.get("include", [])
    if not dimensions:
        # With no dimensions there is nothing to extend, so each include entry
        # is a job of its own. That is how the CLI matrix is written.
        return [dict(entry) for entry in includes]

    for entry in includes:
        shared = {k: v for k, v in entry.items() if k in dimensions}
        extra = {k: v for k, v in entry.items() if k not in dimensions}
        matched = [c for c in combinations if all(c.get(k) == v for k, v in shared.items())]
        if matched:
            for combination in matched:
                combination.update(extra)
        else:
            combinations.append(dict(entry))
    return combinations


def targets(jobs: Mapping[str, Any], job: str) -> set[str]:
    combinations = expand(jobs[job]["strategy"]["matrix"])
    found = {c["target"] for c in combinations if "target" in c}
    if len(found) != len(combinations):
        raise SystemExit(f"{job}: {len(combinations)} job(s) for {len(found)} target(s)")
    return found


def main() -> None:
    jobs = yaml.safe_load(WORKFLOW.read_text())["jobs"]
    cli = targets(jobs, "build")
    extensions = targets(jobs, "extensions")
    if cli != extensions:
        missing = ", ".join(sorted(cli - extensions)) or "none"
        extra = ", ".join(sorted(extensions - cli)) or "none"
        raise SystemExit(
            "cli-binaries.yml builds the CLI and the extension servers for "
            f"different targets.\n  missing from the extensions matrix: {missing}"
            f"\n  built only for the extensions: {extra}"
        )
    print(f"cli-binaries.yml builds both for: {', '.join(sorted(cli))}")


if __name__ == "__main__":
    main()

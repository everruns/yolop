#!/usr/bin/env python3
"""Print publishable workspace crates in dependency-first order for yolop.

The order is derived from Cargo metadata rather than duplicated in the release
workflow, so adding a new in-workspace dependency cannot silently omit it from
publishing.
"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Mapping, Sequence
from typing import Any


def load_metadata() -> Mapping[str, Any]:
    output = subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], text=True
    )
    return json.loads(output)


def publish_order(metadata: Mapping[str, Any], root_name: str = "yolop") -> list[str]:
    packages = {
        package["name"]: package
        for package in metadata["packages"]
        if package["id"] in metadata["workspace_members"]
    }
    if root_name not in packages:
        raise ValueError(f"workspace package {root_name!r} not found")

    ordered: list[str] = []
    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(name: str) -> None:
        if name in visited:
            return
        if name in visiting:
            raise ValueError(f"workspace dependency cycle involving {name}")
        visiting.add(name)
        package = packages[name]
        dependencies: Sequence[Mapping[str, Any]] = package.get("dependencies", [])
        for dependency in dependencies:
            dependency_name = dependency["name"]
            if dependency.get("path") and dependency_name in packages:
                visit(dependency_name)
        visiting.remove(name)
        visited.add(name)
        if package.get("publish") != []:
            ordered.append(name)

    # Visit every publishable workspace package, including independent
    # first-party extensions that are not dependencies of the yolop binary.
    # Keep the root package last so a release publishes supporting packages
    # before the primary binary.
    for name in sorted(packages):
        if name != root_name:
            visit(name)
    visit(root_name)
    return ordered


def main() -> None:
    for crate in publish_order(load_metadata()):
        print(crate)


if __name__ == "__main__":
    main()

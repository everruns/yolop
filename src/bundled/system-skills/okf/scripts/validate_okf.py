#!/usr/bin/env python3
"""Validate an OKF v0.1 bundle. Zero dependencies (stdlib only).

Hard conformance rules (a failure exits non-zero):
  1. Every non-reserved `.md` file has a parseable YAML frontmatter block.
  2. Every such frontmatter has a non-empty `type` field.
  3. Reserved files (`index.md`, `log.md`) are exempt from the `type` rule.

Optional modes:
  --strict       also require `title`, `description`, and `timestamp`.
  --check-links  report broken `/`-absolute intra-bundle links (informational;
                 never fails the run — broken links are soft guidance in OKF).

Usage: python3 validate_okf.py <bundle-dir> [--strict] [--check-links]
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

RESERVED = {"index.md", "log.md"}


def parse_frontmatter(text: str):
    """Return (frontmatter_dict, error). Only top-level `key: value` pairs are
    read — enough for conformance without a YAML dependency. Nested structures
    are ignored, not rejected."""
    if not text.startswith("---"):
        return None, "missing YAML frontmatter (file must start with `---`)"
    # Split off the block between the first two `---` fences.
    parts = text.split("\n")
    if parts[0].strip() != "---":
        return None, "frontmatter fence `---` must be on the first line"
    fields: dict[str, str] = {}
    closed = False
    for line in parts[1:]:
        if line.strip() == "---":
            closed = True
            break
        m = re.match(r"^([A-Za-z0-9_-]+)\s*:\s*(.*)$", line)
        if m:
            fields[m.group(1)] = m.group(2).strip()
    if not closed:
        return None, "frontmatter block is not closed with a trailing `---`"
    return fields, None


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = {a for a in sys.argv[1:] if a.startswith("--")}
    if len(args) != 1:
        print(__doc__)
        return 2
    root = Path(args[0])
    if not root.is_dir():
        print(f"error: {root} is not a directory")
        return 2

    strict = "--strict" in flags
    check_links = "--check-links" in flags

    md_files = sorted(root.rglob("*.md"))
    ids = {f.relative_to(root).with_suffix("").as_posix() for f in md_files}

    errors: list[str] = []
    warnings: list[str] = []
    concepts = 0

    for f in md_files:
        rel = f.relative_to(root).as_posix()
        reserved = f.name in RESERVED
        text = f.read_text(encoding="utf-8", errors="replace")
        fm, err = parse_frontmatter(text)
        if reserved:
            continue
        if err:
            errors.append(f"{rel}: {err}")
            continue
        concepts += 1
        if not fm.get("type"):
            errors.append(f"{rel}: frontmatter has no non-empty `type` field")
        if strict:
            for req in ("title", "description", "timestamp"):
                if not fm.get(req):
                    errors.append(f"{rel}: --strict requires `{req}`")
        if check_links:
            for target in re.findall(r"\]\((/[^)\s]+)\)", text):
                cid = target.lstrip("/").removesuffix(".md")
                if cid not in ids:
                    warnings.append(f"{rel}: broken link -> {target}")

    for w in warnings:
        print(f"warn: {w}")
    for e in errors:
        print(f"FAIL: {e}")

    print(
        f"\n{concepts} concept(s) checked in {root} — "
        f"{len(errors)} error(s), {len(warnings)} warning(s)"
    )
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())

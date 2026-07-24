#!/usr/bin/env python3
"""Validate an Open Knowledge Format bundle using only the standard library.

Usage: python3 scripts/validate_okf.py <bundle-dir> [--strict] [--check-links]

--strict requires title, description, and timestamp in addition to OKF's
required type field. --check-links validates local Markdown link targets.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

RESERVED = {"index.md", "log.md"}
FRONTMATTER_RE = re.compile(r"\A---\s*\n(.*?)\n---\s*\n", re.DOTALL)
LINK_RE = re.compile(r"\]\(([^)\s]+)(?:\s+['\"][^)]*['\"])?\)")
SCHEME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")


def parse_frontmatter(text: str) -> tuple[dict[str, str], str | None]:
    match = FRONTMATTER_RE.match(text)
    if not match:
        return {}, "missing or malformed YAML frontmatter"
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or line[:1].isspace():
            continue
        if ":" not in line:
            return {}, f"malformed frontmatter line: {line!r}"
        key, value = line.split(":", 1)
        fields[key.strip()] = value.strip().strip("'\"")
    return fields, None


def local_link_target(root: Path, source: Path, target: str) -> Path | None:
    target = target.split("#", 1)[0]
    if not target or target.startswith("#") or SCHEME_RE.match(target):
        return None
    if target.startswith("/"):
        candidate = root / target.lstrip("/")
        if candidate.suffix == "":
            candidate = candidate.with_suffix(".md")
        return candidate
    # Bare placeholders such as `(url)` are not filesystem links. Repository
    # links have a path marker, a Markdown suffix, or name a directory.
    if "/" not in target and not target.endswith(".md"):
        return None
    return source.parent / target


def validate(root: Path, *, strict: bool = False, check_links: bool = False) -> tuple[list[str], int]:
    messages: list[str] = []
    concepts = 0
    for path in sorted(root.rglob("*.md")):
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8", errors="replace")
        if path.name not in RESERVED:
            frontmatter, error = parse_frontmatter(text)
            if error:
                messages.append(f"{rel}: {error}")
                continue
            concepts += 1
            if not frontmatter.get("type"):
                messages.append(f"{rel}: frontmatter has no non-empty `type` field")
            if strict:
                for required in ("title", "description", "timestamp"):
                    if not frontmatter.get(required):
                        messages.append(f"{rel}: --strict requires `{required}`")
        if check_links:
            for target in LINK_RE.findall(text):
                local = local_link_target(root, path, target)
                if local is not None and not local.resolve().exists():
                    messages.append(f"{rel}: broken link -> {target}")
    return messages, concepts


def main() -> int:
    args = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    flags = {arg for arg in sys.argv[1:] if arg.startswith("--")}
    if len(args) != 1:
        print(__doc__)
        return 2
    root = Path(args[0])
    if not root.is_dir():
        print(f"error: {root} is not a directory")
        return 2

    messages, concepts = validate(
        root, strict="--strict" in flags, check_links="--check-links" in flags
    )
    for message in messages:
        print(f"FAIL: {message}")
    print(f"\n{concepts} concept(s) checked in {root} — {len(messages)} error(s)")
    return 1 if messages else 0


if __name__ == "__main__":
    raise SystemExit(main())

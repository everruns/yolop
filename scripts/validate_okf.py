#!/usr/bin/env python3
"""Validate an Open Knowledge Format (OKF v0.2) bundle using only the standard library.

Usage: python3 validate_okf.py <bundle-dir> [--strict] [--check-links]

Without flags this checks only OKF conformance: every non-reserved `.md` file
has parseable YAML frontmatter with a non-empty `type`. Everything else in the
format is soft guidance, so it lives behind the opt-in flags.

--strict adds producer-side lint: `title`, `description`, and `generated`; the
`runtime` an Attested Computation requires; the shapes of `status`, `stale_after`
and `okf_version`; and the v0.1 fields v0.2 superseded. --check-links resolves
intra-bundle Markdown link targets. Findings from either flag are reasons to
improve a bundle you own, never to reject one you are only reading.

Spec: https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

RESERVED = {"index.md", "log.md"}
FRONTMATTER_RE = re.compile(r"\A---\s*\n(.*?)\n---\s*\n", re.DOTALL)
LINK_RE = re.compile(r"\]\(([^)\s]+)(?:\s+['\"][^)]*['\"])?\)")
SCHEME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
STATUSES = {"draft", "stable", "deprecated"}
ATTESTED_COMPUTATION = "attested computation"
# v0.1 fields v0.2 replaced. A bundle carrying them still conforms; --strict
# reports them so a producer migrates instead of writing new legacy documents.
SUPERSEDED = {"timestamp": "generated: { by, at }"}


def parse_frontmatter(text: str) -> tuple[dict[str, str], str | None]:
    """Return (top-level fields, error). Nested structures are flattened to their
    raw text, which is enough for presence and shape checks without a YAML
    dependency."""
    match = FRONTMATTER_RE.match(text)
    if not match:
        return {}, "missing or malformed YAML frontmatter"
    fields: dict[str, str] = {}
    for line in match.group(1).splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or line[:1].isspace():
            continue
        if stripped.startswith("- "):  # continuation of a block sequence
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


def lint_concept(fields: dict[str, str]) -> list[str]:
    """Soft guidance from the spec, reported only under --strict."""
    findings: list[str] = []
    for recommended in ("title", "description", "generated"):
        if not fields.get(recommended):
            findings.append(f"--strict requires `{recommended}`")
    for legacy, replacement in SUPERSEDED.items():
        if legacy in fields:
            findings.append(f"`{legacy}` is superseded by `{replacement}`")
    if fields.get("type", "").lower() == ATTESTED_COMPUTATION and not fields.get("runtime"):
        findings.append("`type: Attested Computation` requires `runtime`")
    status = fields.get("status")
    if status and status not in STATUSES:
        findings.append(f"`status` must be one of {sorted(STATUSES)}, got {status!r}")
    stale_after = fields.get("stale_after")
    if stale_after and not DATE_RE.match(stale_after):
        findings.append(f"`stale_after` must be an absolute YYYY-MM-DD date, got {stale_after!r}")
    return findings


def lint_index(fields: dict[str, str], is_bundle_root: bool) -> list[str]:
    """Index files carry no frontmatter, except `okf_version` at the bundle root."""
    allowed = {"okf_version"} if is_bundle_root else set()
    extra = sorted(set(fields) - allowed)
    if extra:
        return [f"index.md must not carry frontmatter keys {extra}"]
    version = fields.get("okf_version")
    if version and not re.match(r"^\d+\.\d+$", version):
        return [f"`okf_version` must be `<major>.<minor>`, got {version!r}"]
    return []


def validate(root: Path, *, strict: bool = False, check_links: bool = False) -> tuple[list[str], int]:
    messages: list[str] = []
    concepts = 0
    for path in sorted(root.rglob("*.md")):
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8", errors="replace")
        if path.name not in RESERVED:
            fields, error = parse_frontmatter(text)
            if error:
                messages.append(f"{rel}: {error}")
                continue
            concepts += 1
            if not fields.get("type"):
                messages.append(f"{rel}: frontmatter has no non-empty `type` field")
            if strict:
                messages.extend(f"{rel}: {finding}" for finding in lint_concept(fields))
        elif strict and path.name == "index.md" and text.startswith("---"):
            fields, error = parse_frontmatter(text)
            if error:
                messages.append(f"{rel}: {error}")
            else:
                messages.extend(f"{rel}: {f}" for f in lint_index(fields, rel == "index.md"))
        if check_links:
            for target in LINK_RE.findall(text):
                local = local_link_target(root, path, target)
                if local is not None and not local.resolve().exists():
                    messages.append(f"{rel}: broken link -> {target}")
    return messages, concepts


def main() -> int:
    args = [arg for arg in sys.argv[1:] if not arg.startswith("--")]
    flags = {arg for arg in sys.argv[1:] if arg.startswith("--")}
    if len(args) != 1 or flags - {"--strict", "--check-links"}:
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

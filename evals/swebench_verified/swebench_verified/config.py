"""Load the config matrix.

The matrix declares the set of agent configurations to benchmark. ``defaults``
applies to every config; per-config keys override it. Example::

    defaults:
      binary: ../target/release/yolop
      timeout: 1800
    configs:
      openai-default:
        provider: openai
        model: gpt-5.5
      openai-high:
        provider: openai
        model: gpt-5.5
        reasoning_effort: high
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml

DEFAULT_MATRIX = Path(__file__).resolve().parents[1] / "configs" / "matrix.yaml"


def load_matrix(path: str | Path | None = None) -> tuple[dict[str, Any], dict[str, dict]]:
    """Return ``(defaults, configs)`` from the matrix YAML.

    A relative ``binary`` path (in ``defaults`` or a config) is resolved to an
    absolute path against the matrix file's directory, so the harness works
    regardless of the caller's cwd.
    """
    path = Path(path) if path else DEFAULT_MATRIX
    base = path.resolve().parent
    data = yaml.safe_load(path.read_text()) or {}
    defaults = data.get("defaults", {}) or {}
    configs = data.get("configs", {}) or {}
    if not configs:
        raise ValueError(f"no configs defined in {path}")

    _resolve_binary(defaults, base)
    for cfg in configs.values():
        _resolve_binary(cfg or {}, base)
    return defaults, configs


def _resolve_binary(spec: dict[str, Any], base: Path) -> None:
    binary = spec.get("binary")
    if binary and not Path(binary).is_absolute():
        spec["binary"] = str((base / binary).resolve())

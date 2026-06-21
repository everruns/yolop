"""Benchmark abstraction.

A benchmark knows how to (1) enumerate normalized :class:`Instance` objects and
(2) score predictions. SWE-bench Verified is the first implementation; future
benchmarks (SWE-bench Lite, Multi-SWE, custom suites) implement the same two
methods so the runner stays benchmark-agnostic.
"""

from __future__ import annotations

import abc
from typing import Iterable, Mapping

from ..models import EvalResult, Instance


class Benchmark(abc.ABC):
    #: Stable short name used in result paths (e.g. ``swebench_verified``).
    name: str

    @abc.abstractmethod
    def load(self, instance_ids: list[str] | None = None) -> list[Instance]:
        """Return instances, optionally filtered to ``instance_ids``."""

    @abc.abstractmethod
    def evaluate(
        self,
        predictions: Mapping[str, str],
        instances: Iterable[Instance],
        run_id: str,
    ) -> dict[str, EvalResult]:
        """Score ``instance_id -> patch`` predictions.

        Returns ``instance_id -> EvalResult``. Implementations own any sandbox
        (e.g. Docker) needed to run the hidden tests.
        """


def get_benchmark(name: str, **kwargs) -> Benchmark:
    """Factory: map a benchmark name to its implementation."""
    if name in ("swebench_verified", "swe-bench-verified", "swebench"):
        from .swebench import SWEBenchVerified

        return SWEBenchVerified(**kwargs)
    raise ValueError(f"unknown benchmark: {name!r}")

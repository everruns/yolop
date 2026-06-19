"""yoloeval — evaluation harness for benchmarking yolop on coding benchmarks.

The harness is organized around three pluggable abstractions so new benchmarks
and new agents can be added without touching the orchestration core:

* ``datasets`` — a :class:`~yoloeval.datasets.base.Benchmark` loads instances and
  scores predictions (e.g. SWE-bench Verified).
* ``agents`` — an :class:`~yoloeval.agents.base.Agent` turns an instance into a
  patch plus a metrics record (yolop today, other coding agents later).
* ``runner`` — drives the (instance x config) matrix and persists results.

See ``bench/README.md`` for usage.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"

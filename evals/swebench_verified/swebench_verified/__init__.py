"""swebench_verified — a Mira eval study for benchmarking yolop on coding benchmarks.

swebench_verified is the benchmark-specific half of a two-process split: the generic
``mira`` host CLI owns the model matrix, selection, checkpoints, and reporting,
while this package — driven over Mira's stdio protocol — owns the parts that are
SWE-bench-specific. It is organized around pluggable abstractions so new
benchmarks and agents can be added without touching the protocol layer:

* ``datasets`` — a :class:`~swebench_verified.datasets.base.Benchmark` loads instances and
  scores predictions (e.g. SWE-bench Verified, via the official Docker harness).
* ``agents`` — an :class:`~swebench_verified.agents.base.Agent` turns an instance into a
  patch plus a metrics record (yolop, claude-code, codex, pi).
* ``study`` — answers Mira ``initialize``/``list``/``run`` requests, mapping each
  matrix config to a model and each instance to a sample.

See ``bench/README.md`` for usage.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"

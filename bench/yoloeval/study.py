"""A Mira eval study for yolop's coding benchmarks.

This is the polyglot seam described in mira's docs: the ``mira`` host CLI speaks
newline-delimited JSON over stdio to a child process, so an eval study can be
written in any language. yolop's harness is that child process — it keeps the
parts that are genuinely benchmark-specific (loading SWE-bench Verified, checking
out repos, running agent CLIs, and the Docker ``FAIL_TO_PASS`` scoring) and hands
everything generic — the model matrix, selection, checkpoints, and
JSON/HTML/JUnit reporting — to the host.

Drive it with the host CLI (install with ``brew install everruns/tap/mira`` or
``cargo install mira-cli``)::

    mira --cmd "bench/.venv/bin/python -m yoloeval" list
    mira --cmd "bench/.venv/bin/python -m yoloeval" run
    mira --cmd "bench/.venv/bin/python -m yoloeval" run astropy__astropy-12907
    mira --cmd "bench/.venv/bin/python -m yoloeval" run --tag tracking-v1
    mira --cmd "bench/.venv/bin/python -m yoloeval" run --models openai-default
    mira --cmd "bench/.venv/bin/python -m yoloeval" run --format html --out report.html

Mapping to mira's model:

* one **eval** per benchmark (``swebench_verified``);
* each benchmark **instance** is a **sample** (tagged with its repo, difficulty,
  and any curated suite it belongs to);
* each entry in ``configs/matrix.yaml`` is a **model** (label = config name); a
  config whose provider key is missing is advertised ``available: false`` so the
  host skips it instead of failing.

stdout carries ONLY protocol JSON (one object per line); logs go to stderr.
See mira's ``docs/protocol.md`` for the normative reference.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import uuid
from pathlib import Path
from typing import Any

from .agents import build_agent
from .agents.base import PROMPT_TEMPLATE
from .config import load_matrix
from .datasets.base import Benchmark, get_benchmark
from .models import AgentRun, Instance, RunMetrics
from .suites import suite_membership
from .workspace import capture_patch, checkout

PROTOCOL_VERSION = "1.0"
STUDY_VERSION = "0.2.0"
CACHE_DIR = Path(__file__).resolve().parents[1] / ".cache"

# Provider -> the env var whose presence makes its configs runnable. ``llmsim``
# is the offline plumbing provider and is always available.
_PROVIDER_KEYS = {
    "openai": "OPENAI_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
    "openrouter": "OPENROUTER_API_KEY",
    "llmsim": None,
}
# Agents that don't take a ``provider:`` use a fixed key (their CLI's own).
_AGENT_KEYS = {"claude-code": "ANTHROPIC_API_KEY", "codex": "OPENAI_API_KEY"}


def log(msg: str) -> None:
    print(f"[yoloeval-study] {msg}", file=sys.stderr, flush=True)


def _config_available(spec: dict) -> bool:
    """Whether a matrix config can run given the current environment."""
    provider = spec.get("provider")
    if provider == "llmsim":
        return True
    if provider in _PROVIDER_KEYS:
        return bool(os.environ.get(_PROVIDER_KEYS[provider]))
    key = _AGENT_KEYS.get(spec.get("agent", "yolop"))
    return bool(os.environ.get(key)) if key else False


def _sanitize(s: str) -> str:
    return "".join(c if c.isalnum() or c in "-._" else "_" for c in s)


class Study:
    """Holds the loaded benchmark + matrix and answers protocol requests.

    The host spawns one study process and sends many ``run`` requests over its
    lifetime, so the (cached) dataset and matrix are loaded once and reused.
    """

    def __init__(
        self,
        benchmark: str = "swebench_verified",
        matrix_path: str | Path | None = None,
        max_workers: int = 4,
        namespace: str = "swebench",
        eval_timeout: int = 1800,
        do_eval: bool = True,
    ):
        self.benchmark_name = benchmark
        self.matrix_path = matrix_path
        self.max_workers = max_workers
        self.namespace = namespace
        self.eval_timeout = eval_timeout
        self.do_eval = do_eval
        self._benchmark: Benchmark | None = None
        self._instances: dict[str, Instance] | None = None
        self._defaults: dict[str, Any] = {}
        self._configs: dict[str, dict] = {}
        self._load_matrix()

    # -- lazy resources ----------------------------------------------------
    def _load_matrix(self) -> None:
        self._defaults, self._configs = load_matrix(self.matrix_path)

    @property
    def benchmark(self) -> Benchmark:
        if self._benchmark is None:
            self._benchmark = get_benchmark(
                self.benchmark_name,
                max_workers=self.max_workers,
                namespace=self.namespace,
                timeout=self.eval_timeout,
            )
        return self._benchmark

    def instances(self) -> dict[str, Instance]:
        if self._instances is None:
            self._instances = {i.instance_id: i for i in self.benchmark.load()}
        return self._instances

    # -- protocol handlers -------------------------------------------------
    def initialize(self) -> dict[str, Any]:
        return {
            "protocol_version": PROTOCOL_VERSION,
            "study": "yoloeval",
            "study_version": STUDY_VERSION,
            "evals": 1,
            "capabilities": ["usage"],
        }

    def list(self) -> dict[str, Any]:
        membership = suite_membership()
        samples = []
        for iid, inst in self.instances().items():
            tags = [inst.repo] if inst.repo else []
            difficulty = (inst.extra or {}).get("difficulty")
            if difficulty:
                tags.append(_sanitize(str(difficulty)))
            tags.extend(membership.get(iid, []))
            samples.append({
                "id": iid,
                "tags": tags,
                "metadata": {
                    "repo": inst.repo or "",
                    "difficulty": str(difficulty or ""),
                },
            })

        models = [
            {
                "label": name,
                "provider": spec.get("provider") or spec.get("agent", "yolop"),
                "available": _config_available(spec),
            }
            for name, spec in self._configs.items()
        ]

        eval_def = {
            "name": self.benchmark_name,
            "description": "Resolve SWE-bench instances; FAIL_TO_PASS via the official Docker harness",
            "samples": samples,
            "scorers": ["succeeded", "resolved"],
            "models": models,
            "max_turns": 0,
            "metadata": {"benchmark": self.benchmark_name},
        }
        return {"evals": [eval_def]}

    def run(self, params: dict[str, Any]) -> dict[str, Any]:
        eval_name = params.get("eval")
        sample_id = params.get("sample")
        model = params.get("model")

        if eval_name != self.benchmark_name:
            raise ValueError(f"unknown eval: {eval_name!r}")
        spec = self._configs.get(model)
        if spec is None:
            raise ValueError(f"unknown model/config: {model!r}")
        inst = self.instances().get(sample_id)
        if inst is None:
            raise ValueError(f"unknown sample/instance: {sample_id!r}")

        if not _config_available(spec):
            return self._result(eval_name, sample_id, model, skipped=True)

        run = self._run_agent(inst, model, spec)
        resolved, eval_report, eval_err = self._score(inst, run, model)
        if eval_err and not run.error:
            run.error = eval_err

        transcript = self._transcript(run, resolved, eval_report, model, spec)
        scores = [
            {
                "scorer": "succeeded",
                "value": 0.0 if run.error else 1.0,
                "pass": run.error is None,
                "reason": run.error or "agent run completed",
            },
            {
                "scorer": "resolved",
                "value": 1.0 if resolved else 0.0,
                "pass": resolved,
                "reason": "FAIL_TO_PASS passed" if resolved else "not resolved",
            },
        ]
        aggregate = sum(s["value"] for s in scores) / len(scores)
        # ``resolved`` is the benchmark gate: a cell passes iff the hidden test
        # suite passed. ``succeeded`` is reported for signal but does not gate.
        return self._result(
            eval_name, sample_id, model,
            passed=resolved, aggregate=aggregate, scores=scores, transcript=transcript,
        )

    # -- helpers -----------------------------------------------------------
    def _run_agent(self, inst: Instance, model: str, spec: dict) -> AgentRun:
        run_id = f"{_sanitize(model)}-{_sanitize(inst.instance_id)}-{uuid.uuid4().hex[:6]}"
        workdir = CACHE_DIR / "work" / run_id
        session_dir = CACHE_DIR / "sessions" / run_id
        session_dir.mkdir(parents=True, exist_ok=True)
        try:
            agent = build_agent(model, spec, self._defaults)
            checkout(inst, workdir)
            run = agent.run(inst, str(workdir), str(session_dir))
            run.patch = capture_patch(workdir)
        except Exception as e:  # report, don't crash the study loop
            log(f"agent run failed for {model}/{inst.instance_id}: {e}")
            run = AgentRun(patch="", metrics=RunMetrics(), error=f"runner: {e}")
        finally:
            shutil.rmtree(workdir, ignore_errors=True)
        return run

    def _score(self, inst: Instance, run: AgentRun, model: str):
        if not self.do_eval:
            return False, {}, None
        try:
            run_id = f"{_sanitize(model)}-{_sanitize(inst.instance_id)}-{uuid.uuid4().hex[:6]}"
            results = self.benchmark.evaluate(
                {inst.instance_id: run.patch}, [inst], run_id
            )
            ev = results.get(inst.instance_id)
            if ev is None:
                return False, {}, "no eval result"
            return bool(ev.resolved), ev.report, ev.error
        except Exception as e:  # Docker/infra failure: not resolved, surface it
            log(f"eval failed for {model}/{inst.instance_id}: {e}")
            return False, {}, f"eval: {e}"

    def _transcript(self, run: AgentRun, resolved: bool, eval_report: dict,
                    model: str, spec: dict) -> dict[str, Any]:
        m = run.metrics
        tools = [name for name, count in m.tools_used.items() for _ in range(count)]
        files = {}
        if run.patch:
            files["model_patch.diff"] = run.patch
        return {
            "final_response": f"resolved={resolved}",
            "iterations": m.iterations,
            "tool_calls_count": m.tool_calls,
            "tool_calls": tools,
            "usage": {
                "input_tokens": m.tokens.input_tokens,
                "output_tokens": m.tokens.output_tokens,
                "cache_read_tokens": m.tokens.cache_read_tokens,
                "cache_creation_tokens": m.tokens.cache_creation_tokens,
                "total_tokens": m.tokens.total_tokens,
                "cost_usd": m.cost_usd,
            },
            "timing": {"duration_ms": int(m.wall_time_s * 1000)},
            "files": files,
            "metadata": {
                "config": model,
                "agent": str(spec.get("agent", "yolop")),
                "provider": str(spec.get("provider") or ""),
                "model": str(spec.get("model") or ""),
                "reasoning_effort": str(m.reasoning_effort or ""),
                "stop_reason": m.stop_reason,
                "resolved": str(resolved),
                "fail_to_pass": str((eval_report or {}).get("resolved", "")),
            },
            "error": run.error,
        }

    def _result(self, eval_name, sample_id, model, *, passed=False, aggregate=0.0,
                scores=None, transcript=None, skipped=False) -> dict[str, Any]:
        return {
            "eval": eval_name,
            "sample": sample_id,
            "model": model,
            "passed": passed,
            "aggregate": aggregate,
            "scores": scores or [],
            "transcript": transcript or {"final_response": "", "error": None},
            "skipped": skipped,
        }


def _build_study() -> Study:
    """Construct the study from env-var knobs (mira can't pass study CLI flags).

    The host owns selection/matrix/reporting; study-internal knobs come from the
    environment so they can be set on the ``mira --cmd`` line.
    """

    def _int(name: str, default: int) -> int:
        try:
            return int(os.environ[name])
        except (KeyError, ValueError):
            return default

    return Study(
        benchmark=os.environ.get("YOLOEVAL_BENCHMARK", "swebench_verified"),
        matrix_path=os.environ.get("YOLOEVAL_MATRIX") or None,
        max_workers=_int("YOLOEVAL_MAX_WORKERS", 4),
        namespace=os.environ.get("YOLOEVAL_NAMESPACE", "swebench"),
        eval_timeout=_int("YOLOEVAL_EVAL_TIMEOUT", 1800),
        # Set YOLOEVAL_NO_EVAL=1 for a plumbing run that skips Docker scoring.
        do_eval=os.environ.get("YOLOEVAL_NO_EVAL", "") not in ("1", "true", "yes"),
    )


def serve(study: Study | None = None) -> None:
    """Run the stdio protocol loop until stdin closes."""
    study = study or _build_study()
    log(f"ready: benchmark={study.benchmark_name} do_eval={study.do_eval}")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            print(json.dumps({"method": "log", "params": {"message": "bad json"}}), flush=True)
            continue
        rid, method, params = msg.get("id"), msg.get("method"), msg.get("params", {})
        try:
            if method == "initialize":
                result = study.initialize()
            elif method == "list":
                result = study.list()
            elif method == "run":
                result = study.run(params)
            else:
                raise ValueError(f"unknown method: {method!r}")
            print(json.dumps({"id": rid, "result": result}), flush=True)
        except Exception as exc:  # noqa: BLE001 — report, never crash the loop
            log(f"error handling {method}: {exc}")
            print(json.dumps({"id": rid, "error": {"message": str(exc)}}), flush=True)
    log("stdin closed, exiting")


def main(argv: list[str] | None = None) -> int:
    serve()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

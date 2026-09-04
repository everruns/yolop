# /// script
# requires-python = ">=3.12"
# dependencies = ["mira-eval>=0.3.0", "harbor>=0.20.0"]
# ///
"""terminal_bench — a single-file Mira eval study for Terminal-Bench 2.1.

[Terminal-Bench](https://www.tbench.ai) measures an agent's ability to finish
real work in a terminal: 89 containerized tasks (compile a toolchain, recover a
corrupted database, break a cipher) each scored by the task's own test suite.
2.1 is a revision of 2.0 that fixes 28 tasks — same 89-task set, so 2.0 numbers
are *not* comparable.

Two harnesses stack here, each owning what it is good at:

* **Mira** (`mira` host CLI) owns the target matrix, sample/target selection,
  saved run folders, and reporting — the same role it plays for
  `evals/swebench_verified`.
* **Harbor** (`harbor` CLI, Terminal-Bench's own execution framework) owns one
  case: building the task container, running the agent in it, and running the
  task's verifier. This study shells out to `harbor run` per case and reads the
  reward back out of the trial's `result.json`.

yolop plugs into Harbor through `harbor_yolop.py` (an adapter alongside this
file), which uploads a host-built yolop binary into the task container. Harbor's
own agents (`terminus-2`, `claude-code`, `codex`, …) are available as targets
too, so yolop can be compared against them on identical tasks and scorer.

    cd evals/terminal_bench
    ./bootstrap.sh                                    # yolop musl build + mira + dataset
    mira list                                         # eval, samples, targets
    mira run --samples fix-git --targets llmsim       # offline plumbing check
    TB_MAX_COST_USD=10 doppler run -- mira run --preset control   # the periodic run

Study-internal knobs come from the environment (the Mira host cannot pass study
CLI flags): `TB_YOLOP_BIN` (yolop binary to upload; default the musl build),
`TB_MAX_COST_USD` (whole-run USD budget across every case), `TB_DATASET_DIR`
(pre-downloaded task tree), `TB_AGENT_ENV` / `TB_VERIFIER_ENV` (comma-separated
`KEY=VALUE` passed into the agent's resp. the verifier's environment),
`TB_CA_CERT` (CA bundle to install in the container, for sandboxes behind a
TLS-terminating proxy), `TB_OVERRIDE_MEMORY_MB` (shared task-container memory), `TB_JOB_TIMEOUT`,
`TB_TIMEOUT_MULTIPLIER`, `TB_KEEP_JOBS=0` (discard retained Harbor job dirs).

stdout carries ONLY protocol JSON (one object per line); logs go to stderr.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import time
import tomllib
import uuid
from collections import abc
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import mira  # the mira-eval SDK: protocol types + the stdio serve loop

sys.path.insert(0, str(Path(__file__).resolve().parent))
import harbor_yolop  # noqa: E402  the Harbor agent adapter (sibling module)

STUDY_VERSION = "0.1.0"

# The Harbor Hub dataset id. `harbor dataset download` resolves it against the
# hub registry; the tasks land under <output-dir>/<DATASET_NAME>/<task>/.
DATASET = "terminal-bench/terminal-bench-2-1"
DATASET_NAME = "terminal-bench-2-1"
N_TASKS = 89

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]  # evals/terminal_bench -> repo root
CACHE_DIR = HERE / ".cache"
# Container mode is the only mode: Harbor always runs the agent inside the task
# image, so the uploaded binary must match that ABI. A static musl build runs in
# every task image; the host glibc build generally does not.
YOLOP_BIN = os.environ.get("TB_YOLOP_BIN") or str(
    REPO_ROOT / "target/x86_64-unknown-linux-musl/release/yolop"
)


def log(msg: str) -> None:
    print(f"[terminal_bench] {msg}", file=sys.stderr, flush=True)


def _int(name: str, default: int) -> int:
    try:
        return int(os.environ[name])
    except (KeyError, ValueError):
        return default


def _float(name: str, default: float | None) -> float | None:
    try:
        return float(os.environ[name])
    except (KeyError, ValueError):
        return default


def _bool(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None:
        return default
    normalized = raw.strip().lower()
    if normalized in ("1", "true", "yes"):
        return True
    if normalized in ("0", "false", "no"):
        return False
    return default


# ============================================================================ #
# The config matrix (one entry = one Mira target). `agent:` is what Harbor is
# told to run: `yolop` maps to the adapter import path, anything else is passed
# to `harbor run --agent` verbatim (so Harbor's built-in agents are targets too).
# ============================================================================ #
YOLOP_AGENT_IMPORT_PATH = "harbor_yolop:Yolop"

DEFAULTS: dict[str, Any] = {"max_cost_usd": 5.0}

MATRIX: dict[str, dict[str, Any]] = {
    # yolop x OpenAI (default provider)
    "openai-gpt-5.6-luna": {"agent": "yolop", "model": "openai/gpt-5.6-luna"},
    "openai-gpt-5.6-terra": {"agent": "yolop", "model": "openai/gpt-5.6-terra"},
    "openai-gpt-5.6-terra-medium": {
        "agent": "yolop", "model": "openai/gpt-5.6-terra", "reasoning_effort": "medium",
    },
    "openai-gpt-5.6-terra-high": {
        "agent": "yolop", "model": "openai/gpt-5.6-terra", "reasoning_effort": "high",
    },
    "openai-gpt-5.5": {"agent": "yolop", "model": "openai/gpt-5.5"},
    # The same model through OpenRouter. Worth its own target beyond redundancy:
    # OpenRouter returns an inline price, so `cost_usd` (and the cap that reads
    # it) is the real charge rather than yolop's price-table estimate.
    "openrouter-gpt-5.6-terra": {"agent": "yolop", "model": "openrouter/openai/gpt-5.6-terra"},
    "openrouter-claude-sonnet-5": {
        "agent": "yolop", "model": "openrouter/anthropic/claude-sonnet-5",
    },
    # yolop x Anthropic (secondary provider)
    "anthropic-claude-sonnet-5": {"agent": "yolop", "model": "anthropic/claude-sonnet-5"},
    "anthropic-claude-opus-4.8": {"agent": "yolop", "model": "anthropic/claude-opus-4-8"},
    "anthropic-claude-sonnet-4.5": {"agent": "yolop", "model": "anthropic/claude-sonnet-4-5"},
    # Offline plumbing check: exercises upload/run/verify with no API key. It
    # will not solve anything, so a 0.0 reward here is the expected result.
    "llmsim": {"agent": "yolop", "model": "llmsim/llmsim-yolop"},
    # Harbor's own agents, for cross-agent comparison on identical tasks.
    "terminus-2": {"agent": "terminus-2", "model": "openai/gpt-5.6-terra"},
    "claude-code-opus-4.8": {"agent": "claude-code", "model": "anthropic/claude-opus-4-8"},
    "claude-code-sonnet-5": {
        "agent": "claude-code",
        "harbor_agent": "harbor_claude:ClaudeCodeCompatibility",
        "model": "anthropic/claude-sonnet-5",
    },
    "codex": {
        "agent": "codex", "harbor_agent": "harbor_codex:CodexCompatibility",
        "model": "openai/gpt-5.6-terra",
    },
}

# provider (the part before `/` in a model id) -> env var that makes it runnable.
_PROVIDER_KEYS = {
    "anthropic": "ANTHROPIC_API_KEY",
    "google": "GEMINI_API_KEY",
    "openai": "OPENAI_API_KEY",
    "openrouter": "OPENROUTER_API_KEY",
    "llmsim": None,
}


def provider_of(spec: dict) -> str:
    model = spec.get("model") or ""
    return model.split("/", 1)[0] if "/" in model else ""


def config_available(spec: dict) -> bool:
    """Whether a matrix config can run given the current environment."""
    provider = provider_of(spec)
    if provider == "llmsim":
        return True
    key = _PROVIDER_KEYS.get(provider)
    return bool(os.environ.get(key)) if key else False


def _target_metadata(spec: dict) -> dict[str, Any]:
    cfg = {**DEFAULTS, **spec}
    md: dict[str, Any] = {"agent": cfg.get("agent", "yolop"), "provider": provider_of(cfg)}
    for key in ("model", "reasoning_effort", "max_cost_usd"):
        if cfg.get(key) is not None:
            md[key] = cfg[key]
    return md


# ============================================================================ #
# Dataset — the 89 task directories, downloaded once with the harbor CLI and
# cached. Each task's `task.toml` carries the metadata Mira tags samples with.
# ============================================================================ #
@dataclass
class Task:
    name: str
    path: Path
    difficulty: str = ""
    category: str = ""
    tags: list[str] = field(default_factory=list)
    agent_timeout_sec: float = 0.0


def harbor_bin() -> str:
    """The `harbor` console script. `uv run` installs it next to the interpreter
    from this file's PEP 723 deps, so prefer that over whatever is on PATH."""
    local = Path(sys.executable).parent / "harbor"
    if local.exists():
        return str(local)
    found = shutil.which("harbor")
    if not found:
        raise RuntimeError("harbor CLI not found; install it with `pip install harbor`")
    return found


def dataset_dir() -> Path:
    """The task tree, downloading it on first use. `TB_DATASET_DIR` points at an
    existing download (a shared cache, or a pinned copy)."""
    override = os.environ.get("TB_DATASET_DIR")
    if override:
        return Path(override).expanduser().resolve()
    root = CACHE_DIR / "dataset"
    tasks_dir = root / DATASET_NAME
    # A marker, not "the directory exists": `harbor dataset download` writes
    # tasks as it fetches them, so an interrupted download leaves a partial tree
    # that would otherwise be reused as a silently shrunken benchmark.
    marker = root / f".{DATASET_NAME}.complete"
    if marker.is_file() and tasks_dir.is_dir():
        return tasks_dir
    root.mkdir(parents=True, exist_ok=True)
    log(f"downloading {DATASET} -> {tasks_dir}")
    proc = subprocess.run(
        [harbor_bin(), "dataset", "download", DATASET, "-o", str(root), "--overwrite"],
        stdout=sys.stderr, stderr=subprocess.STDOUT,
    )
    if proc.returncode != 0 or not tasks_dir.is_dir():
        raise RuntimeError(f"`harbor dataset download {DATASET}` failed (exit {proc.returncode})")
    marker.write_text(DATASET, encoding="utf-8")
    return tasks_dir


def load_task(path: Path) -> Task:
    config = tomllib.loads((path / "task.toml").read_text(encoding="utf-8"))
    metadata = config.get("metadata") or {}
    return Task(
        name=path.name,
        path=path,
        difficulty=str(metadata.get("difficulty") or ""),
        category=str(metadata.get("category") or ""),
        tags=[str(t) for t in (metadata.get("tags") or [])],
        agent_timeout_sec=float((config.get("agent") or {}).get("timeout_sec") or 0.0),
    )


# ---- Suites — curated task subsets under suites/*.json, surfaced as sample tags
# so a set can be selected with `mira run --tag <suite>`. ---------------------- #
SUITES_DIR = HERE / "suites"
_SAFE_NAME = re.compile(r"^[A-Za-z0-9._-]+$")


def load_suite(name: str) -> dict:
    if not name or name in (".", "..") or not _SAFE_NAME.match(name):
        raise SystemExit(f"invalid suite name {name!r}")
    path = SUITES_DIR / f"{name}.json"
    if not path.exists():
        raise SystemExit(f"unknown suite {name!r}; available: {available_suites()}")
    return json.loads(path.read_text(encoding="utf-8"))


def available_suites() -> list[str]:
    return sorted(p.stem for p in SUITES_DIR.glob("*.json")) if SUITES_DIR.exists() else []


def suite_membership() -> dict[str, list[str]]:
    """task name -> the suites it belongs to."""
    membership: dict[str, list[str]] = {}
    for name in available_suites():
        for task in load_suite(name).get("tasks", []):
            membership.setdefault(task["name"], []).append(name)
    return membership


def load_tasks() -> list[Task]:
    root = dataset_dir()
    tasks = [
        load_task(path)
        for path in sorted(root.iterdir())
        if (path / "task.toml").is_file()
    ]
    if len(tasks) != N_TASKS:
        # Not fatal — a filtered or pinned copy is a legitimate TB_DATASET_DIR —
        # but a silent partial download would quietly shrink the benchmark.
        log(f"warning: expected {N_TASKS} tasks, found {len(tasks)} in {root}")
    return tasks


# ============================================================================ #
# Running one case — `harbor run` for a single (task, config), then read the
# reward and usage back out of the trial's result.json.
# ============================================================================ #
@dataclass
class TrialOutcome:
    reward: float = 0.0
    resolved: bool = False
    wall_time_s: float = 0.0
    input_tokens: int = 0
    cache_tokens: int = 0
    output_tokens: int = 0
    cost_usd: float = 0.0
    stop_reason: str = "completed"
    agent_metadata: dict[str, Any] = field(default_factory=dict)
    exception: str | None = None
    error: str | None = None


def _sanitize(value: str) -> str:
    return "".join(c if c.isalnum() or c in "-._" else "_" for c in value)


def _env_pairs(name: str) -> list[str]:
    """Parse a `KEY=VALUE,KEY=VALUE` env knob into Harbor's repeated-flag form.

    The sandbox a study runs in, not the benchmark, decides these (a proxy host,
    a base URL), so they stay configuration rather than matrix entries.
    """
    raw = os.environ.get(name, "")
    return [pair.strip() for pair in raw.split(",") if "=" in pair]


def _verifier_env_pairs() -> list[str]:
    """`TB_VERIFIER_ENV`, plus the CA the agent adapter already installed.

    The verifier phase is a separate process from the agent and gets none of the
    agent's environment — but many tasks' `test.sh` fetches something over the
    network, so behind a TLS-terminating proxy it needs the same trust setup or
    every task fails to *score* rather than failing on merit.
    """
    pairs = _env_pairs("TB_VERIFIER_ENV")
    if os.environ.get("TB_CA_CERT"):
        declared = {pair.split("=", 1)[0] for pair in pairs}
        pairs += [
            f"{var}={harbor_yolop.CONTAINER_CA_PATH}"
            for var in ("SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE",
                        "NODE_EXTRA_CA_CERTS")
            if var not in declared
        ]
    return pairs


def build_command(task: Task, spec: dict, job_dir: Path, *, jobs_dir: Path) -> list[str]:
    cfg = {**DEFAULTS, **spec}
    agent = cfg.get("agent", "yolop")
    harbor_agent = cfg.get("harbor_agent") or (YOLOP_AGENT_IMPORT_PATH if agent == "yolop" else agent)
    cmd = [
        harbor_bin(), "run",
        "-p", str(task.path),
        "--jobs-dir", str(jobs_dir),
        "--job-name", job_dir.name,
        "--agent", harbor_agent,
        # Mira owns concurrency across cases; one Harbor job is one case, so its
        # own trial concurrency stays at 1.
        "-n", "1",
        "--quiet",
        "--yes",
    ]
    if cfg.get("model"):
        cmd += ["--model", cfg["model"]]
    if cfg.get("agent_setup_timeout_multiplier") is not None:
        cmd += ["--agent-setup-timeout-multiplier", str(cfg["agent_setup_timeout_multiplier"])]
    if (memory_mb := _int("TB_OVERRIDE_MEMORY_MB", None)) is not None:
        cmd += ["--override-memory-mb", str(memory_mb)]
    if agent == "yolop":
        cmd += ["--ak", f"binary_path={YOLOP_BIN}"]
        if cfg.get("max_cost_usd") is not None:
            cmd += ["--ak", f"max_cost_usd={cfg['max_cost_usd']}"]
        if cfg.get("reasoning_effort"):
            cmd += ["--ak", f"reasoning_effort={cfg['reasoning_effort']}"]
        if os.environ.get("TB_CA_CERT"):
            cmd += ["--ak", f"ca_cert_path={os.environ['TB_CA_CERT']}"]
    for pair in _env_pairs("TB_AGENT_ENV"):
        cmd += ["--ae", pair]
    for pair in _verifier_env_pairs():
        cmd += ["--ve", pair]
    multiplier = _float("TB_TIMEOUT_MULTIPLIER", None)
    if multiplier is not None:
        cmd += ["--timeout-multiplier", str(multiplier)]
    return cmd + list(cfg.get("extra_args") or [])


def find_trial_result(job_dir: Path) -> Path | None:
    """The single trial's result.json. One task x one target is one trial, but
    its directory name carries a random suffix, so it is globbed, not named."""
    candidates = sorted(job_dir.glob("*/result.json"))
    return candidates[0] if candidates else None


def parse_trial(path: Path) -> TrialOutcome:
    result = json.loads(path.read_text(encoding="utf-8"))
    outcome = TrialOutcome()

    rewards = ((result.get("verifier_result") or {}).get("rewards")) or {}
    # Terminal-Bench scores each task 1.0 (all tests passed) or 0.0. Harbor's
    # reward map is open-ended, so read the canonical key and keep the rest in
    # metadata rather than assuming this is the only one.
    outcome.reward = float(rewards.get("reward") or 0.0)
    outcome.resolved = outcome.reward >= 1.0

    agent_result = result.get("agent_result") or {}
    outcome.input_tokens = int(agent_result.get("n_input_tokens") or 0)
    outcome.cache_tokens = int(agent_result.get("n_cache_tokens") or 0)
    outcome.output_tokens = int(agent_result.get("n_output_tokens") or 0)
    outcome.cost_usd = float(agent_result.get("cost_usd") or 0.0)
    outcome.agent_metadata = agent_result.get("metadata") or {}
    outcome.stop_reason = str(outcome.agent_metadata.get("stop_reason") or "completed")

    exception = result.get("exception_info")
    if exception:
        outcome.exception = exception.get("exception_type")
        outcome.error = (
            f"{exception.get('exception_type')}: {exception.get('exception_message')}"
        )
    return outcome


def run_case(task: Task, target: str, spec: dict, *, job_timeout: int,
             keep_jobs: bool) -> TrialOutcome:
    """One `harbor run`: build the container, run the agent, run the verifier."""
    jobs_dir = CACHE_DIR / "jobs"
    job_dir = jobs_dir / f"{_sanitize(target)}-{_sanitize(task.name)}-{uuid.uuid4().hex[:6]}"
    cmd = build_command(task, spec, job_dir, jobs_dir=jobs_dir)

    env = dict(os.environ)
    # The adapter is imported by path from this directory by the harbor subprocess.
    env["PYTHONPATH"] = os.pathsep.join(filter(None, [str(HERE), env.get("PYTHONPATH")]))

    log(f"{target}/{task.name}: {' '.join(cmd)}")
    start = time.monotonic()
    # Harbor's progress output is verbose and must not reach this process's
    # stdout — that is the Mira protocol channel.
    proc = subprocess.run(cmd, env=env, stdout=sys.stderr, stderr=subprocess.STDOUT,
                          timeout=job_timeout)
    elapsed = round(time.monotonic() - start, 3)

    result_path = find_trial_result(job_dir)
    if result_path is None:
        outcome = TrialOutcome(
            wall_time_s=elapsed, stop_reason="error",
            error=f"harbor produced no trial result (exit {proc.returncode}) in {job_dir}",
        )
    else:
        outcome = parse_trial(result_path)
        outcome.wall_time_s = elapsed
    if not keep_jobs:
        shutil.rmtree(job_dir, ignore_errors=True)
    return outcome


# ============================================================================ #
# The study — the Terminal-Bench-specific work behind the mira-eval SDK. The SDK
# owns the stdio protocol loop; this class holds the task cache and the run-wide
# budget, and exposes a subject `(sample, cx) -> mira.Transcript`.
# ============================================================================ #
def _resolved_scorer() -> mira.Scorer:
    """The benchmark gate: did the task's own test suite pass?"""

    def fn(_sample, transcript) -> mira.Score:
        resolved = bool(transcript.metadata.get("resolved"))
        return mira.make_score(
            "resolved", 1.0 if resolved else 0.0, resolved,
            "tests passed" if resolved else "tests failed",
        )

    return mira.scorer("resolved", fn)


def build_transcript(task: Task, spec: dict, outcome: TrialOutcome,
                     error_kind: str | None) -> mira.Transcript:
    tools_used = outcome.agent_metadata.get("tools_used") or {}
    tools = [name for name, count in tools_used.items() for _ in range(count)]
    metrics: dict[str, float] = {
        "reward": outcome.reward,
        "tool_calls": float(outcome.agent_metadata.get("tool_calls") or 0),
        "llm_calls": float(outcome.agent_metadata.get("llm_calls") or 0),
        "cache_read_tokens": float(outcome.cache_tokens),
    }
    return mira.Transcript(
        final_response=f"resolved={outcome.resolved}",
        iterations=int(outcome.agent_metadata.get("agent_steps") or 0),
        tool_calls_count=int(outcome.agent_metadata.get("tool_calls") or 0),
        tool_calls=tools,
        usage=mira.Usage(
            # Harbor's n_input_tokens includes cache reads; Mira's does not.
            input_tokens=max(outcome.input_tokens - outcome.cache_tokens, 0),
            output_tokens=outcome.output_tokens,
            cache_read_tokens=outcome.cache_tokens,
            cost_usd=outcome.cost_usd,
        ),
        timing=mira.Timing(duration_ms=int(outcome.wall_time_s * 1000)),
        metrics=metrics,
        metadata={
            "agent": spec.get("agent", "yolop"),
            "provider": provider_of(spec),
            "model": spec.get("model") or "",
            "reasoning_effort": outcome.agent_metadata.get("reasoning_effort"),
            "provider_requested": provider_of(spec),
            "model_requested": spec.get("model") or "",
            "reasoning_effort_requested": spec.get("reasoning_effort"),
            "provider_effective": outcome.agent_metadata.get("provider"),
            "model_effective": outcome.agent_metadata.get("model"),
            "reasoning_effort_effective": outcome.agent_metadata.get("reasoning_effort"),
            "stop_reason": outcome.stop_reason,
            "resolved": outcome.resolved,
            "reward": outcome.reward,
            "difficulty": task.difficulty,
            "category": task.category,
            "exception": outcome.exception,
            "tools_used": tools_used,
            "yolop_version": outcome.agent_metadata.get("yolop_version"),
        },
        error=outcome.error,
        # Harbor/Docker failures and budget stops are surfaced as N/A by the
        # SDK's scorer short-circuit rather than counted as the model failing.
        error_kind=error_kind,
    )


class Study:
    """Terminal-Bench study state: the task cache plus the run-wide USD budget."""

    def __init__(self, *, max_cost_usd: float | None = None, job_timeout: int = 5400,
                 keep_jobs: bool = True):
        self.max_cost_usd = max_cost_usd
        self.job_timeout = job_timeout
        self.keep_jobs = keep_jobs
        self._tasks: dict[str, Task] | None = None
        self._spent_usd = 0.0

    def tasks(self) -> dict[str, Task]:
        if self._tasks is None:
            self._tasks = {task.name: task for task in load_tasks()}
        return self._tasks

    @property
    def spent_usd(self) -> float:
        return round(self._spent_usd, 6)

    def _budget_left(self) -> float | None:
        if self.max_cost_usd is None:
            return None
        return self.max_cost_usd - self._spent_usd

    def _subject(self, sample: mira.Sample, cx: mira.RunCx) -> mira.Transcript:
        spec = MATRIX.get(cx.target)
        if spec is None:
            raise ValueError(f"unknown target/config: {cx.target!r}")
        task = self.tasks().get(sample.id)
        if task is None:
            raise ValueError(f"unknown sample/task: {sample.id!r}")

        # The run-wide cap is checked before a case starts: once the budget is
        # gone, no further case is worth starting, and reporting the remaining
        # cases as infra-N/A keeps them out of the resolve rate.
        left = self._budget_left()
        if left is not None and left <= 0:
            log(f"budget exhausted (${self.spent_usd:.2f}); skipping {cx.target}/{task.name}")
            return build_transcript(
                task, spec,
                TrialOutcome(stop_reason="budget",
                             error=f"run budget ${self.max_cost_usd:.2f} exhausted "
                                   f"(${self.spent_usd:.2f} spent)"),
                error_kind="infra",
            )

        # Per-case cap: never let one case spend more than the run has left.
        case_spec = dict(spec)
        cap = {**DEFAULTS, **spec}.get("max_cost_usd")
        if left is not None:
            case_spec["max_cost_usd"] = min(left, cap) if cap is not None else left

        try:
            outcome = run_case(task, cx.target, case_spec, job_timeout=self.job_timeout,
                               keep_jobs=self.keep_jobs)
        except subprocess.TimeoutExpired:
            outcome = TrialOutcome(stop_reason="timeout",
                                   error=f"harbor run exceeded {self.job_timeout}s")
        except Exception as exc:  # report, don't crash the study loop
            log(f"case failed for {cx.target}/{task.name}: {exc}")
            outcome = TrialOutcome(stop_reason="error", error=f"runner: {exc}")

        self._spent_usd += outcome.cost_usd
        if self.max_cost_usd is not None:
            log(f"spent ${self.spent_usd:.2f} of ${self.max_cost_usd:.2f}")

        error_kind = "infra" if outcome.stop_reason in ("error", "timeout") else None
        return build_transcript(task, case_spec, outcome, error_kind)

    def _build_samples(self) -> list[mira.Sample]:
        membership = suite_membership()
        samples: list[mira.Sample] = []
        for task in self.tasks().values():
            tags = [t for t in (task.difficulty, task.category) if t]
            tags += [_sanitize(t) for t in task.tags]
            tags += membership.get(task.name, [])
            samples.append(mira.Sample(
                task.name, tags=tags,
                metadata={"difficulty": task.difficulty, "category": task.category,
                          "agent_timeout_sec": task.agent_timeout_sec,
                          "suites": membership.get(task.name, [])},
            ))
        return samples

    def _targets(self) -> list[mira.Target]:
        return [
            mira.target(
                name, provider=provider_of(spec) or spec.get("agent", "yolop"),
                available=config_available(spec), metadata=_target_metadata(spec),
            )
            for name, spec in MATRIX.items()
        ]

    def protocol(self) -> mira.Study:
        study = mira.Study("terminal_bench", version=STUDY_VERSION)
        study.add(mira.Eval(
            name="terminal_bench",
            subject=self._subject,
            samples=_LazySamples(self),
            targets=self._targets(),
            scorers=[mira.succeeded(), _resolved_scorer()],
            description="Solve Terminal-Bench 2.1 tasks; scored by each task's own tests",
            metadata={"benchmark": DATASET_NAME},
        ))
        return study


class _LazySamples(abc.Sequence):
    """The study's samples, materialized (downloading the dataset) on first
    access. Keeps `initialize` cheap and offline."""

    def __init__(self, study: Study):
        self._study = study
        self._cache: list[mira.Sample] | None = None

    def _materialized(self) -> list[mira.Sample]:
        if self._cache is None:
            self._cache = self._study._build_samples()
        return self._cache

    def __getitem__(self, index):
        return self._materialized()[index]

    def __len__(self) -> int:
        return len(self._materialized())


def _build_study() -> Study:
    return Study(
        max_cost_usd=_float("TB_MAX_COST_USD", None),
        job_timeout=_int("TB_JOB_TIMEOUT", 5400),
        keep_jobs=_bool("TB_KEEP_JOBS", True),
    )


def serve(study: Study | None = None) -> None:
    """Build the study and run the SDK's stdio protocol loop until stdin closes."""
    study = study or _build_study()
    budget = f"${study.max_cost_usd:.2f}" if study.max_cost_usd is not None else "unset"
    log(f"ready: study=terminal_bench dataset={DATASET} budget={budget}")
    study.protocol().serve()


def main() -> int:
    serve()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

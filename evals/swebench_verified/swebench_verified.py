# /// script
# requires-python = ">=3.11"
# dependencies = ["mira-eval>=0.3.0", "pandas>=2.0", "pyarrow>=14.0", "swebench>=4.1.0"]
# ///
"""swebench_verified — a single-file Mira eval study for SWE-bench Verified.

Mira's host CLI speaks newline-delimited JSON over stdio to a child process, so
an eval study can be any program in any language. This is that program for
yolop's SWE-bench Verified benchmark, built on the `mira-eval` Python SDK (the
`mira` package on PyPI) — the SDK owns the stdio protocol loop (initialize / list
/ run / execute / score) while this file owns the SWE-bench-specific work. One
self-contained file with its dependencies declared inline (PEP 723), so `uv`
builds an ephemeral env and runs it with no project scaffolding:

    cd evals/swebench_verified
    mira list                  # mira.toml's default_launcher drives this study
    mira run --samples astropy__astropy-12907 --targets anthropic-claude-sonnet-4.5

(`mira --uv swebench_verified.py …` is the explicit equivalent; `--cmd "uv run
swebench_verified.py"` still works for arbitrary command lines.)

The `mira` host owns the target matrix, selection, concurrency, saved run folders,
and reporting; this study owns the SWE-bench-specific work:

* loads SWE-bench Verified instances (the HF parquet),
* runs a coding agent (yolop, claude-code, codex, or pi) in a fresh checkout,
* mines the agent's event log into common metrics, and
* scores the patch with the official `swebench` Docker `FAIL_TO_PASS` harness.

Mapping to Mira: one **eval** (`swebench_verified`); each instance is a
**sample** (tagged with repo, difficulty, and any curated suite); each entry in
`MATRIX` below is a **target** — the thing under evaluation, here an agent config
(label = config name) — advertised `available: false` when its provider key is
absent so the host skips it.

Study-internal knobs come from the environment (the host can't pass study CLI
flags): `SWEBENCH_NO_EVAL=1` skips Docker scoring, `SWEBENCH_MAX_WORKERS`,
`SWEBENCH_NAMESPACE` (`none` builds images locally), `SWEBENCH_EVAL_TIMEOUT`,
`SWEBENCH_YOLOP_BIN` (override the yolop binary path), `SWEBENCH_CACHE_LEVEL`
(SWE-bench image cache level; `instance` keeps the per-instance image so a
matrix run pulls it once instead of re-pulling per case — default `env`),
`SWEBENCH_IN_CONTAINER=1` (run every yolop config *inside* the instance's
SWE-bench container instead of a host checkout — see below).

By default a yolop case copies `/testbed` out of the image and runs the agent on
the host. That host has no compiled/installed copy of the project (astropy is a
C-extension package, and the host Python is a different version), so the agent
cannot `import` the project or run its tests — weaker models then burn dozens of
turns trying to build the package locally. Running the agent *inside* the
instance container (`container: True` per config, or `SWEBENCH_IN_CONTAINER=1`
for all yolop configs) is the standard SWE-bench setup: the repo is already
installed in the image's `testbed` env, so `import`/tests work immediately and
the agent measures reasoning, not environment-debugging grit. Container mode
needs a yolop binary built for the image's ABI (glibc 2.35 / musl) via
`SWEBENCH_YOLOP_BIN`; the host-built binary will not run in the container.

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
import urllib.request
import uuid
from collections import abc
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator, Mapping, Sequence

import mira  # the mira-eval SDK: protocol types + the stdio serve loop

# The mira-eval SDK pins the protocol version (`mira.PROTOCOL_VERSION`, currently
# "1.0"); everything we emit (open-ended `metadata`, the numeric `metrics` map,
# `error_kind`, and per-target `metadata` columns) is part of that 1.0 baseline.
STUDY_VERSION = "0.6.0"

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]                      # evals/swebench_verified -> repo root
CACHE_DIR = HERE / ".cache"
SUITES_DIR = HERE / "suites"
YOLOP_BIN = os.environ.get("SWEBENCH_YOLOP_BIN") or str(REPO_ROOT / "target/release/yolop")


def log(msg: str) -> None:
    print(f"[swebench_verified] {msg}", file=sys.stderr, flush=True)


# ============================================================================ #
# Shared records
# ============================================================================ #
@dataclass
class TokenUsage:
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0

    @property
    def total_tokens(self) -> int:
        return self.input_tokens + self.output_tokens


@dataclass
class RunMetrics:
    """Operational metrics for one agent run, mined from its event log."""

    wall_time_s: float = 0.0
    agent_reported_time_s: float | None = None
    turns: int = 0
    iterations: int = 0
    assistant_messages: int = 0
    user_messages: int = 0
    llm_calls: int = 0
    tool_calls: int = 0
    tool_calls_failed: int = 0
    tools_used: dict[str, int] = field(default_factory=dict)
    tokens: TokenUsage = field(default_factory=TokenUsage)
    reasoning_effort: str | None = None
    cost_usd: float = 0.0
    stop_reason: str = "completed"  # completed | timeout | budget | error


@dataclass
class AgentRun:
    patch: str
    metrics: RunMetrics
    session_log_path: str | None = None
    error: str | None = None


@dataclass
class Instance:
    instance_id: str
    problem_statement: str
    repo: str | None = None
    base_commit: str | None = None
    extra: dict[str, Any] = field(default_factory=dict)


@dataclass
class EvalResult:
    resolved: bool
    report: dict[str, Any] = field(default_factory=dict)
    error: str | None = None


# ============================================================================ #
# The config matrix (one entry = one Mira target). Inlined here so the study is
# a single file. `agent:` picks the adapter; remaining keys go to it. yolop
# configs run the binary at REPO_ROOT/target/release/yolop (override with
# SWEBENCH_YOLOP_BIN).
# ============================================================================ #
DEFAULTS: dict[str, Any] = {"timeout": 1800, "max_cost_usd": 5.0}

MATRIX: dict[str, dict[str, Any]] = {
    # yolop x OpenAI (default provider)
    "openai-gpt-5.5": {"agent": "yolop", "provider": "openai", "model": "gpt-5.5"},
    "openai-gpt-5.5-high": {"agent": "yolop", "provider": "openai", "model": "gpt-5.5",
                    "reasoning_effort": "high"},
    # Low/no-reasoning baselines. yolop otherwise applies the model profile's
    # default effort to gpt-5.5, so these isolate how much of yolop's turn/cost
    # overhead is reasoning vs harness — and put it on equal footing with `pi`,
    # which runs gpt-5.5 with no reasoning controls. effort "none" omits the
    # reasoning param entirely; "low" is the lowest reasoning tier. (gpt-5.5
    # supports none/low/medium/high/xhigh — "minimal" is rejected by the API.)
    "openai-gpt-5.5-none": {"agent": "yolop", "provider": "openai", "model": "gpt-5.5",
                    "reasoning_effort": "none"},
    "openai-gpt-5.5-low": {"agent": "yolop", "provider": "openai", "model": "gpt-5.5",
                    "reasoning_effort": "low"},
    # yolop x Anthropic (secondary provider)
    "anthropic-claude-sonnet-4.5": {"agent": "yolop", "provider": "anthropic", "model": "claude-sonnet-4-5"},
    "anthropic-claude-opus-4.8": {"agent": "yolop", "provider": "anthropic", "model": "claude-opus-4-8"},
    # yolop x OpenRouter (needs OPENROUTER_API_KEY). `container: True` runs the
    # agent inside the instance's SWE-bench image, where the project is already
    # installed — without it these open models waste 60-100+ turns trying to
    # build astropy on the host before they can test their fix.
    "openrouter-nemotron-3-ultra": {"agent": "yolop", "provider": "openrouter",
                            "model": "nvidia/nemotron-3-ultra-550b-a55b", "container": True},
    "openrouter-glm-5.2": {"agent": "yolop", "provider": "openrouter", "model": "z-ai/glm-5.2",
                            "container": True},
    # kimi-k2.7-code's endpoint rejects requests with reasoning disabled
    # ("Reasoning is mandatory for this endpoint"), so an effort must be set. Keep
    # it low: at high effort the model emits long silent thinking blocks that
    # exceed yolop's 120s stream-stall guard ("no tokens for 120s") and abort.
    "openrouter-kimi-k2.7-code": {"agent": "yolop", "provider": "openrouter",
                            "model": "moonshotai/kimi-k2.7-code", "reasoning_effort": "low",
                            "container": True},
    "openrouter-qwen3-coder": {"agent": "yolop", "provider": "openrouter",
                            "model": "qwen/qwen3-coder", "container": True},
    # Offline plumbing check (no key; won't solve tasks)
    "llmsim": {"agent": "yolop", "provider": "llmsim"},
    # Other coding agents (need their CLI on PATH + provider keys)
    "claude-code-sonnet-4.5": {"agent": "claude-code", "model": "claude-sonnet-4-5"},
    "claude-code-opus-4.8": {"agent": "claude-code", "model": "claude-opus-4-8"},
    # codex reports tokens but no cost; `price` (USD per 1M) lets us estimate +
    # cap. sandbox: bypass — its Landlock sandbox can't init as root; the
    # checkout is already isolated.
    "codex": {"agent": "codex", "model": "gpt-5.5", "sandbox": "bypass",
              "price": {"input": 1.25, "output": 10.0, "cache_read": 0.125}},
    "pi": {"agent": "pi", "provider": "openai", "model": "gpt-5.5"},
}

# Provider -> env var whose presence makes its configs runnable; llmsim is the
# offline provider and is always available. Agents without a `provider:` use a
# fixed key (their CLI's own).
_PROVIDER_KEYS = {"openai": "OPENAI_API_KEY", "anthropic": "ANTHROPIC_API_KEY",
                  "openrouter": "OPENROUTER_API_KEY", "llmsim": None}
_AGENT_KEYS = {"claude-code": "ANTHROPIC_API_KEY", "codex": "OPENAI_API_KEY"}


def config_available(spec: dict) -> bool:
    """Whether a matrix config can run given the current environment."""
    provider = spec.get("provider")
    if provider == "llmsim":
        return True
    if provider in _PROVIDER_KEYS:
        return bool(os.environ.get(_PROVIDER_KEYS[provider]))
    key = _AGENT_KEYS.get(spec.get("agent", "yolop"))
    return bool(os.environ.get(key)) if key else False


def _target_metadata(spec: dict) -> dict[str, Any]:
    """Descriptive config that rides the target column (open JSON). Lets the host
    surface it per column and group results with `mira run --group-by agent`."""
    cfg = {**DEFAULTS, **spec}
    md: dict[str, Any] = {"agent": cfg.get("agent", "yolop")}
    for k in ("model", "provider", "reasoning_effort", "sandbox", "container",
              "max_cost_usd", "price"):
        if cfg.get(k) is not None:
            md[k] = cfg[k]
    return md


# Shared issue-fixing prompt so the only variable across agents is the agent.
PROMPT_TEMPLATE = """\
You are fixing a bug in the {repo} repository. The working directory is a clean \
checkout at the relevant base commit.

Resolve the following issue by editing the source code. Do not write new tests; \
the project's own hidden test suite will be used to verify your fix. Make the \
smallest change that correctly fixes the problem.

--- ISSUE ---
{problem_statement}
--- END ISSUE ---

When you are done, ensure the change is saved to the files on disk."""


# ============================================================================ #
# Subprocess driver — runs an agent CLI to completion, enforcing a wall-clock
# timeout and a per-run USD cap (observed via a cost_probe over the agent's own
# incrementally-written log, so the cap works the same across agents).
# ============================================================================ #
@dataclass
class ProcResult:
    stop_reason: str
    error: str | None
    returncode: int | None
    stderr_tail: str


def run_agent_process(
    cmd: Sequence[str], *, env: dict[str, str], name: str,
    cwd: str | Path | None = None, timeout: float = 1800,
    stdout_log: str | Path | None = None, max_cost_usd: float | None = None,
    cost_probe: Callable[[], float | None] | None = None, poll_s: float = 2.0,
) -> ProcResult:
    start = time.monotonic()
    stdout_f = open(stdout_log, "wb") if stdout_log else None
    try:
        proc = subprocess.Popen(
            list(cmd), env=env, cwd=str(cwd) if cwd else None,
            stdin=subprocess.DEVNULL, stdout=stdout_f or subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    finally:
        if stdout_f:
            stdout_f.close()

    error: str | None = None
    stop_reason = "completed"
    while True:
        try:
            proc.wait(timeout=poll_s)
            break
        except subprocess.TimeoutExpired:
            # Still running at the poll interval — fall through to the wall-clock
            # and budget checks below, then loop back and keep polling.
            pass
        if time.monotonic() - start > timeout:
            _kill(proc)
            stop_reason, error = "timeout", f"{name} timed out after {timeout}s"
            break
        if max_cost_usd is not None and cost_probe is not None:
            try:
                cost = cost_probe()
            except Exception:
                cost = None
            if cost is not None and cost > max_cost_usd:
                _kill(proc)
                stop_reason = "budget"
                error = f"budget cap hit: ${cost:.2f} > ${max_cost_usd:.2f}"
                break

    stderr_tail = ""
    if proc.stderr:
        try:
            stderr_tail = proc.stderr.read().decode("utf-8", "replace")[-2000:]
        except Exception:
            # stderr tail is best-effort diagnostics; never fail the run over it.
            pass
    if stop_reason == "completed" and proc.returncode not in (0, None):
        stop_reason = "error"
        error = f"{name} exited {proc.returncode}: {stderr_tail}"
    return ProcResult(stop_reason, error, proc.returncode, stderr_tail)


def _kill(proc: subprocess.Popen) -> None:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


def tool_version(cmd: Sequence[str]) -> str | None:
    try:
        out = subprocess.run(list(cmd), capture_output=True, text=True, timeout=30)
        line = (out.stdout or out.stderr).strip().splitlines()
        return line[0] if line else None
    except Exception:
        return None


# ============================================================================ #
# Metrics — mine each agent's newline-delimited JSON event log into RunMetrics.
# Tolerant by design: truncated logs (killed runs) and schema drift degrade to
# zeros rather than raising. cost_usd prefers the tool's reported cost, else a
# per-1M `price`-based estimate.
# ============================================================================ #
def iter_events(path: Path) -> Iterator[dict]:
    if not path.exists():
        return
    with path.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue  # skip a partially-written trailing line (crash/SIGKILL)


def cost_from_usage(tokens: TokenUsage, price: dict | None) -> float | None:
    """Estimate USD from token counts and a per-1M price block; None if unpriced."""
    if not price:
        return None
    in_p = float(price.get("input", 0.0))
    out_p = float(price.get("output", 0.0))
    cr_p = float(price.get("cache_read", in_p))
    cw_p = float(price.get("cache_write", in_p))
    cost = (tokens.input_tokens * in_p + tokens.output_tokens * out_p
            + tokens.cache_read_tokens * cr_p + tokens.cache_creation_tokens * cw_p) / 1_000_000.0
    return round(cost, 6)


def _finalize_cost(m: RunMetrics, reported: float | None, price: dict | None) -> None:
    if reported is not None:
        m.cost_usd = round(reported, 6)
    else:
        est = cost_from_usage(m.tokens, price)
        if est is not None:
            m.cost_usd = est


# -- yolop (events.jsonl: one everruns Event per line) ----------------------- #
def _yolop_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    tokens.input_tokens += int(usage.get("input_tokens") or 0)
    tokens.output_tokens += int(usage.get("output_tokens") or 0)
    tokens.cache_read_tokens += int(usage.get("cache_read_tokens") or 0)
    tokens.cache_creation_tokens += int(usage.get("cache_creation_tokens") or 0)


def _yolop_usage_cost(usage: dict | None) -> float:
    if not usage:
        return 0.0
    actual = usage.get("actual_cost_usd")
    if actual is not None:
        return float(actual)
    return float(usage.get("estimated_cost_usd") or 0.0)


def running_cost(session_log_path: str | Path) -> float:
    """Cheap incremental cost read for in-flight budget enforcement (yolop)."""
    total = 0.0
    for ev in iter_events(Path(session_log_path)):
        if ev.get("type") == "output.message.completed":
            total += _yolop_usage_cost((ev.get("data") or {}).get("usage"))
    return total


def extract_yolop(log_path: str | Path) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m
    turn_duration_ms = reason_duration_ms = 0
    saw_turn = False
    for ev in iter_events(path):
        etype = ev.get("type", "")
        data = ev.get("data") or {}
        if etype == "output.message.completed":
            m.assistant_messages += 1
            m.llm_calls += 1
            _yolop_usage(m.tokens, data.get("usage"))
            m.cost_usd = round(m.cost_usd + _yolop_usage_cost(data.get("usage")), 6)
            meta = (data.get("message") or {}).get("metadata") or {}
            if meta.get("reasoning_effort"):
                m.reasoning_effort = meta["reasoning_effort"]
        elif etype == "input.message":
            m.user_messages += 1
        elif etype == "tool.completed":
            m.tool_calls += 1
            name = data.get("tool_name") or "unknown"
            m.tools_used[name] = m.tools_used.get(name, 0) + 1
            if data.get("success") is False:
                m.tool_calls_failed += 1
        elif etype == "reason.completed":
            m.iterations += 1
            if (dur := data.get("duration_ms")) is not None:
                reason_duration_ms += int(dur)
        elif etype == "turn.completed":
            saw_turn = True
            m.turns += 1
            if (dur := data.get("duration_ms")) is not None:
                turn_duration_ms += int(dur)
    if not saw_turn:
        m.turns = m.iterations
        if reason_duration_ms:
            m.agent_reported_time_s = round(reason_duration_ms / 1000.0, 3)
    elif turn_duration_ms:
        m.agent_reported_time_s = round(turn_duration_ms / 1000.0, 3)
    return m


# -- claude-code (stream-json: assistant/user/result) ------------------------ #
def _claude_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    tokens.input_tokens += int(usage.get("input_tokens") or 0)
    tokens.output_tokens += int(usage.get("output_tokens") or 0)
    tokens.cache_read_tokens += int(usage.get("cache_read_input_tokens") or 0)
    tokens.cache_creation_tokens += int(usage.get("cache_creation_input_tokens") or 0)


def extract_claude_code(log_path: str | Path, price: dict | None = None) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m
    summed = TokenUsage()
    final_usage: dict | None = None
    reported_cost: float | None = None
    for ev in iter_events(path):
        etype = ev.get("type")
        if etype == "assistant":
            msg = ev.get("message") or {}
            m.assistant_messages += 1
            m.llm_calls += 1
            _claude_usage(summed, msg.get("usage"))
            for block in msg.get("content") or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    m.tool_calls += 1
                    name = block.get("name") or "unknown"
                    m.tools_used[name] = m.tools_used.get(name, 0) + 1
        elif etype == "user":
            content = (ev.get("message") or {}).get("content")
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "tool_result" \
                            and block.get("is_error"):
                        m.tool_calls_failed += 1
        elif etype == "result":
            if ev.get("num_turns") is not None:
                m.turns = int(ev["num_turns"])
            if ev.get("duration_ms") is not None:
                m.agent_reported_time_s = round(int(ev["duration_ms"]) / 1000.0, 3)
            if ev.get("total_cost_usd") is not None:
                reported_cost = float(ev["total_cost_usd"])
            if isinstance(ev.get("usage"), dict):
                final_usage = ev["usage"]
    if final_usage is not None:
        _claude_usage(m.tokens, final_usage)
    else:
        m.tokens = summed
    m.iterations = m.llm_calls
    if not m.turns:
        m.turns = m.iterations
    _finalize_cost(m, reported_cost, price)
    return m


# -- codex (exec --json: item.completed/turn.completed) ---------------------- #
def _codex_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    # input_tokens includes the cached portion; subtract it to avoid double count.
    cached = int(usage.get("cached_input_tokens") or usage.get("cache_read_tokens") or 0)
    tokens.input_tokens += max(int(usage.get("input_tokens") or 0) - cached, 0)
    tokens.output_tokens += int(usage.get("output_tokens") or 0)
    tokens.cache_read_tokens += cached


def extract_codex(log_path: str | Path, price: dict | None = None) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m
    summed = TokenUsage()
    cumulative: dict | None = None
    reasoning_steps = 0
    for ev in iter_events(path):
        msg = ev.get("msg") or {}
        etype = ev.get("type") or msg.get("type")
        item = ev.get("item") or {}
        itype = item.get("type")
        if etype == "item.completed" and itype == "command_execution":
            m.tool_calls += 1
            m.tools_used["command"] = m.tools_used.get("command", 0) + 1
            if item.get("exit_code") not in (0, None):
                m.tool_calls_failed += 1
        elif etype == "item.completed" and itype == "file_change":
            m.tool_calls += 1
            m.tools_used["file_change"] = m.tools_used.get("file_change", 0) + 1
        elif etype == "item.completed" and itype == "agent_message":
            m.assistant_messages += 1
        elif etype == "item.completed" and itype == "reasoning":
            reasoning_steps += 1
        elif etype == "exec_command_end":  # older schema
            m.tool_calls += 1
            m.tools_used["command"] = m.tools_used.get("command", 0) + 1
            if msg.get("exit_code") not in (0, None):
                m.tool_calls_failed += 1
        elif etype == "agent_message":  # older schema
            m.assistant_messages += 1
        elif etype == "agent_reasoning":  # older schema
            reasoning_steps += 1
        elif etype == "token_count":  # older schema, cumulative
            info = msg.get("info") or {}
            cumulative = info.get("total_token_usage") or cumulative
        if etype == "turn.completed":
            m.turns += 1
            if isinstance(ev.get("usage"), dict):
                _codex_usage(summed, ev["usage"])
    if cumulative is not None:
        _codex_usage(m.tokens, cumulative)
    else:
        m.tokens = summed
    m.llm_calls = m.assistant_messages
    # `codex exec` emits a single turn.completed for the whole run, so `turns`
    # is not a measure of agentic loop steps. Count model round-trips instead:
    # each round-trip emits one `reasoning` item (codex runs reasoning models
    # here). Fall back to assistant messages, then turns, when reasoning
    # summaries are absent.
    m.iterations = reasoning_steps or m.assistant_messages or m.turns
    if not m.turns:
        m.turns = m.iterations
    _finalize_cost(m, None, price)  # codex reports no cost
    return m


# -- pi (--mode json: message_end/tool_execution_end/turn_end) --------------- #
def _pi_usage(tokens: TokenUsage, usage: dict | None) -> None:
    if not usage:
        return
    tokens.input_tokens += int(usage.get("input") or 0)
    tokens.output_tokens += int(usage.get("output") or 0)
    tokens.cache_read_tokens += int(usage.get("cacheRead") or 0)
    tokens.cache_creation_tokens += int(usage.get("cacheWrite") or 0)


def extract_pi(log_path: str | Path, price: dict | None = None) -> RunMetrics:
    path = Path(log_path)
    m = RunMetrics()
    if not path.exists():
        return m
    reported_cost = 0.0
    saw_cost = False
    for ev in iter_events(path):
        etype = ev.get("type")
        if etype == "message_end":
            usage = (ev.get("message") or {}).get("usage") or ev.get("usage") or {}
            m.assistant_messages += 1
            m.llm_calls += 1
            _pi_usage(m.tokens, usage)
            total = (usage.get("cost") or {}).get("total")
            if total is not None:
                reported_cost += float(total)
                saw_cost = True
        elif etype == "tool_execution_end":
            m.tool_calls += 1
            name = ev.get("toolName") or "unknown"
            m.tools_used[name] = m.tools_used.get(name, 0) + 1
            result = ev.get("result")
            if isinstance(result, dict) and (result.get("isError") or result.get("is_error")):
                m.tool_calls_failed += 1
        elif etype == "turn_end":
            m.turns += 1
    m.iterations = m.turns or m.llm_calls
    if not m.turns:
        m.turns = m.iterations
    _finalize_cost(m, reported_cost if saw_cost else None, price)
    return m


# ============================================================================ #
# Agents — run a CLI in the checkout (it edits files in place); the runner
# captures the diff. Each returns an AgentRun with mined metrics. Dispatch on
# the config's `agent:`.
# ============================================================================ #
def _prompt(instance: Instance) -> str:
    return PROMPT_TEMPLATE.format(repo=instance.repo or "the",
                                  problem_statement=instance.problem_statement)


def run_yolop(cfg: dict, instance: Instance, workdir: str, session_dir: str) -> AgentRun:
    session_id = "session_" + uuid.uuid4().hex
    # yolop defaults to worktree mode and would move edits onto a new branch in a
    # linked worktree — our diff of `workdir` would then see nothing. Force it
    # off via an isolated XDG config dir so edits land in `workdir`.
    config_home = Path(session_dir) / "xdg_config"
    cfg_dir = config_home / "yolop"
    cfg_dir.mkdir(parents=True, exist_ok=True)
    (cfg_dir / "settings.toml").write_text('worktrees = "off"\n', encoding="utf-8")

    cmd = [YOLOP_BIN, "-C", workdir]
    if cfg.get("provider"):
        cmd += ["--provider", cfg["provider"]]
    if cfg.get("model"):
        cmd += ["--model", cfg["model"]]
    if cfg.get("reasoning_effort"):
        cmd += ["--reasoning-effort", cfg["reasoning_effort"]]
    cmd += list(cfg.get("extra_args") or [])
    cmd += ["--session", session_id, "--session-dir", session_dir, "-p", _prompt(instance)]

    env = dict(os.environ)
    env["XDG_CONFIG_HOME"] = str(config_home)
    env.update(cfg.get("env") or {})
    log_path = Path(session_dir) / session_id / "events.jsonl"

    start = time.monotonic()
    res = run_agent_process(
        cmd, env=env, name="yolop", timeout=cfg["timeout"],
        max_cost_usd=cfg.get("max_cost_usd"), cost_probe=lambda: running_cost(log_path),
    )
    error, stop_reason = res.error, res.stop_reason
    if log_path.exists():
        metrics = extract_yolop(log_path)
    else:
        metrics = RunMetrics()
        if error is None:
            error, stop_reason = f"no session log at {log_path}", "error"
    metrics.wall_time_s = round(time.monotonic() - start, 3)
    metrics.stop_reason = stop_reason
    return AgentRun(patch="", metrics=metrics, session_log_path=str(log_path), error=error)


# Env that lets the in-container agent reach the LLM provider through the host's
# egress proxy (the SWE-bench image carries no proxy/CA config of its own).
_PROXY_ENV = ("HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "https_proxy", "http_proxy",
              "no_proxy", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE",
              "GIT_SSL_CAINFO", "NODE_EXTRA_CA_CERTS")
_PROVIDER_KEY_ENV = ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY")


def _yolop_in_container(spec: dict) -> bool:
    """True when this yolop config should run inside the instance container
    (per-config `container: True`, or `SWEBENCH_IN_CONTAINER` for all)."""
    if spec.get("agent", "yolop") != "yolop":
        return False
    if spec.get("container") is not None:
        return bool(spec["container"])
    return os.environ.get("SWEBENCH_IN_CONTAINER", "").lower() in ("1", "true", "yes")


def _docker(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", *args], capture_output=True, text=True)


def _container_yolop_bin() -> str:
    """The yolop binary to bind-mount into the instance image. The image ABI
    (glibc 2.35) differs from the host, so prefer an explicit override, then a
    static musl build (runs in any image), then fall back to the host binary."""
    explicit = os.environ.get("SWEBENCH_YOLOP_BIN")
    if explicit:
        return explicit
    musl = REPO_ROOT / "target/x86_64-unknown-linux-musl/release/yolop"
    return str(musl) if musl.exists() else YOLOP_BIN


def _container_patch(cid: str) -> str:
    """The agent's edits as a patch, captured from /testbed inside the container
    (mirrors the host `capture_patch`: stage all, diff cached)."""
    git = ["exec", cid, "git", "-c", "safe.directory=/testbed", "-C", "/testbed"]
    add = _docker([*git, "add", "-A"])
    if add.returncode != 0:
        raise RuntimeError(f"container git add failed: {add.stderr.strip()}")
    diff = _docker([*git, "diff", "--cached", "--no-color"])
    if diff.returncode != 0:
        raise RuntimeError(f"container git diff failed: {diff.stderr.strip()}")
    return diff.stdout


def run_yolop_container(cfg: dict, instance: Instance, session_dir: str) -> AgentRun:
    """Run yolop *inside* the instance's SWE-bench image, where the project is
    already installed (the image's `testbed` env). The agent edits /testbed in
    place and the patch is captured there — so `import`/tests work immediately
    and the agent never has to build the package. The host `YOLOP_BIN` is
    bind-mounted, so it must be built for the image ABI (glibc 2.35 / musl); set
    `SWEBENCH_YOLOP_BIN`. Decoupled from scoring: `_score` still runs the official
    evaluator in its own fresh container against the captured patch.
    """
    cfg = {**DEFAULTS, **cfg}  # the host path merges these in run_agent; mirror it
    session_id = "session_" + uuid.uuid4().hex
    image = _ensure_image(instance)
    sess = Path(session_dir)
    sess.mkdir(parents=True, exist_ok=True)
    log_path = sess / session_id / "events.jsonl"

    # worktrees off so edits land in /testbed (not a linked branch); settings
    # live under the mounted session dir.
    cfg_home = sess / "xdg_config"
    (cfg_home / "yolop").mkdir(parents=True, exist_ok=True)
    (cfg_home / "yolop" / "settings.toml").write_text('worktrees = "off"\n', encoding="utf-8")

    # Long-lived container on the host network so it can reach the egress proxy.
    run_cmd = ["run", "-d", "--rm", "--network", "host",
               "-v", f"{_container_yolop_bin()}:/usr/local/bin/yolop:ro", "-v", f"{sess}:{sess}"]
    ca = os.environ.get("SSL_CERT_FILE")
    if ca and os.path.exists(ca):
        run_cmd += ["-v", f"{ca}:{ca}:ro"]
    run_cmd += [image, "sleep", "infinity"]
    created = _docker(run_cmd)
    if created.returncode != 0:
        raise RuntimeError(f"docker run {image} failed: {created.stderr.strip()}")
    cid = created.stdout.strip()

    error: str | None = None
    stop_reason = "completed"
    patch = ""
    start = time.monotonic()
    try:
        # Pristine base_commit; `clean -qfd` drops untracked cruft but keeps
        # gitignored build artifacts (compiled extensions) so the project stays
        # importable.
        gitc = ["exec", cid, "git", "-c", "safe.directory=/testbed", "-C", "/testbed"]
        if _docker([*gitc, "reset", "-q", "--hard", instance.base_commit]).returncode != 0:
            _docker([*gitc, "checkout", "-q", "-f", instance.base_commit])
        _docker([*gitc, "clean", "-qfd"])

        exec_cmd = ["docker", "exec", "-w", "/testbed", "-e", f"XDG_CONFIG_HOME={cfg_home}"]
        # Forward proxy/CA + provider keys by NAME only; `docker exec` reads the
        # value from this process's env (passed below), so secrets never appear
        # in argv (host `ps`).
        for k in (*_PROXY_ENV, *_PROVIDER_KEY_ENV):
            if os.environ.get(k):
                exec_cmd += ["-e", k]
        for k, v in (cfg.get("env") or {}).items():
            exec_cmd += ["-e", f"{k}={v}"]
        # A login shell activates the image's `testbed` conda env (via .bashrc),
        # so `python`/`pip` in the agent's tool calls resolve to the installed
        # project; `exec yolop "$@"` passes argv through unparsed (no shell
        # quoting of the problem statement).
        yargs = ["-C", "/testbed"]
        if cfg.get("provider"):
            yargs += ["--provider", cfg["provider"]]
        if cfg.get("model"):
            yargs += ["--model", cfg["model"]]
        if cfg.get("reasoning_effort"):
            yargs += ["--reasoning-effort", cfg["reasoning_effort"]]
        yargs += list(cfg.get("extra_args") or [])
        yargs += ["--session", session_id, "--session-dir", str(sess), "-p", _prompt(instance)]
        exec_cmd += [cid, "bash", "-lc", 'exec yolop "$@"', "yolop", *yargs]

        start = time.monotonic()
        res = run_agent_process(
            exec_cmd, env=dict(os.environ), name="yolop", timeout=cfg["timeout"],
            max_cost_usd=cfg.get("max_cost_usd"), cost_probe=lambda: running_cost(log_path),
        )
        error, stop_reason = res.error, res.stop_reason
        patch = _container_patch(cid)
    finally:
        _docker(["rm", "-f", cid])

    if log_path.exists():
        metrics = extract_yolop(log_path)
    else:
        metrics = RunMetrics()
        if error is None:
            error, stop_reason = f"no session log at {log_path}", "error"
    metrics.wall_time_s = round(time.monotonic() - start, 3)
    metrics.stop_reason = stop_reason
    return AgentRun(patch=patch, metrics=metrics, session_log_path=str(log_path), error=error)


def _run_cli_agent(cfg: dict, instance: Instance, workdir: str, session_dir: str, *,
                   name: str, log_name: str, build_cmd, extract, env_extra=None) -> AgentRun:
    """Shared body for the stdout-JSON agents (claude-code, codex, pi)."""
    log_path = Path(session_dir) / log_name
    price = cfg.get("price")
    env = dict(os.environ)
    env.update(env_extra or {})
    env.update(cfg.get("env") or {})
    start = time.monotonic()
    res = run_agent_process(
        build_cmd(cfg, instance, workdir), env=env, name=name, cwd=workdir,
        timeout=cfg["timeout"], stdout_log=log_path, max_cost_usd=cfg.get("max_cost_usd"),
        cost_probe=lambda: extract(log_path, price).cost_usd,
    )
    metrics = extract(log_path, price)
    metrics.wall_time_s = round(time.monotonic() - start, 3)
    metrics.stop_reason = res.stop_reason
    return AgentRun(patch="", metrics=metrics, session_log_path=str(log_path), error=res.error)


def run_claude_code(cfg, instance, workdir, session_dir) -> AgentRun:
    def build(cfg, inst, wd):
        cmd = [cfg.get("binary", "claude"), "-p", _prompt(inst),
               "--output-format", "stream-json", "--verbose",
               "--dangerously-skip-permissions"]
        if cfg.get("model"):
            cmd += ["--model", cfg["model"]]
        return cmd + list(cfg.get("extra_args") or [])
    # Claude Code rejects skip-permissions under root unless it believes it's
    # sandboxed; the checkout is already isolated. Only "1" is truthy.
    return _run_cli_agent(cfg, instance, workdir, session_dir, name="claude-code",
                          log_name="claude_code.jsonl", build_cmd=build,
                          extract=extract_claude_code, env_extra={"IS_SANDBOX": "1"})


def run_codex(cfg, instance, workdir, session_dir) -> AgentRun:
    def build(cfg, inst, wd):
        cmd = [cfg.get("binary", "codex"), "exec", _prompt(inst), "--json", "--cd", wd]
        if cfg.get("model"):
            cmd += ["--model", cfg["model"]]
        cmd += (["--dangerously-bypass-approvals-and-sandbox"]
                if cfg.get("sandbox") == "bypass" else ["--full-auto"])
        return cmd + list(cfg.get("extra_args") or [])
    return _run_cli_agent(cfg, instance, workdir, session_dir, name="codex",
                          log_name="codex.jsonl", build_cmd=build, extract=extract_codex)


def run_pi(cfg, instance, workdir, session_dir) -> AgentRun:
    def build(cfg, inst, wd):
        cmd = [cfg.get("binary", "pi"), "-p", _prompt(inst), "--mode", "json", "-a"]
        if cfg.get("provider"):
            cmd += ["--provider", cfg["provider"]]
        if cfg.get("model"):
            cmd += ["--model", cfg["model"]]
        return cmd + list(cfg.get("extra_args") or [])
    return _run_cli_agent(cfg, instance, workdir, session_dir, name="pi",
                          log_name="pi.jsonl", build_cmd=build, extract=extract_pi)


_AGENTS: dict[str, Callable[..., AgentRun]] = {
    "yolop": run_yolop, "claude-code": run_claude_code, "codex": run_codex, "pi": run_pi,
}


def run_agent(spec: dict, instance: Instance, workdir: str, session_dir: str) -> AgentRun:
    cfg = {**DEFAULTS, **spec}
    agent = cfg.get("agent", "yolop")
    try:
        runner = _AGENTS[agent]
    except KeyError:
        raise ValueError(f"unknown agent: {agent!r}") from None
    return runner(cfg, instance, workdir, session_dir)


# ============================================================================ #
# Workspace — checkout a repo at base_commit; capture the working-tree diff.
# ============================================================================ #
def _git(args: list[str], cwd: str | Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=str(cwd) if cwd else None,
                          capture_output=True, text=True)


def _instance_image_key(instance_id: str, namespace: str) -> str:
    """The SWE-bench instance Docker image that hosts the repo at base_commit.

    Derived via swebench's own `make_test_spec` so the name matches exactly what
    the scorer pulls (e.g. `swebench/sweb.eval.x86_64.astropy_1776_…:latest`)."""
    raw = _RAW.get(instance_id)
    if not raw:
        raise RuntimeError(f"{instance_id}: dataset row not loaded; cannot derive image key")
    from swebench.harness.test_spec.test_spec import make_test_spec

    return make_test_spec(raw, namespace=namespace).instance_image_key


def _ensure_image(instance: Instance) -> str:
    """Resolve the instance's SWE-bench image key, pulling it if absent. Shared by
    the host checkout (copies /testbed out) and container mode (runs the agent in
    the image directly). Cached per instance via SWEBENCH_CACHE_LEVEL."""
    namespace = os.environ.get("SWEBENCH_NAMESPACE", "swebench")
    image = _instance_image_key(instance.instance_id, namespace)
    if subprocess.run(["docker", "image", "inspect", image],
                      capture_output=True).returncode != 0:
        pull = subprocess.run(["docker", "pull", image], capture_output=True, text=True)
        if pull.returncode != 0:
            raise RuntimeError(f"docker pull {image} failed for {instance.instance_id}: "
                               f"{pull.stderr.strip()}")
    return image


def checkout(instance: Instance, dest: Path) -> None:
    """Materialize instance.repo at base_commit in dest from the SWE-bench image.

    Copies `/testbed` out of the instance's prebuilt SWE-bench Docker image — the
    authoritative repo state the scorer evaluates against — rather than cloning
    github, which some network egress policies deny (403 on git-upload-pack). The
    working tree is reset to a pristine base_commit so HEAD == base_commit and
    `capture_patch` (git diff --cached) yields exactly the agent's edits.
    """
    if not instance.repo or not instance.base_commit:
        raise RuntimeError(f"{instance.instance_id}: missing repo/base_commit")
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True, exist_ok=True)

    image = _ensure_image(instance)

    # Copy /testbed out of a throwaway container, then make the working tree
    # pristine at base_commit (the image may carry build artifacts / generated
    # files that would otherwise leak into the captured patch).
    create = subprocess.run(["docker", "create", image], capture_output=True, text=True)
    if create.returncode != 0:
        raise RuntimeError(f"docker create {image} failed for {instance.instance_id}: "
                           f"{create.stderr.strip()}")
    cid = create.stdout.strip()
    try:
        cp = subprocess.run(["docker", "cp", f"{cid}:/testbed/.", str(dest)],
                            capture_output=True, text=True)
        if cp.returncode != 0:
            raise RuntimeError(f"docker cp from {image} failed for {instance.instance_id}: "
                               f"{cp.stderr.strip()}")
    finally:
        subprocess.run(["docker", "rm", "-f", cid], capture_output=True)

    if not (dest / ".git").exists():
        raise RuntimeError(f"{instance.instance_id}: /testbed has no git repo")
    reset = _git(["reset", "-q", "--hard", instance.base_commit], dest)
    if reset.returncode != 0:  # fall back to a detached checkout of the SHA
        reset = _git(["checkout", "-q", "-f", instance.base_commit], dest)
    if reset.returncode != 0:
        raise RuntimeError(f"git reset to base_commit failed for {instance.instance_id}: "
                           f"{reset.stderr.strip()}")
    _git(["clean", "-qfd"], dest)  # drop untracked, non-ignored cruft


def capture_patch(workdir: Path) -> str:
    # Fail loudly: a silent git error here would yield an empty/partial patch and
    # mis-score the case. The caller treats the raise as an infra error.
    add = _git(["add", "-A"], workdir)
    if add.returncode != 0:
        raise RuntimeError(f"git add failed in {workdir}: {add.stderr.strip()}")
    diff = _git(["diff", "--cached", "--no-color"], workdir)
    if diff.returncode != 0:
        raise RuntimeError(f"git diff failed in {workdir}: {diff.stderr.strip()}")
    return diff.stdout


# ============================================================================ #
# Dataset + scoring — SWE-bench Verified. Loaded once from the HF parquet;
# scored with the official `swebench` Docker harness (FAIL_TO_PASS), parsing the
# per-instance report.json back out.
# ============================================================================ #
_HF_PARQUET = ("https://huggingface.co/datasets/princeton-nlp/SWE-bench_Verified"
               "/resolve/main/data/test-00000-of-00001.parquet")
_DATASET_FIELDS = ["repo", "instance_id", "base_commit", "patch", "test_patch",
                   "problem_statement", "hints_text", "created_at", "version",
                   "FAIL_TO_PASS", "PASS_TO_PASS", "environment_setup_commit", "difficulty"]

_RAW: dict[str, dict] = {}  # instance_id -> raw HF row, for the harness dataset file


def _ensure_parquet() -> Path:
    path = CACHE_DIR / "swebench_verified.parquet"
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        log(f"downloading dataset -> {path}")
        urllib.request.urlretrieve(_HF_PARQUET, path)
    return path


def _native(value):
    if hasattr(value, "item"):
        try:
            return value.item()
        except (ValueError, AttributeError):
            pass
    if hasattr(value, "tolist"):
        return value.tolist()
    return value


def load_instances(instance_ids: list[str] | None = None) -> list[Instance]:
    import pandas as pd  # heavy; imported lazily so the module loads without it

    df = pd.read_parquet(_ensure_parquet())
    if instance_ids:
        df = df[df["instance_id"].isin(set(instance_ids))]
    instances: list[Instance] = []
    for _, row in df.iterrows():
        raw = {k: _native(row[k]) for k in _DATASET_FIELDS if k in row}
        _RAW[raw["instance_id"]] = raw
        instances.append(Instance(
            instance_id=raw["instance_id"], problem_statement=raw["problem_statement"],
            repo=raw["repo"], base_commit=raw["base_commit"],
            extra={"version": raw.get("version"), "difficulty": raw.get("difficulty"),
                   "environment_setup_commit": raw.get("environment_setup_commit")},
        ))
    return instances


def evaluate_predictions(predictions: Mapping[str, str], instances: list[Instance],
                         run_id: str, *, max_workers: int = 4, namespace: str = "swebench",
                         timeout: int = 1800) -> dict[str, EvalResult]:
    ids = [i.instance_id for i in instances]
    if not _RAW:
        load_instances(ids)  # evaluate() before load(): repopulate raw rows
    work = CACHE_DIR / "eval" / run_id
    work.mkdir(parents=True, exist_ok=True)
    dataset_file, preds_file = work / "dataset.jsonl", work / "predictions.jsonl"
    with dataset_file.open("w", encoding="utf-8") as fh:
        for iid in ids:
            fh.write(json.dumps(_RAW[iid]) + "\n")
    model_name = "yolop"
    with preds_file.open("w", encoding="utf-8") as fh:
        for iid in ids:
            fh.write(json.dumps({"instance_id": iid, "model_name_or_path": model_name,
                                 "model_patch": predictions.get(iid, "")}) + "\n")
    # SWE-bench's default cache_level "env" deletes the per-instance image after
    # each case, so a multi-target matrix re-pulls the same prebuilt image once
    # per case — which trips Docker Hub's anonymous pull rate limit. "instance"
    # keeps the instance image so the matrix pulls it once and reuses it.
    cache_level = os.environ.get("SWEBENCH_CACHE_LEVEL", "env")
    cmd = [sys.executable, "-m", "swebench.harness.run_evaluation",
           "--dataset_name", str(dataset_file), "--predictions_path", str(preds_file),
           "--run_id", run_id, "--instance_ids", *ids, "--max_workers", str(max_workers),
           "--namespace", namespace, "--timeout", str(timeout), "--report_dir", str(work),
           "--cache_level", cache_level]
    log(f"running evaluation: {' '.join(cmd)}")
    # Keep the harness's copious output OFF this process's stdout (the protocol
    # channel under the Mira host); route it all to stderr.
    proc = subprocess.run(cmd, cwd=str(work), stdout=sys.stderr, stderr=subprocess.STDOUT)

    results: dict[str, EvalResult] = {}
    log_root = work / "logs" / "run_evaluation" / run_id / model_name
    for iid in ids:
        report_path = log_root / iid / "report.json"
        if not report_path.exists():
            results[iid] = EvalResult(False, error=f"no report.json (eval exit {proc.returncode})")
            continue
        entry = json.loads(report_path.read_text()).get(iid, {})
        results[iid] = EvalResult(resolved=bool(entry.get("resolved", False)), report=entry)
    return results


# ============================================================================ #
# Suites — curated instance subsets under suites/*.json, surfaced as sample
# tags so a set can be selected with `mira run --tag <suite>`.
# ============================================================================ #
_SAFE_NAME = re.compile(r"^[A-Za-z0-9._-]+$")


def load_suite(name: str) -> dict:
    if not name or name in (".", "..") or not _SAFE_NAME.match(name):
        raise SystemExit(f"invalid suite name {name!r}")
    path = SUITES_DIR / f"{name}.json"
    if not path.exists():
        raise SystemExit(f"unknown suite {name!r}; available: {available_suites()}")
    return json.loads(path.read_text(encoding="utf-8"))


def suite_instance_ids(name: str) -> list[str]:
    return [i["instance_id"] for i in load_suite(name)["instances"]]


def available_suites() -> list[str]:
    return sorted(p.stem for p in SUITES_DIR.glob("*.json")) if SUITES_DIR.exists() else []


def suite_membership() -> dict[str, list[str]]:
    membership: dict[str, list[str]] = {}
    for name in available_suites():
        for iid in suite_instance_ids(name):
            membership.setdefault(iid, []).append(name)
    return membership


# ============================================================================ #
# The study — owns the SWE-bench-specific work behind the mira-eval SDK. The SDK
# (`mira.Study`) owns the stdio protocol loop (initialize/list/run/execute/score)
# and the scoring/verdict machinery; this `Study` holds the dataset cache and the
# Docker-scoring config, and exposes a subject `(sample, cx) -> mira.Transcript`.
# One process serves many run requests, so the dataset is loaded once and reused.
# ============================================================================ #
def _sanitize(s: str) -> str:
    return "".join(c if c.isalnum() or c in "-._" else "_" for c in s)


def _run_agent_case(inst: Instance, target: str, spec: dict, *,
                    max_workers: int, namespace: str, eval_timeout: int,
                    do_eval: bool) -> tuple[AgentRun, bool, dict, str | None]:
    """Checkout + run the agent + (optionally) Docker-score one case. Module-level
    so the unit tests can patch `checkout`/`run_agent`/`capture_patch`/
    `evaluate_predictions` as globals. Returns (run, resolved, report, error_kind)."""
    run = _run_agent(inst, target, spec)
    # A checkout/setup failure is infra (not the agent's fault).
    error_kind = "infra" if (run.error or "").startswith("runner:") else None
    resolved, report, eval_err = _score(inst, run, target, do_eval=do_eval,
                                        max_workers=max_workers, namespace=namespace,
                                        eval_timeout=eval_timeout)
    if eval_err and not run.error:
        run.error = eval_err  # Docker/harness failed to score
        error_kind = "infra"
    return run, resolved, report, error_kind


def _run_agent(inst: Instance, target: str, spec: dict) -> AgentRun:
    run_id = f"{_sanitize(target)}-{_sanitize(inst.instance_id)}-{uuid.uuid4().hex[:6]}"
    session_dir = CACHE_DIR / "sessions" / run_id
    session_dir.mkdir(parents=True, exist_ok=True)
    # Container mode (yolop only): run the agent inside the instance image, where
    # the project is installed. No host checkout/diff — the patch is captured in
    # /testbed by the runner.
    if _yolop_in_container(spec):
        try:
            return run_yolop_container(spec, inst, str(session_dir))
        except Exception as e:  # report, don't crash the study loop
            log(f"agent run failed for {target}/{inst.instance_id}: {e}")
            return AgentRun(patch="", metrics=RunMetrics(), error=f"runner: {e}")
    workdir = CACHE_DIR / "work" / run_id
    try:
        checkout(inst, workdir)
        run = run_agent(spec, inst, str(workdir), str(session_dir))
        run.patch = capture_patch(workdir)
    except Exception as e:  # report, don't crash the study loop
        log(f"agent run failed for {target}/{inst.instance_id}: {e}")
        run = AgentRun(patch="", metrics=RunMetrics(), error=f"runner: {e}")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    return run


def _score(inst: Instance, run: AgentRun, target: str, *, do_eval: bool,
           max_workers: int, namespace: str, eval_timeout: int):
    if not do_eval:
        return False, {}, None
    try:
        run_id = f"{_sanitize(target)}-{_sanitize(inst.instance_id)}-{uuid.uuid4().hex[:6]}"
        results = evaluate_predictions({inst.instance_id: run.patch}, [inst], run_id,
                                       max_workers=max_workers, namespace=namespace,
                                       timeout=eval_timeout)
        ev = results.get(inst.instance_id)
        if ev is None:
            return False, {}, "no eval result"
        return bool(ev.resolved), ev.report, ev.error
    except Exception as e:  # Docker/infra failure: not resolved, surface it
        log(f"eval failed for {target}/{inst.instance_id}: {e}")
        return False, {}, f"eval: {e}"


def _build_transcript(run: AgentRun, resolved: bool, report: dict,
                      spec: dict, inst: Instance, error_kind: str | None) -> mira.Transcript:
    m = run.metrics
    tools = [name for name, count in m.tools_used.items() for _ in range(count)]
    # `metrics` is the numeric map the host's generic budget scorers read; keep
    # anything numeric here (incl. cache_creation_tokens, which mira.Usage has no
    # field for). `metadata` is open-ended JSON: structured values, not strings.
    metrics: dict[str, float] = {
        "turns": m.turns, "iterations": m.iterations, "tool_calls": m.tool_calls,
        "tool_calls_failed": m.tool_calls_failed,
        "cache_read_tokens": m.tokens.cache_read_tokens,
        "cache_creation_tokens": m.tokens.cache_creation_tokens,
    }
    if m.agent_reported_time_s is not None:
        metrics["agent_reported_time_s"] = m.agent_reported_time_s
    return mira.Transcript(
        final_response=f"resolved={resolved}",
        iterations=m.iterations,
        tool_calls_count=m.tool_calls,
        tool_calls=tools,
        usage=mira.Usage(input_tokens=m.tokens.input_tokens, output_tokens=m.tokens.output_tokens,
                         cache_read_tokens=m.tokens.cache_read_tokens, cost_usd=m.cost_usd),
        timing=mira.Timing(duration_ms=int(m.wall_time_s * 1000)),
        metrics=metrics,
        files={"model_patch.diff": run.patch} if run.patch else {},
        # No `config` here: mira already records the target id as the case `model`
        # column, so echoing it again only invites rename drift.
        metadata={
            "agent": spec.get("agent", "yolop"),
            "provider": spec.get("provider") or "", "model": spec.get("model") or "",
            "reasoning_effort": m.reasoning_effort, "stop_reason": m.stop_reason,
            "resolved": resolved, "repo": inst.repo,
            "difficulty": (inst.extra or {}).get("difficulty"),
            "tools_used": m.tools_used, "eval_report": report or {},
        },
        error=run.error,
        # Infra errors (Docker/harness, checkout) are surfaced as N/A by the SDK's
        # scorer short-circuit and retried — not counted as the model getting it
        # wrong. `error_kind` left None on a clean run (omitted on the wire).
        error_kind=error_kind,
    )


def _resolved_scorer() -> mira.Scorer:
    """The benchmark gate: did the hidden FAIL_TO_PASS suite pass? Reads the
    verdict the subject already computed (and stashed in transcript metadata) so
    a score-later pass needs no Docker."""
    def fn(_sample, transcript) -> mira.Score:
        resolved = bool(transcript.metadata.get("resolved"))
        return mira.make_score("resolved", 1.0 if resolved else 0.0, resolved,
                               "FAIL_TO_PASS passed" if resolved else "not resolved")
    return mira.scorer("resolved", fn)


class Study:
    """SWE-bench study state: the dataset cache plus the Docker-scoring config.
    `protocol()` wires it into a `mira.Study` (the SDK protocol layer)."""

    def __init__(self, *, do_eval: bool = True, max_workers: int = 4,
                 namespace: str = "swebench", eval_timeout: int = 1800):
        self.do_eval = do_eval
        self.max_workers = max_workers
        self.namespace = namespace
        self.eval_timeout = eval_timeout
        self._instances: dict[str, Instance] | None = None

    def instances(self) -> dict[str, Instance]:
        if self._instances is None:
            self._instances = {i.instance_id: i for i in load_instances()}
        return self._instances

    def _subject(self, sample: mira.Sample, cx: mira.RunCx) -> mira.Transcript:
        """Run one (instance, config) case. The SDK has already skipped
        unavailable targets, so any target reaching here is runnable."""
        spec = MATRIX.get(cx.target)
        if spec is None:
            raise ValueError(f"unknown target/config: {cx.target!r}")
        inst = self.instances().get(sample.id)
        if inst is None:
            raise ValueError(f"unknown sample/instance: {sample.id!r}")
        run, resolved, report, error_kind = _run_agent_case(
            inst, cx.target, spec, max_workers=self.max_workers, namespace=self.namespace,
            eval_timeout=self.eval_timeout, do_eval=self.do_eval)
        return _build_transcript(run, resolved, report, spec, inst, error_kind)

    def _build_samples(self) -> list[mira.Sample]:
        membership = suite_membership()
        samples: list[mira.Sample] = []
        for iid, inst in self.instances().items():
            tags = [inst.repo] if inst.repo else []
            difficulty = (inst.extra or {}).get("difficulty")
            if difficulty:
                tags.append(_sanitize(str(difficulty)))
            tags.extend(membership.get(iid, []))
            # The subject pulls the full problem statement from the Instance, so
            # the Sample carries only the routing metadata (the SDK doesn't put a
            # prompt on the wire `list` anyway).
            samples.append(mira.Sample(
                iid, tags=tags,
                metadata={"repo": inst.repo or "", "difficulty": str(difficulty or "")}))
        return samples

    def _targets(self) -> list[mira.Target]:
        # Each MATRIX entry is a Mira *target* (the thing under evaluation — here a
        # harness/agent config, not just a model). label = config name; an absent
        # provider key marks it unavailable so the SDK reports it N/A and skips it.
        return [mira.target(name, provider=spec.get("provider") or spec.get("agent", "yolop"),
                            available=config_available(spec), metadata=_target_metadata(spec))
                for name, spec in MATRIX.items()]

    def protocol(self) -> mira.Study:
        study = mira.Study("swebench_verified", version=STUDY_VERSION)
        # Samples are lazy (_LazySamples): the dataset loads on the first `list`/
        # `run`, not at construction, so `initialize` stays cheap and works offline
        # (the SDK's `initialize` only counts evals, never enumerates samples).
        study.add(mira.Eval(
            name="swebench_verified", subject=self._subject,
            samples=_LazySamples(self), targets=self._targets(),
            scorers=[mira.succeeded(), _resolved_scorer()],
            description="Resolve SWE-bench instances; FAIL_TO_PASS via the official Docker harness",
            metadata={"benchmark": "swebench_verified"}))
        return study


class _LazySamples(abc.Sequence):
    """A Sequence of the study's samples that materializes (loading the dataset)
    only on first access, then caches. Lets `protocol()` build the Eval — and the
    host answer `initialize` — without touching the dataset."""

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
    def _int(name: str, default: int) -> int:
        try:
            return int(os.environ[name])
        except (KeyError, ValueError):
            return default
    return Study(
        do_eval=os.environ.get("SWEBENCH_NO_EVAL", "") not in ("1", "true", "yes"),
        max_workers=_int("SWEBENCH_MAX_WORKERS", 4),
        namespace=os.environ.get("SWEBENCH_NAMESPACE", "swebench"),
        eval_timeout=_int("SWEBENCH_EVAL_TIMEOUT", 1800),
    )


def serve(study: Study | None = None) -> None:
    """Build the study and run the SDK's stdio protocol loop until stdin closes."""
    study = study or _build_study()
    log(f"ready: study=swebench_verified do_eval={study.do_eval}")
    study.protocol().serve()


def main() -> int:
    serve()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

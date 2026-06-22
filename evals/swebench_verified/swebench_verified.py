# /// script
# requires-python = ">=3.11"
# dependencies = ["pandas>=2.0", "pyarrow>=14.0", "swebench>=4.1.0"]
# ///
"""swebench_verified — a single-file Mira eval study for SWE-bench Verified.

Mira's host CLI speaks newline-delimited JSON over stdio to a child process, so
an eval study can be any program in any language. This is that program for
yolop's SWE-bench Verified benchmark — one self-contained file with its
dependencies declared inline (PEP 723), so `uv` builds an ephemeral env and runs
it with no project scaffolding:

    cd evals/swebench_verified
    mira --cmd "uv run swebench_verified.py" list
    mira --cmd "uv run swebench_verified.py" run astropy__astropy-12907 --models anthropic-sonnet --save

The `mira` host owns the model matrix, selection, concurrency, checkpoints, and
reporting; this study owns the SWE-bench-specific work:

* loads SWE-bench Verified instances (the HF parquet),
* runs a coding agent (yolop, claude-code, codex, or pi) in a fresh checkout,
* mines the agent's event log into common metrics, and
* scores the patch with the official `swebench` Docker `FAIL_TO_PASS` harness.

Mapping to Mira: one **eval** (`swebench_verified`); each instance is a
**sample** (tagged with repo, difficulty, and any curated suite); each entry in
`MATRIX` below is a **model** (label = config name), advertised `available:
false` when its provider key is absent so the host skips it.

Study-internal knobs come from the environment (the host can't pass study CLI
flags): `SWEBENCH_NO_EVAL=1` skips Docker scoring, `SWEBENCH_MAX_WORKERS`,
`SWEBENCH_NAMESPACE` (`none` builds images locally), `SWEBENCH_EVAL_TIMEOUT`,
`SWEBENCH_YOLOP_BIN` (override the yolop binary path).

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
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator, Mapping, Sequence

# Mira collapsed its pre-release minors back to a single 1.0 baseline; everything
# we emit (open-ended `metadata`, the numeric `metrics` map, `error_kind`, and
# per-model `metadata` columns) is part of that baseline.
PROTOCOL_VERSION = "1.0"
STUDY_VERSION = "0.4.0"

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
# The config matrix (one entry = one Mira model). Inlined here so the study is
# a single file. `agent:` picks the adapter; remaining keys go to it. yolop
# configs run the binary at REPO_ROOT/target/release/yolop (override with
# SWEBENCH_YOLOP_BIN).
# ============================================================================ #
DEFAULTS: dict[str, Any] = {"timeout": 1800, "max_cost_usd": 5.0}

MATRIX: dict[str, dict[str, Any]] = {
    # yolop x OpenAI (default provider)
    "openai-default": {"agent": "yolop", "provider": "openai", "model": "gpt-5.5"},
    "openai-high": {"agent": "yolop", "provider": "openai", "model": "gpt-5.5",
                    "reasoning_effort": "high"},
    # yolop x Anthropic (secondary provider)
    "anthropic-sonnet": {"agent": "yolop", "provider": "anthropic", "model": "claude-sonnet-4-5"},
    "anthropic-opus": {"agent": "yolop", "provider": "anthropic", "model": "claude-opus-4-8"},
    # yolop x OpenRouter (needs OPENROUTER_API_KEY)
    "openrouter-nemotron": {"agent": "yolop", "provider": "openrouter",
                            "model": "nvidia/nemotron-3-ultra-550b-a55b"},
    "openrouter-glm": {"agent": "yolop", "provider": "openrouter", "model": "z-ai/glm-5.2"},
    "openrouter-qwen-coder": {"agent": "yolop", "provider": "openrouter", "model": "qwen/qwen3-coder"},
    # Offline plumbing check (no key; won't solve tasks)
    "llmsim": {"agent": "yolop", "provider": "llmsim"},
    # Other coding agents (need their CLI on PATH + provider keys)
    "claude-code": {"agent": "claude-code", "model": "claude-sonnet-4-5"},
    "claude-code-opus": {"agent": "claude-code", "model": "claude-opus-4-8"},
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


def _model_metadata(spec: dict) -> dict[str, Any]:
    """Descriptive config that rides the model column (open JSON). Lets the host
    surface it per column and group results with `mira run --group-by agent`."""
    cfg = {**DEFAULTS, **spec}
    md: dict[str, Any] = {"agent": cfg.get("agent", "yolop")}
    for k in ("model", "provider", "reasoning_effort", "sandbox", "max_cost_usd", "price"):
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
        elif etype == "exec_command_end":  # older schema
            m.tool_calls += 1
            m.tools_used["command"] = m.tools_used.get("command", 0) + 1
            if msg.get("exit_code") not in (0, None):
                m.tool_calls_failed += 1
        elif etype == "agent_message":  # older schema
            m.assistant_messages += 1
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
    m.iterations = m.turns or m.assistant_messages
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


def checkout(instance: Instance, dest: Path) -> None:
    """SHA-targeted shallow fetch of instance.repo at base_commit into dest."""
    if not instance.repo or not instance.base_commit:
        raise RuntimeError(f"{instance.instance_id}: missing repo/base_commit")
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True, exist_ok=True)
    url = f"https://github.com/{instance.repo}.git"
    for step in (["init", "-q"], ["remote", "add", "origin", url]):
        r = _git(step, dest)
        if r.returncode != 0:
            raise RuntimeError(f"git {step[0]} failed for {instance.instance_id}: {r.stderr.strip()}")
    fetch = _git(["fetch", "-q", "--depth", "1", "origin", instance.base_commit], dest)
    if fetch.returncode != 0:
        raise RuntimeError(f"fetch failed for {instance.instance_id}: {fetch.stderr.strip()}")
    co = _git(["checkout", "-q", "FETCH_HEAD"], dest)
    if co.returncode != 0:
        raise RuntimeError(f"checkout failed for {instance.instance_id}: {co.stderr.strip()}")


def capture_patch(workdir: Path) -> str:
    _git(["add", "-A"], workdir)
    return _git(["diff", "--cached", "--no-color"], workdir).stdout


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
    cmd = [sys.executable, "-m", "swebench.harness.run_evaluation",
           "--dataset_name", str(dataset_file), "--predictions_path", str(preds_file),
           "--run_id", run_id, "--instance_ids", *ids, "--max_workers", str(max_workers),
           "--namespace", namespace, "--timeout", str(timeout), "--report_dir", str(work)]
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
# The study — answers Mira initialize/list/run over stdio. One process serves
# many run requests, so the dataset is loaded once and reused.
# ============================================================================ #
def _sanitize(s: str) -> str:
    return "".join(c if c.isalnum() or c in "-._" else "_" for c in s)


class Study:
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

    # -- protocol handlers -------------------------------------------------
    def initialize(self) -> dict[str, Any]:
        return {"protocol_version": PROTOCOL_VERSION, "study": "swebench_verified",
                "study_version": STUDY_VERSION, "evals": 1, "capabilities": ["usage"]}

    def list(self) -> dict[str, Any]:
        membership = suite_membership()
        samples = []
        for iid, inst in self.instances().items():
            tags = [inst.repo] if inst.repo else []
            difficulty = (inst.extra or {}).get("difficulty")
            if difficulty:
                tags.append(_sanitize(str(difficulty)))
            tags.extend(membership.get(iid, []))
            samples.append({"id": iid, "tags": tags,
                            "metadata": {"repo": inst.repo or "", "difficulty": str(difficulty or "")}})
        models = [{"label": name, "provider": spec.get("provider") or spec.get("agent", "yolop"),
                   "available": config_available(spec), "metadata": _model_metadata(spec)}
                  for name, spec in MATRIX.items()]
        return {"evals": [{
            "name": "swebench_verified",
            "description": "Resolve SWE-bench instances; FAIL_TO_PASS via the official Docker harness",
            "samples": samples, "scorers": ["succeeded", "resolved"], "models": models,
            "max_turns": 0, "metadata": {"benchmark": "swebench_verified"},
        }]}

    def run(self, params: dict[str, Any]) -> dict[str, Any]:
        eval_name, sample_id, model = params.get("eval"), params.get("sample"), params.get("model")
        if eval_name != "swebench_verified":
            raise ValueError(f"unknown eval: {eval_name!r}")
        spec = MATRIX.get(model)
        if spec is None:
            raise ValueError(f"unknown model/config: {model!r}")
        inst = self.instances().get(sample_id)
        if inst is None:
            raise ValueError(f"unknown sample/instance: {sample_id!r}")
        if not config_available(spec):
            return self._result(eval_name, sample_id, model, skipped=True)

        run = self._run_agent(inst, model, spec)
        # A checkout/setup failure is infra (not the model's fault).
        error_kind = "infra" if (run.error or "").startswith("runner:") else None
        resolved, report, eval_err = self._score(inst, run, model)
        if eval_err and not run.error:
            run.error = eval_err  # Docker/harness failed to score
            error_kind = "infra"

        scores = [
            {"scorer": "succeeded", "value": 0.0 if run.error else 1.0,
             "pass": run.error is None, "reason": run.error or "agent run completed"},
            {"scorer": "resolved", "value": 1.0 if resolved else 0.0, "pass": resolved,
             "reason": "FAIL_TO_PASS passed" if resolved else "not resolved"},
        ]
        aggregate = sum(s["value"] for s in scores) / len(scores)
        # `resolved` is the benchmark gate; `succeeded` is reported for signal.
        # On an infra error the host overrides these to a single N/A.
        return self._result(eval_name, sample_id, model, passed=resolved, aggregate=aggregate,
                            scores=scores,
                            transcript=self._transcript(run, resolved, report, model, spec, inst, error_kind))

    # -- helpers -----------------------------------------------------------
    def _run_agent(self, inst: Instance, model: str, spec: dict) -> AgentRun:
        run_id = f"{_sanitize(model)}-{_sanitize(inst.instance_id)}-{uuid.uuid4().hex[:6]}"
        workdir = CACHE_DIR / "work" / run_id
        session_dir = CACHE_DIR / "sessions" / run_id
        session_dir.mkdir(parents=True, exist_ok=True)
        try:
            checkout(inst, workdir)
            run = run_agent(spec, inst, str(workdir), str(session_dir))
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
            results = evaluate_predictions({inst.instance_id: run.patch}, [inst], run_id,
                                           max_workers=self.max_workers, namespace=self.namespace,
                                           timeout=self.eval_timeout)
            ev = results.get(inst.instance_id)
            if ev is None:
                return False, {}, "no eval result"
            return bool(ev.resolved), ev.report, ev.error
        except Exception as e:  # Docker/infra failure: not resolved, surface it
            log(f"eval failed for {model}/{inst.instance_id}: {e}")
            return False, {}, f"eval: {e}"

    def _transcript(self, run: AgentRun, resolved: bool, report: dict, model: str,
                    spec: dict, inst: Instance, error_kind: str | None):
        m = run.metrics
        tools = [name for name, count in m.tools_used.items() for _ in range(count)]
        files = {"model_patch.diff": run.patch} if run.patch else {}
        # metadata is open-ended JSON (protocol 1.4): structured values, not
        # stringified. Use `metrics` (1.2) for anything numeric so it surfaces in
        # reports and feeds generic budget scorers.
        metrics = {
            "turns": m.turns, "iterations": m.iterations, "tool_calls": m.tool_calls,
            "tool_calls_failed": m.tool_calls_failed,
            "cache_read_tokens": m.tokens.cache_read_tokens,
            "cache_creation_tokens": m.tokens.cache_creation_tokens,
        }
        if m.agent_reported_time_s is not None:
            metrics["agent_reported_time_s"] = m.agent_reported_time_s
        transcript = {
            "final_response": f"resolved={resolved}", "iterations": m.iterations,
            "tool_calls_count": m.tool_calls, "tool_calls": tools,
            "usage": {"input_tokens": m.tokens.input_tokens, "output_tokens": m.tokens.output_tokens,
                      "cache_read_tokens": m.tokens.cache_read_tokens,
                      "cache_creation_tokens": m.tokens.cache_creation_tokens,
                      "total_tokens": m.tokens.total_tokens, "cost_usd": m.cost_usd},
            "timing": {"duration_ms": int(m.wall_time_s * 1000)},
            "metrics": metrics, "files": files,
            "metadata": {
                "config": model, "agent": spec.get("agent", "yolop"),
                "provider": spec.get("provider") or "", "model": spec.get("model") or "",
                "reasoning_effort": m.reasoning_effort, "stop_reason": m.stop_reason,
                "resolved": resolved, "repo": inst.repo,
                "difficulty": (inst.extra or {}).get("difficulty"),
                "tools_used": m.tools_used, "eval_report": report or {},
            },
            "error": run.error,
        }
        # Infra errors (Docker/harness, checkout) are surfaced as N/A by the host
        # and retried — not counted as the model getting it wrong.
        if error_kind:
            transcript["error_kind"] = error_kind
        return transcript

    def _result(self, eval_name, sample_id, model, *, passed=False, aggregate=0.0,
                scores=None, transcript=None, skipped=False) -> dict[str, Any]:
        return {"eval": eval_name, "sample": sample_id, "model": model, "passed": passed,
                "aggregate": aggregate, "scores": scores or [],
                "transcript": transcript or {"final_response": "", "error": None}, "skipped": skipped}


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
    """Run the stdio protocol loop until stdin closes."""
    study = study or _build_study()
    log(f"ready: study=swebench_verified do_eval={study.do_eval}")
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


def main() -> int:
    serve()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

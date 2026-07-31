"""harbor_yolop — a Harbor agent adapter that runs yolop inside a task container.

[Harbor](https://harborframework.com) is the execution framework behind
Terminal-Bench 2.x: it builds the task's container, runs an agent inside it,
then runs the task's own verifier. Agents plug in as `BaseInstalledAgent`
subclasses; Harbor selects one with `--agent <module>:<Class>`, so this file is
loaded by import path rather than being registered in Harbor's own agent map:

    PYTHONPATH=evals/terminal_bench harbor run \\
        --agent harbor_yolop:Yolop --model openai/gpt-5.6-terra \\
        -p <task-dir>

yolop is a single static binary, not an npm/pip package, so `install()` uploads
a host-built binary into the container instead of fetching a release. The image
ABI is not the host's (Terminal-Bench images are mostly Debian bookworm,
glibc 2.36, against a newer host), so point `YOLOP_BIN` at a static musl build:
`cargo build --release --target x86_64-unknown-linux-musl`.

Two yolop features do the heavy lifting here:

* `--trajectory-out` writes the run as an ATIF v1.7 document, the format Harbor
  itself consumes, so no bespoke event-log converter is needed — the adapter
  points it at `/logs/agent/trajectory.json` and declares `SUPPORTS_ATIF`.
* `--session-dir` writes an incrementally-flushed `events.jsonl`. `/logs/agent`
  is bind-mounted from the host trial dir, so the host can watch the run's cost
  as it accrues and stop it at `max_cost_usd` — yolop has no native dollar cap.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shlex
import uuid
from pathlib import Path
from typing import Any, override

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    CliFlag,
    with_prompt_template,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.utils.env import parse_bool_env_value

# Everything the adapter owns inside the container lives here; `/logs/agent` is
# reserved for artifacts the harness collects (trajectory, sessions).
_INSTALL_DIR = "/installed-agent/yolop"
_BIN_PATH = f"{_INSTALL_DIR}/yolop"
_CONFIG_HOME = f"{_INSTALL_DIR}/config"
# Deliberately outside the yolop dir: the container outlives the agent phase, so
# the verifier can trust the same bundle. Exported for callers that build the
# verifier's environment (see `TB_VERIFIER_ENV` in the study).
CONTAINER_CA_PATH = "/installed-agent/ca-bundle.crt"
_AGENT_LOGS = "/logs/agent"
_SESSION_DIR = f"{_AGENT_LOGS}/sessions"
_TRAJECTORY_PATH = f"{_AGENT_LOGS}/trajectory.json"
_STDOUT_FILENAME = "yolop.txt"

# Terminal-Bench instructions are written as if to a colleague ("please help me
# find them and merge them into master"), but there is nobody in the container to
# answer a follow-up, and grading looks only at the filesystem after the agent
# exits. Without this framing yolop's headless run can legitimately stop to ask
# for confirmation before a history-rewriting change and score zero with the work
# undone. Overridable per config with `--ak prompt_template_path=<path>`.
DEFAULT_PROMPT_TEMPLATE = Path(__file__).resolve().parent / "prompt_autonomous.md"

# Provider -> the host env var whose value the in-container agent needs.
_PROVIDER_KEYS = {
    "anthropic": ["ANTHROPIC_API_KEY"],
    "codex": ["OPENAI_API_KEY"],
    "google": ["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    "openai": ["OPENAI_API_KEY"],
    "openrouter": ["OPENROUTER_API_KEY"],
    "custom": ["CUSTOM_API_KEY", "CUSTOM_BASE_URL"],
}


def session_id() -> str:
    """A fresh yolop session id. yolop validates the shape (`session_` + 32 hex)."""
    return f"session_{uuid.uuid4().hex}"


def running_cost(session_log: Path) -> float | None:
    """Cost accrued so far, read from a partially-written yolop `events.jsonl`.

    Returns None while the log has no completed message yet, so a missing or
    empty log reads as "unknown", never as "$0 spent". Actual cost is preferred
    over the estimate, matching yolop's own accounting.
    """
    if not session_log.exists():
        return None
    total = 0.0
    seen = False
    with session_log.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue  # a half-written trailing line; the writer is still going
            if event.get("type") != "output.message.completed":
                continue
            usage = (event.get("data") or {}).get("usage") or {}
            actual = usage.get("actual_cost_usd")
            total += float(
                actual if actual is not None else (usage.get("estimated_cost_usd") or 0.0)
            )
            seen = True
    return total if seen else None


def _kill_command(signal: str) -> str:
    """Signal the in-container yolop process, without depending on `pkill`.

    Task images are minimal and many carry no `procps`, so the process table is
    walked through `/proc` with tools every image has.
    """
    return (
        'for proc in /proc/[0-9]*; do '
        f'  if tr "\\0" " " < "$proc/cmdline" 2>/dev/null | grep -q {shlex.quote(_BIN_PATH)}; then '
        f'    kill -{signal} "${{proc#/proc/}}" 2>/dev/null || true; '
        "  fi; "
        "done; true"
    )


class Yolop(BaseInstalledAgent):
    """yolop, run headlessly (`-p`) inside the task container."""

    SUPPORTS_ATIF: bool = True

    CLI_FLAGS = [
        CliFlag(
            "reasoning_effort",
            cli="--reasoning-effort",
            type="str",
            env_fallback="YOLOP_REASONING_EFFORT",
        ),
    ]

    def __init__(
        self,
        logs_dir: Path,
        binary_path: str | None = None,
        ca_cert_path: str | None = None,
        max_cost_usd: float | None = None,
        sandbox: bool = False,
        poll_s: float = 2.0,
        *args,
        **kwargs,
    ):
        # Resolved on the host: the binary and CA are uploaded, not fetched.
        self._binary_path = Path(
            binary_path or os.environ.get("YOLOP_BIN") or "target/release/yolop"
        ).expanduser()
        ca = ca_cert_path or os.environ.get("YOLOP_CA_CERT")
        self._ca_cert_path = Path(ca).expanduser() if ca else None
        # Harbor delivers `--ak key=value` kwargs as strings, so coerce rather
        # than trust the annotation ("false" is a truthy str).
        self._max_cost_usd = float(max_cost_usd) if max_cost_usd is not None else None
        self._sandbox = parse_bool_env_value(sandbox, name="sandbox")
        self._poll_s = float(poll_s)
        self._session_id = session_id()
        self._stop_reason = "completed"
        kwargs.setdefault("prompt_template_path", DEFAULT_PROMPT_TEMPLATE)
        super().__init__(logs_dir, *args, **kwargs)

    @staticmethod
    @override
    def name() -> str:
        return "yolop"

    @override
    def get_version_command(self) -> str | None:
        return f"{_BIN_PATH} --version"

    @override
    def parse_version(self, stdout: str) -> str:
        # `yolop --version` prints "yolop 0.13.0 (commit …, everruns-runtime …)";
        # the trailing build detail is not the version.
        match = re.search(r"\d+\.\d+\.\d+", stdout)
        return match.group(0) if match else stdout.strip()

    # -- install ------------------------------------------------------------ #
    @override
    async def install(self, environment: BaseEnvironment) -> None:
        if not self._binary_path.exists():
            raise RuntimeError(
                f"yolop binary not found at {self._binary_path}. Build it "
                "(`cargo build --release --target x86_64-unknown-linux-musl`) and pass "
                "`--ak binary_path=<path>` or set YOLOP_BIN. The binary must match the "
                "task image's ABI, so prefer the static musl build."
            )
        await self.exec_as_root(
            environment, command=f"mkdir -p {_INSTALL_DIR} {_CONFIG_HOME}/yolop {_SESSION_DIR}"
        )
        await environment.upload_file(self._binary_path, _BIN_PATH)
        # World-readable/executable: install runs as root, the agent as the task user.
        await self.exec_as_root(environment, command=f"chmod 755 {_BIN_PATH}")

        # yolop defaults to worktree mode. Terminal-Bench containers are not git
        # repos and the verifier reads the live filesystem, so edits must land in
        # place rather than on a linked branch.
        settings = "worktrees = \"off\"\n"
        await self.exec_as_root(
            environment,
            command=(
                f"printf %s {shlex.quote(settings)} > {_CONFIG_HOME}/yolop/settings.toml && "
                f"chmod -R 777 {_CONFIG_HOME} {_SESSION_DIR}"
            ),
        )

        if self._ca_cert_path is not None:
            if not self._ca_cert_path.exists():
                raise RuntimeError(f"ca_cert_path does not exist: {self._ca_cert_path}")
            await environment.upload_file(self._ca_cert_path, CONTAINER_CA_PATH)
            await self.exec_as_root(environment, command=f"chmod 644 {CONTAINER_CA_PATH}")

    # -- run ---------------------------------------------------------------- #
    def _provider_and_model(self) -> tuple[str | None, str | None]:
        """Split Harbor's `provider/model` convention into yolop's two flags.

        A bare model name (no slash) leaves the provider unset, so yolop
        auto-detects it from the environment.
        """
        if not self.model_name:
            return None, None
        if "/" in self.model_name:
            provider, model = self.model_name.split("/", 1)
            return provider, model
        return None, self.model_name

    def _run_env(self) -> dict[str, str]:
        provider, _ = self._provider_and_model()
        env: dict[str, str] = {"XDG_CONFIG_HOME": _CONFIG_HOME}
        for key in _PROVIDER_KEYS.get(provider or "", []):
            value = self._get_env(key)
            if value:
                env[key] = value
        if self._ca_cert_path is not None:
            # yolop's HTTP stack reads SSL_CERT_FILE; the others cover any tool
            # the agent shells out to from inside the container.
            env.update(
                {
                    "SSL_CERT_FILE": CONTAINER_CA_PATH,
                    "REQUESTS_CA_BUNDLE": CONTAINER_CA_PATH,
                    "CURL_CA_BUNDLE": CONTAINER_CA_PATH,
                    "NODE_EXTRA_CA_CERTS": CONTAINER_CA_PATH,
                }
            )
        return env

    def _command(self, instruction: str) -> str:
        provider, model = self._provider_and_model()
        parts = [_BIN_PATH]
        if provider:
            parts += ["--provider", provider]
        if model:
            parts += ["--model", model]
        if self._sandbox:
            parts.append("--sandbox")
        flags = self.build_cli_flags()
        if flags:
            parts.append(flags)
        parts += [
            "--session",
            self._session_id,
            "--session-dir",
            _SESSION_DIR,
            "--trajectory-out",
            _TRAJECTORY_PATH,
            "-p",
            shlex.quote(instruction),
        ]
        # stdbuf keeps the tee'd copy current while the run is in flight.
        return (
            " ".join(parts)
            + f" 2>&1 </dev/null | stdbuf -oL tee {_AGENT_LOGS}/{_STDOUT_FILENAME}"
        )

    @property
    def _host_session_log(self) -> Path:
        """`events.jsonl` as seen from the host (`/logs/agent` is bind-mounted)."""
        return self.logs_dir / "sessions" / self._session_id / "events.jsonl"

    async def _watch_budget(self, environment: BaseEnvironment) -> bool:
        """Poll the in-flight cost and stop yolop once it passes the cap.

        Returns True if the run was stopped. Cancelled by `run()` as soon as the
        agent exec finishes, so a run that stays under the cap pays nothing but
        a few file reads.
        """
        assert self._max_cost_usd is not None
        while True:
            await asyncio.sleep(self._poll_s)
            try:
                cost = running_cost(self._host_session_log)
            except OSError:
                cost = None  # log not readable yet (mount lag); try again
            if cost is None or cost <= self._max_cost_usd:
                continue
            self.logger.warning(
                f"yolop hit the cost cap (${cost:.2f} > ${self._max_cost_usd:.2f}); "
                "stopping the run"
            )
            self._stop_reason = "budget"
            # SIGINT first so yolop flushes its trajectory; SIGKILL is the backstop.
            await environment.exec(command=_kill_command("INT"), user="root")
            await asyncio.sleep(5)
            await environment.exec(command=_kill_command("KILL"), user="root")
            return True

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        command = self._command(instruction)
        env = self._run_env()

        agent_task = asyncio.ensure_future(
            self.exec_as_agent(environment, command=command, env=env)
        )
        watcher = (
            asyncio.ensure_future(self._watch_budget(environment))
            if self._max_cost_usd is not None
            else None
        )
        try:
            await agent_task
        except Exception:
            # A run we stopped on budget exits non-zero by construction; that is a
            # recorded outcome, not a harness failure, and the verifier still runs
            # against whatever the agent had already written to disk.
            if self._stop_reason != "budget":
                raise
        finally:
            if watcher is not None:
                watcher.cancel()
                # Retrieve the cancellation (and any watcher failure) so the loop
                # does not log an unretrieved-exception warning after the trial.
                await asyncio.gather(watcher, return_exceptions=True)

    # -- metrics ------------------------------------------------------------ #
    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        trajectory_path = self.logs_dir / "trajectory.json"
        if not trajectory_path.exists():
            context.metadata = {"stop_reason": self._stop_reason}
            return
        try:
            trajectory = json.loads(trajectory_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            context.metadata = {"stop_reason": self._stop_reason}
            return

        totals = trajectory.get("final_metrics") or {}
        # ATIF `total_prompt_tokens` is the whole prompt including cache reads,
        # which is exactly AgentContext's "input tokens including cache".
        context.n_input_tokens = totals.get("total_prompt_tokens")
        context.n_cache_tokens = totals.get("total_cached_tokens")
        context.n_output_tokens = totals.get("total_completion_tokens")
        context.cost_usd = totals.get("total_cost_usd")
        context.metadata = {"stop_reason": self._stop_reason, **summarize(trajectory)}


def summarize(trajectory: dict[str, Any]) -> dict[str, Any]:
    """Loop-shape metrics ATIF records per step but does not total up."""
    steps = trajectory.get("steps") or []
    tools: dict[str, int] = {}
    agent_steps = llm_calls = tool_calls = 0
    for step in steps:
        if step.get("source") != "agent":
            continue
        agent_steps += 1
        llm_calls += int(step.get("llm_call_count") or 1)
        for call in step.get("tool_calls") or []:
            tool_calls += 1
            name = call.get("function_name") or "unknown"
            tools[name] = tools.get(name, 0) + 1
    return {
        "agent_steps": agent_steps,
        "llm_calls": llm_calls,
        "tool_calls": tool_calls,
        "tools_used": tools,
        "yolop_version": (trajectory.get("agent") or {}).get("version"),
    }

"""Unit tests for the Harbor yolop adapter (no Docker, no network, no API key)."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
import unittest.mock as mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import harbor_yolop as hy  # noqa: E402


def make_agent(tmp: Path, **kwargs) -> hy.Yolop:
    kwargs.setdefault("model_name", "openai/gpt-5.6-terra")
    kwargs.setdefault("binary_path", str(tmp / "yolop"))
    return hy.Yolop(logs_dir=tmp, **kwargs)


class TestSessionId(unittest.TestCase):
    def test_shape_matches_what_yolop_accepts(self):
        # yolop rejects any other shape: `session_` + 32 hex chars.
        self.assertRegex(hy.session_id(), r"^session_[0-9a-f]{32}$")


class TestCommand(unittest.TestCase):
    def test_provider_and_model_are_split_on_the_slash(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp))
            command = agent._command("fix the repo")
        self.assertIn("--provider openai", command)
        self.assertIn("--model gpt-5.6-terra", command)
        self.assertIn(f"--trajectory-out {hy._TRAJECTORY_PATH}", command)
        self.assertIn("-p 'fix the repo'", command)

    def test_bare_model_leaves_the_provider_to_autodetection(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp), model_name="gpt-5.6-terra")
            command = agent._command("go")
        self.assertNotIn("--provider", command)
        self.assertIn("--model gpt-5.6-terra", command)

    def test_instruction_is_shell_quoted(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp))
            command = agent._command("rm -rf /; echo $(whoami)")
        # The whole instruction must land inside one quoted argument.
        self.assertNotIn("; echo $(whoami)'", command.split("-p ", 1)[1].split("' ")[0])
        self.assertIn("'rm -rf /; echo $(whoami)'", command)

    def test_reasoning_effort_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp), reasoning_effort="high")
            self.assertIn("--reasoning-effort high", agent._command("go"))

    def test_sandbox_is_off_unless_asked_for(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertNotIn("--sandbox", make_agent(Path(tmp))._command("go"))
            self.assertIn("--sandbox", make_agent(Path(tmp), sandbox=True)._command("go"))

    def test_string_kwargs_from_harbor_are_coerced(self):
        # `--ak sandbox=false` arrives as the string "false", which is truthy.
        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp), sandbox="false", max_cost_usd="2.5")
        self.assertNotIn("--sandbox", agent._command("go"))
        self.assertEqual(agent._max_cost_usd, 2.5)


class TestPromptTemplate(unittest.TestCase):
    def test_autonomy_framing_is_applied_by_default(self):
        # Terminal-Bench instructions read like a request to a colleague, but
        # nothing in the container can answer a follow-up question.
        with tempfile.TemporaryDirectory() as tmp:
            rendered = make_agent(Path(tmp)).render_instruction("merge my changes")
        self.assertIn("merge my changes", rendered)
        self.assertIn("no user", rendered)
        self.assertNotEqual(rendered, "merge my changes")

    def test_an_explicit_template_wins(self):
        with tempfile.TemporaryDirectory() as tmp:
            template = Path(tmp) / "t.md"
            template.write_text("ONLY: {{ instruction }}", encoding="utf-8")
            agent = make_agent(Path(tmp), prompt_template_path=str(template))
            self.assertEqual(agent.render_instruction("go"), "ONLY: go")


class TestKillCommand(unittest.TestCase):
    def test_walks_proc_instead_of_needing_pkill(self):
        # Terminal-Bench images are minimal and many carry no procps.
        command = hy._kill_command("INT")
        self.assertNotIn("pkill", command)
        self.assertIn("/proc/[0-9]*", command)
        self.assertIn("kill -INT", command)
        self.assertIn(hy._BIN_PATH, command)


class TestRunEnv(unittest.TestCase):
    def test_only_the_selected_provider_key_is_forwarded(self):
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.dict("os.environ",
                                {"OPENAI_API_KEY": "sk-o", "ANTHROPIC_API_KEY": "sk-a"},
                                clear=True):
            env = make_agent(Path(tmp))._run_env()
        self.assertEqual(env["OPENAI_API_KEY"], "sk-o")
        self.assertNotIn("ANTHROPIC_API_KEY", env)
        # worktrees=off lives in the uploaded settings under this config home.
        self.assertEqual(env["XDG_CONFIG_HOME"], hy._CONFIG_HOME)

    def test_ca_cert_sets_every_trust_var(self):
        with tempfile.TemporaryDirectory() as tmp:
            ca = Path(tmp) / "ca.crt"
            ca.write_text("x", encoding="utf-8")
            env = make_agent(Path(tmp), ca_cert_path=str(ca))._run_env()
        for var in ("SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE",
                    "NODE_EXTRA_CA_CERTS"):
            self.assertEqual(env[var], hy.CONTAINER_CA_PATH)


class TestRunningCost(unittest.TestCase):
    def _log(self, tmp: Path, events: list[dict]) -> Path:
        path = tmp / "events.jsonl"
        path.write_text("\n".join(json.dumps(e) for e in events) + "\n", encoding="utf-8")
        return path

    def _usage(self, **usage) -> dict:
        return {"type": "output.message.completed", "data": {"usage": usage}}

    def test_missing_log_reads_as_unknown_not_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(hy.running_cost(Path(tmp) / "nope.jsonl"))

    def test_empty_log_reads_as_unknown(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "events.jsonl"
            path.write_text("", encoding="utf-8")
            self.assertIsNone(hy.running_cost(path))

    def test_actual_cost_wins_over_the_estimate(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._log(Path(tmp), [
                self._usage(actual_cost_usd=0.5, estimated_cost_usd=9.0),
                self._usage(estimated_cost_usd=0.25),
            ])
            self.assertAlmostEqual(hy.running_cost(path), 0.75)

    def test_half_written_trailing_line_is_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._log(Path(tmp), [self._usage(actual_cost_usd=1.0)])
            with path.open("a", encoding="utf-8") as fh:
                fh.write('{"type": "output.message.comp')  # writer still going
            self.assertAlmostEqual(hy.running_cost(path), 1.0)


class TestEffectiveModelMetadata(unittest.TestCase):
    def test_latest_completed_response_values_are_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "events.jsonl"
            events = [
                {
                    "type": "output.message.completed",
                    "data": {"message": {"metadata": {
                        "provider": "openai", "model": "gpt-5.6-terra",
                        "reasoning_effort": "medium",
                    }}},
                },
                {
                    "type": "output.message.completed",
                    "data": {"message": {"metadata": {
                        "provider": "openai", "model": "gpt-5.6-terra",
                        "reasoning_effort": "high",
                    }}},
                },
            ]
            path.write_text(
                "\n".join(json.dumps(event) for event in events) + "\n",
                encoding="utf-8",
            )

            self.assertEqual(hy.effective_model_metadata(path), {
                "provider": "openai",
                "model": "gpt-5.6-terra",
                "reasoning_effort": "high",
            })


class TestPostRun(unittest.TestCase):
    TRAJECTORY = {
        "schema_version": "ATIF-v1.7",
        "agent": {"name": "yolop", "version": "0.13.0"},
        "steps": [
            {"step_id": 1, "source": "user", "message": "go"},
            {"step_id": 2, "source": "agent", "message": "looking",
             "llm_call_count": 1,
             "tool_calls": [
                 {"tool_call_id": "c1", "function_name": "run_terminal_cmd",
                  "arguments": {"command": "ls"}},
                 {"tool_call_id": "c2", "function_name": "read_file",
                  "arguments": {"path": "a.txt"}},
             ]},
            {"step_id": 3, "source": "agent", "message": "done", "llm_call_count": 1},
        ],
        "final_metrics": {"total_prompt_tokens": 1000, "total_cached_tokens": 400,
                          "total_completion_tokens": 200, "total_cost_usd": 0.25},
    }

    def test_totals_and_loop_shape_land_on_the_context(self):
        from harbor.models.agent.context import AgentContext

        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "trajectory.json").write_text(json.dumps(self.TRAJECTORY),
                                                       encoding="utf-8")
            agent = make_agent(Path(tmp))
            agent._host_session_log.parent.mkdir(parents=True)
            agent._host_session_log.write_text(json.dumps({
                "type": "output.message.completed",
                "data": {"message": {"metadata": {
                    "provider": "openai", "model": "gpt-5.6-terra",
                    "reasoning_effort": "medium",
                }}},
            }) + "\n", encoding="utf-8")
            context = AgentContext()
            agent.populate_context_post_run(context)

        self.assertEqual(context.n_input_tokens, 1000)
        self.assertEqual(context.n_cache_tokens, 400)
        self.assertEqual(context.n_output_tokens, 200)
        self.assertEqual(context.cost_usd, 0.25)
        self.assertEqual(context.metadata["agent_steps"], 2)
        self.assertEqual(context.metadata["tool_calls"], 2)
        self.assertEqual(context.metadata["tools_used"],
                         {"run_terminal_cmd": 1, "read_file": 1})
        self.assertEqual(context.metadata["stop_reason"], "completed")
        self.assertEqual(context.metadata["provider"], "openai")
        self.assertEqual(context.metadata["model"], "gpt-5.6-terra")
        self.assertEqual(context.metadata["reasoning_effort"], "medium")

    def test_missing_trajectory_still_records_the_stop_reason(self):
        from harbor.models.agent.context import AgentContext

        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp))
            agent._stop_reason = "budget"
            context = AgentContext()
            agent.populate_context_post_run(context)

        self.assertIsNone(context.cost_usd)
        self.assertEqual(context.metadata, {"stop_reason": "budget"})

    def test_yolop_trajectory_validates_as_harbor_atif(self):
        # The adapter declares SUPPORTS_ATIF and hands Harbor yolop's own
        # `--trajectory-out` document unconverted; this is that contract.
        from harbor.models.trajectories import Trajectory

        Trajectory.model_validate(self.TRAJECTORY)


class TestInstall(unittest.TestCase):
    def test_missing_binary_fails_with_a_build_hint(self):
        import asyncio

        with tempfile.TemporaryDirectory() as tmp:
            agent = make_agent(Path(tmp), binary_path=str(Path(tmp) / "absent"))
            with self.assertRaises(RuntimeError) as caught:
                asyncio.run(agent.install(mock.AsyncMock()))
        self.assertIn("musl", str(caught.exception))


class TestVersion(unittest.TestCase):
    def test_version_is_parsed_from_the_yolop_banner(self):
        # The banner carries build detail after the version, not just the version.
        banner = "yolop 0.13.0 (commit 03d034fd609f, everruns-runtime 0.17.17)\n"
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(make_agent(Path(tmp)).parse_version(banner), "0.13.0")


if __name__ == "__main__":
    unittest.main()

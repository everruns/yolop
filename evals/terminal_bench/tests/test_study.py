"""Unit tests for the terminal_bench study (no Docker, no network, no API key).

Run them from the study directory:

    uv run --with mira-eval --with harbor -m unittest discover -s tests
"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import terminal_bench as tb  # noqa: E402


def make_task(name: str = "fix-git", **kwargs) -> tb.Task:
    return tb.Task(name=name, path=Path("/tasks") / name,
                   difficulty=kwargs.pop("difficulty", "easy"),
                   category=kwargs.pop("category", "software-engineering"), **kwargs)


class TestMatrix(unittest.TestCase):
    def test_every_config_declares_an_agent_and_model(self):
        for name, spec in tb.MATRIX.items():
            self.assertIn("agent", spec, name)
            self.assertIn("model", spec, name)

    def test_provider_is_the_model_prefix(self):
        self.assertEqual(tb.provider_of({"model": "openai/gpt-5.6-terra"}), "openai")
        self.assertEqual(tb.provider_of({"model": "gpt-5.6-terra"}), "")

    def test_llmsim_is_available_without_any_key(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            self.assertTrue(tb.config_available(tb.MATRIX["llmsim"]))
            self.assertFalse(tb.config_available(tb.MATRIX["openai-gpt-5.6-terra"]))

    def test_openai_config_available_with_key(self):
        with mock.patch.dict("os.environ", {"OPENAI_API_KEY": "sk-test"}, clear=True):
            self.assertTrue(tb.config_available(tb.MATRIX["openai-gpt-5.6-terra"]))


class TestBuildCommand(unittest.TestCase):
    def _command(self, spec: dict, env: dict | None = None) -> list[str]:
        with mock.patch.dict("os.environ", env or {}, clear=True), \
                mock.patch.object(tb, "harbor_bin", return_value="/bin/harbor"):
            return tb.build_command(make_task(), spec, Path("/jobs/job1"),
                                    jobs_dir=Path("/jobs"))

    def test_yolop_config_uses_the_adapter_import_path(self):
        cmd = self._command(tb.MATRIX["openai-gpt-5.6-terra"])
        self.assertIn(tb.YOLOP_AGENT_IMPORT_PATH, cmd)
        self.assertEqual(cmd[cmd.index("--model") + 1], "openai/gpt-5.6-terra")
        self.assertIn("binary_path=" + tb.YOLOP_BIN, cmd)
        self.assertIn("max_cost_usd=5.0", cmd)

    def test_reasoning_effort_rides_as_an_agent_kwarg(self):
        cmd = self._command(tb.MATRIX["openai-gpt-5.6-terra-high"])
        self.assertIn("reasoning_effort=high", cmd)

    def test_harbor_native_agent_is_passed_through(self):
        cmd = self._command(tb.MATRIX["terminus-2"])
        self.assertEqual(cmd[cmd.index("--agent") + 1], "terminus-2")
        # Adapter-only kwargs must not leak onto a Harbor-native agent.
        self.assertNotIn("--ak", cmd)

    def test_agent_env_pairs_are_forwarded(self):
        cmd = self._command(tb.MATRIX["llmsim"],
                            env={"TB_AGENT_ENV": "HTTPS_PROXY=http://p:1, BAD, X=1"})
        self.assertIn("HTTPS_PROXY=http://p:1", cmd)
        self.assertIn("X=1", cmd)
        self.assertNotIn("BAD", cmd)

    def test_ca_cert_also_reaches_the_verifier(self):
        # The verifier is a separate process from the agent, and many tasks'
        # test.sh fetches over the network — without trust it fails to *score*.
        cmd = self._command(tb.MATRIX["llmsim"], env={"TB_CA_CERT": "/host/ca.crt"})
        self.assertIn("--ve", cmd)
        self.assertIn(f"CURL_CA_BUNDLE={tb.harbor_yolop.CONTAINER_CA_PATH}", cmd)

    def test_explicit_verifier_env_is_not_overridden(self):
        cmd = self._command(tb.MATRIX["llmsim"],
                            env={"TB_CA_CERT": "/host/ca.crt",
                                 "TB_VERIFIER_ENV": "SSL_CERT_FILE=/mine.crt"})
        self.assertIn("SSL_CERT_FILE=/mine.crt", cmd)
        self.assertNotIn(f"SSL_CERT_FILE={tb.harbor_yolop.CONTAINER_CA_PATH}", cmd)

    def test_no_verifier_env_without_the_knobs(self):
        self.assertNotIn("--ve", self._command(tb.MATRIX["llmsim"]))


class TestParseTrial(unittest.TestCase):
    def _write(self, tmp: Path, payload: dict) -> Path:
        path = tmp / "result.json"
        path.write_text(json.dumps(payload), encoding="utf-8")
        return path

    def test_reward_one_resolves(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(Path(tmp), {
                "verifier_result": {"rewards": {"reward": 1.0}},
                "agent_result": {"n_input_tokens": 1000, "n_cache_tokens": 400,
                                 "n_output_tokens": 200, "cost_usd": 0.25,
                                 "metadata": {"stop_reason": "completed", "tool_calls": 7,
                                              "tools_used": {"run_terminal_cmd": 7}}},
            })
            outcome = tb.parse_trial(path)
        self.assertTrue(outcome.resolved)
        self.assertEqual(outcome.reward, 1.0)
        self.assertEqual(outcome.cache_tokens, 400)
        self.assertEqual(outcome.cost_usd, 0.25)
        self.assertEqual(outcome.stop_reason, "completed")

    def test_exception_is_surfaced(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(Path(tmp), {
                "verifier_result": {"rewards": {"reward": 0.0}},
                "exception_info": {"exception_type": "ApiRateLimitError",
                                   "exception_message": "429"},
            })
            outcome = tb.parse_trial(path)
        self.assertFalse(outcome.resolved)
        self.assertEqual(outcome.exception, "ApiRateLimitError")
        self.assertIn("429", outcome.error or "")

    def test_missing_verifier_result_is_not_resolved(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(Path(tmp), {"agent_result": {}})
            outcome = tb.parse_trial(path)
        self.assertFalse(outcome.resolved)
        self.assertEqual(outcome.reward, 0.0)


class TestTranscript(unittest.TestCase):
    def test_input_tokens_exclude_cache(self):
        outcome = tb.TrialOutcome(reward=1.0, resolved=True, input_tokens=1000,
                                  cache_tokens=400, output_tokens=200, cost_usd=0.25)
        transcript = tb.build_transcript(make_task(), tb.MATRIX["openai-gpt-5.6-terra"],
                                         outcome, None)
        # Harbor counts cache reads inside n_input_tokens; Mira keeps them apart.
        self.assertEqual(transcript.usage.input_tokens, 600)
        self.assertEqual(transcript.usage.cache_read_tokens, 400)
        self.assertTrue(transcript.metadata["resolved"])

    def test_infra_error_kind_rides_through(self):
        outcome = tb.TrialOutcome(stop_reason="error", error="runner: boom")
        transcript = tb.build_transcript(make_task(), tb.MATRIX["llmsim"], outcome, "infra")
        self.assertEqual(transcript.error_kind, "infra")
        self.assertFalse(transcript.metadata["resolved"])


class TestBudget(unittest.TestCase):
    def test_case_cap_is_clamped_to_what_the_run_has_left(self):
        study = tb.Study(max_cost_usd=1.0)
        study._tasks = {"fix-git": make_task()}
        captured: dict = {}

        def fake_run_case(task, target, spec, **kwargs):
            captured["spec"] = spec
            return tb.TrialOutcome(reward=1.0, resolved=True, cost_usd=0.75)

        with mock.patch.object(tb, "run_case", fake_run_case):
            transcript = study._subject(mira_sample("fix-git"), run_cx("openai-gpt-5.6-terra"))

        # The config's own cap is $5, but only $1 is budgeted for the whole run.
        self.assertEqual(captured["spec"]["max_cost_usd"], 1.0)
        self.assertTrue(transcript.metadata["resolved"])
        self.assertEqual(study.spent_usd, 0.75)

    def test_exhausted_budget_skips_the_case_as_infra(self):
        study = tb.Study(max_cost_usd=1.0)
        study._tasks = {"fix-git": make_task()}
        study._spent_usd = 1.0

        with mock.patch.object(tb, "run_case", side_effect=AssertionError("must not run")):
            transcript = study._subject(mira_sample("fix-git"), run_cx("openai-gpt-5.6-terra"))

        self.assertEqual(transcript.error_kind, "infra")
        self.assertEqual(transcript.metadata["stop_reason"], "budget")

    def test_no_budget_means_no_clamping(self):
        study = tb.Study(max_cost_usd=None)
        study._tasks = {"fix-git": make_task()}
        captured: dict = {}

        def fake_run_case(task, target, spec, **kwargs):
            captured["spec"] = spec
            return tb.TrialOutcome(cost_usd=9.0)

        with mock.patch.object(tb, "run_case", fake_run_case):
            study._subject(mira_sample("fix-git"), run_cx("openai-gpt-5.6-terra"))

        self.assertNotIn("max_cost_usd", captured["spec"])


class TestLoadTask(unittest.TestCase):
    def test_task_toml_metadata_becomes_sample_tags(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            task_dir = Path(tmp) / "fix-git"
            task_dir.mkdir()
            (task_dir / "task.toml").write_text(
                '[metadata]\ndifficulty = "easy"\ncategory = "system-administration"\n'
                'tags = ["git", "recovery"]\n[agent]\ntimeout_sec = 900.0\n',
                encoding="utf-8",
            )
            task = tb.load_task(task_dir)

        self.assertEqual(task.difficulty, "easy")
        self.assertEqual(task.category, "system-administration")
        self.assertEqual(task.tags, ["git", "recovery"])
        self.assertEqual(task.agent_timeout_sec, 900.0)

        study = tb.Study()
        study._tasks = {task.name: task}
        sample = study._build_samples()[0]
        self.assertEqual(sample.id, "fix-git")
        for tag in ("easy", "system-administration", "git", "recovery"):
            self.assertIn(tag, sample.tags)


def mira_sample(sample_id: str):
    import mira

    return mira.Sample(sample_id)


def run_cx(target: str):
    """A minimal stand-in for the SDK's RunCx — the subject only reads `target`."""

    class _Cx:
        pass

    cx = _Cx()
    cx.target = target
    return cx


if __name__ == "__main__":
    unittest.main()

"""Tests for the Mira eval study protocol layer.

Fully offline: the dataset, agent, and Docker scorer are faked so the test
exercises the protocol mapping (initialize/list/run, sample tagging, model
availability, transcript + score shape, stdout cleanliness) with no network or
Docker. Stdlib-only:

    python3 -m unittest discover -s evals/swebench_verified/tests
"""

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import swebench_verified as sv  # noqa: E402
from swebench_verified import AgentRun, EvalResult, Instance, RunMetrics, Study, TokenUsage  # noqa: E402


_INSTANCE = Instance(
    instance_id="org__repo-1", problem_statement="fix the bug",
    repo="org/repo", base_commit="deadbeef", extra={"difficulty": "15 min - 1 hour"},
)


def _study(instances=None):
    s = Study()
    s._instances = {(_INSTANCE.instance_id): _INSTANCE} if instances is None else instances
    return s


def _fake_agent_run(*_a, **_k):
    m = RunMetrics(
        wall_time_s=1.5, turns=3, iterations=3, tool_calls=2,
        tools_used={"edit_file": 1, "read_file": 1},
        tokens=TokenUsage(input_tokens=100, output_tokens=20),
        cost_usd=0.01, stop_reason="completed",
    )
    return AgentRun(patch="diff --git a b", metrics=m)


class InitializeListTest(unittest.TestCase):
    def test_initialize_announces_study(self):
        info = _study().initialize()
        self.assertEqual(info["protocol_version"], "1.0")
        self.assertEqual(info["study"], "swebench_verified")
        self.assertIn("usage", info["capabilities"])

    def test_list_maps_instances_to_samples(self):
        ev = _study().list()["evals"][0]
        self.assertEqual(ev["name"], "swebench_verified")
        sample = ev["samples"][0]
        self.assertEqual(sample["id"], "org__repo-1")
        self.assertIn("org/repo", sample["tags"])             # repo tag
        self.assertIn("15_min_-_1_hour", sample["tags"])      # sanitized difficulty

    def test_list_models_reflect_availability(self):
        with mock.patch.dict("os.environ", {"OPENAI_API_KEY": "", "ANTHROPIC_API_KEY": ""}):
            models = {m["label"]: m for m in _study().list()["evals"][0]["models"]}
        self.assertTrue(models["llmsim"]["available"])        # offline provider
        self.assertFalse(models["openai-default"]["available"])  # no key -> skipped

    def test_list_models_carry_config_metadata(self):
        models = {m["label"]: m for m in _study().list()["evals"][0]["models"]}
        # config detail rides the model column for `mira run --group-by agent`
        self.assertEqual(models["openai-high"]["metadata"]["agent"], "yolop")
        self.assertEqual(models["openai-high"]["metadata"]["reasoning_effort"], "high")
        self.assertEqual(models["codex"]["metadata"]["agent"], "codex")
        self.assertIn("price", models["codex"]["metadata"])


class RunTest(unittest.TestCase):
    def _run(self, *, resolved=True, do_eval=True, model="llmsim", sample="org__repo-1"):
        study = _study()
        study.do_eval = do_eval
        with mock.patch.object(sv, "checkout", lambda *a, **k: None), \
             mock.patch.object(sv, "capture_patch", lambda *a, **k: "diff --git a b"), \
             mock.patch.object(sv, "run_agent", _fake_agent_run), \
             mock.patch.object(sv, "evaluate_predictions",
                               lambda *a, **k: {sample: EvalResult(resolved, {"resolved": resolved})}):
            return study.run({"eval": "swebench_verified", "sample": sample, "model": model})

    def test_resolved_cell_passes(self):
        res = self._run(resolved=True)
        self.assertTrue(res["passed"])
        self.assertFalse(res["skipped"])
        names = {s["scorer"]: s for s in res["scores"]}
        self.assertTrue(names["resolved"]["pass"])
        self.assertTrue(names["succeeded"]["pass"])
        tr = res["transcript"]
        self.assertEqual(tr["usage"]["input_tokens"], 100)
        self.assertEqual(tr["usage"]["cost_usd"], 0.01)
        self.assertIn("model_patch.diff", tr["files"])
        # numeric metrics map (protocol 1.2): values are plain numbers
        self.assertEqual(tr["metrics"]["tool_calls"], 2)
        self.assertEqual(tr["metrics"]["turns"], 3)
        # open-ended metadata (protocol 1.4): structured, not stringified
        md = tr["metadata"]
        self.assertIs(md["resolved"], True)
        self.assertEqual(md["repo"], "org/repo")
        self.assertEqual(md["tools_used"], {"edit_file": 1, "read_file": 1})
        self.assertEqual(md["eval_report"], {"resolved": True})
        self.assertNotIn("error_kind", tr)  # clean run: no infra error

    def test_infra_eval_failure_is_marked_na(self):
        # When the Docker harness fails to score, the cell is an infra error
        # (error_kind=infra) so the host treats it as N/A, not a model failure.
        study = _study()
        with mock.patch.object(sv, "checkout", lambda *a, **k: None), \
             mock.patch.object(sv, "capture_patch", lambda *a, **k: "d"), \
             mock.patch.object(sv, "run_agent", _fake_agent_run), \
             mock.patch.object(sv, "evaluate_predictions",
                               lambda *a, **k: {"org__repo-1": EvalResult(False, {}, "no report.json")}):
            res = study.run({"eval": "swebench_verified", "sample": "org__repo-1", "model": "llmsim"})
        self.assertEqual(res["transcript"]["error_kind"], "infra")

    def test_unresolved_cell_fails(self):
        res = self._run(resolved=False)
        self.assertFalse(res["passed"])
        self.assertFalse({s["scorer"]: s for s in res["scores"]}["resolved"]["pass"])

    def test_no_eval_skips_scoring(self):
        called = []
        study = _study()
        study.do_eval = False
        with mock.patch.object(sv, "checkout", lambda *a, **k: None), \
             mock.patch.object(sv, "capture_patch", lambda *a, **k: "d"), \
             mock.patch.object(sv, "run_agent", _fake_agent_run), \
             mock.patch.object(sv, "evaluate_predictions", lambda *a, **k: called.append(1) or {}):
            res = study.run({"eval": "swebench_verified", "sample": "org__repo-1", "model": "llmsim"})
        self.assertFalse(res["passed"])   # unscored -> not resolved
        self.assertEqual(called, [])      # Docker scorer not invoked

    def test_unavailable_config_is_skipped(self):
        with mock.patch.dict("os.environ", {"OPENAI_API_KEY": ""}):
            res = _study().run(
                {"eval": "swebench_verified", "sample": "org__repo-1", "model": "openai-default"}
            )
        self.assertTrue(res["skipped"])

    def test_unknown_sample_raises(self):
        with self.assertRaises(ValueError):
            _study().run({"eval": "swebench_verified", "sample": "nope", "model": "llmsim"})

    def test_unknown_model_raises(self):
        with self.assertRaises(ValueError):
            _study().run({"eval": "swebench_verified", "sample": "org__repo-1", "model": "nope"})


class StdoutCleanlinessTest(unittest.TestCase):
    """Under the Mira host the study's stdout is the protocol channel: nothing
    the SWE-bench Docker harness emits may leak onto it."""

    def test_evaluate_routes_harness_output_to_stderr(self):
        import contextlib
        import io
        import subprocess

        sv._RAW.clear()
        sv._RAW["iid"] = {"instance_id": "iid", "repo": "org/repo", "patch": ""}
        inst = Instance(instance_id="iid", problem_statement="x", repo="org/repo", base_commit="c")
        seen = {}

        def fake_run(cmd, cwd=None, stdout=None, stderr=None):
            seen["stdout"], seen["stderr"] = stdout, stderr
            return mock.Mock(returncode=1)

        out = io.StringIO()
        with mock.patch.object(subprocess, "run", fake_run), contextlib.redirect_stdout(out):
            sv.evaluate_predictions({"iid": "diff"}, [inst], "rid-1")

        self.assertEqual(out.getvalue(), "")          # banner not on stdout
        self.assertIs(seen["stdout"], sys.stderr)     # harness stdout -> stderr


if __name__ == "__main__":
    unittest.main()

"""Tests for the Mira eval study protocol layer.

These run fully offline: the dataset, agent, and Docker scorer are faked so the
test exercises the protocol mapping (initialize/list/run, sample tagging, model
availability, transcript + score shape) without a network or Docker.
"""

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from yoloeval import study as study_mod  # noqa: E402
from yoloeval.models import AgentRun, EvalResult, Instance, RunMetrics, TokenUsage  # noqa: E402


class FakeBenchmark:
    name = "swebench_verified"

    def __init__(self, resolved=True):
        self.resolved = resolved
        self.evaluated = []

    def load(self, instance_ids=None):
        return list(_INSTANCES.values())

    def evaluate(self, predictions, instances, run_id):
        self.evaluated.append(dict(predictions))
        return {
            iid: EvalResult(resolved=self.resolved, report={"resolved": self.resolved})
            for iid in predictions
        }


_INSTANCES = {
    "org__repo-1": Instance(
        instance_id="org__repo-1",
        problem_statement="fix the bug",
        repo="org/repo",
        base_commit="deadbeef",
        extra={"difficulty": "15 min - 1 hour"},
    ),
}


def _fake_agent_run(*_a, **_k):
    m = RunMetrics(
        wall_time_s=1.5, turns=3, iterations=3, tool_calls=2,
        tools_used={"edit_file": 1, "read_file": 1},
        tokens=TokenUsage(input_tokens=100, output_tokens=20),
        cost_usd=0.01, stop_reason="completed",
    )
    return AgentRun(patch="diff --git a b", metrics=m)


def _make_study(resolved=True, do_eval=True):
    s = study_mod.Study(do_eval=do_eval)
    s._benchmark = FakeBenchmark(resolved=resolved)
    s._instances = dict(_INSTANCES)
    return s


class InitializeListTest(unittest.TestCase):
    def test_initialize_announces_study(self):
        info = _make_study().initialize()
        self.assertEqual(info["protocol_version"], "1.0")
        self.assertEqual(info["study"], "yoloeval")
        self.assertIn("usage", info["capabilities"])

    def test_list_maps_instances_to_samples(self):
        evals = _make_study().list()["evals"]
        self.assertEqual(len(evals), 1)
        ev = evals[0]
        self.assertEqual(ev["name"], "swebench_verified")
        sample = ev["samples"][0]
        self.assertEqual(sample["id"], "org__repo-1")
        # Tagged with repo + sanitized difficulty for `mira run --tag`.
        self.assertIn("org/repo", sample["tags"])
        self.assertIn("15_min_-_1_hour", sample["tags"])

    def test_list_models_reflect_availability(self):
        with mock.patch.dict("os.environ", {"OPENAI_API_KEY": "", "ANTHROPIC_API_KEY": ""}):
            models = {m["label"]: m for m in _make_study().list()["evals"][0]["models"]}
        # llmsim is the offline provider: always runnable.
        self.assertTrue(models["llmsim"]["available"])
        # A provider config with no key is advertised unavailable (host skips it).
        self.assertFalse(models["openai-default"]["available"])


class RunTest(unittest.TestCase):
    def _run(self, study, model="llmsim", sample="org__repo-1"):
        with mock.patch.object(study_mod, "checkout", lambda *a, **k: None), \
             mock.patch.object(study_mod, "capture_patch", lambda *a, **k: "diff --git a b"), \
             mock.patch("yoloeval.study.build_agent") as build:
            build.return_value = mock.Mock(run=_fake_agent_run)
            return study.run({"eval": "swebench_verified", "sample": sample, "model": model})

    def test_resolved_cell_passes(self):
        res = self._run(_make_study(resolved=True))
        self.assertTrue(res["passed"])
        self.assertFalse(res["skipped"])
        names = {s["scorer"]: s for s in res["scores"]}
        self.assertTrue(names["resolved"]["pass"])
        self.assertTrue(names["succeeded"]["pass"])
        usage = res["transcript"]["usage"]
        self.assertEqual(usage["input_tokens"], 100)
        self.assertEqual(usage["cost_usd"], 0.01)
        # The captured patch is exposed as a workspace file for file scorers.
        self.assertIn("model_patch.diff", res["transcript"]["files"])

    def test_unresolved_cell_fails(self):
        res = self._run(_make_study(resolved=False))
        self.assertFalse(res["passed"])
        names = {s["scorer"]: s for s in res["scores"]}
        self.assertFalse(names["resolved"]["pass"])

    def test_no_eval_skips_docker_scoring(self):
        study = _make_study(resolved=True, do_eval=False)
        res = self._run(study)
        self.assertFalse(res["passed"])  # unscored → not resolved
        self.assertEqual(study._benchmark.evaluated, [])

    def test_unavailable_config_is_skipped(self):
        with mock.patch.dict("os.environ", {"OPENAI_API_KEY": ""}):
            res = _make_study().run(
                {"eval": "swebench_verified", "sample": "org__repo-1", "model": "openai-default"}
            )
        self.assertTrue(res["skipped"])

    def test_unknown_sample_raises(self):
        with self.assertRaises(ValueError):
            _make_study().run({"eval": "swebench_verified", "sample": "nope", "model": "llmsim"})

    def test_unknown_model_raises(self):
        with self.assertRaises(ValueError):
            _make_study().run({"eval": "swebench_verified", "sample": "org__repo-1", "model": "nope"})


if __name__ == "__main__":
    unittest.main()

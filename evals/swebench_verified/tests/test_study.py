"""Tests for the study's SWE-bench-specific logic.

The protocol loop, scoring/verdict math, pagination, and wire serialization
belong to the `mira-eval` SDK and are covered by its own conformance suite — we
don't re-test them here. These tests exercise only what this study owns:

* sample tagging (instance -> repo/difficulty/suite tags),
* the target matrix (availability from env, config metadata),
* transcript building (usage/metrics/metadata/files mapping, infra error_kind),
* the `resolved` scorer and the cell orchestration (checkout + agent + score),
* stdout cleanliness of the Docker scorer.

Plus one thin integration smoke that the study registers with the SDK and a run
flows through it. Needs `mira-eval` installed (the study imports it):

    uv run --with mira-eval -m unittest discover -s evals/swebench_verified/tests
"""

import sys
import unittest
import unittest.mock as mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import mira  # noqa: E402  (SDK types for the unit tests below)
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
        tokens=TokenUsage(input_tokens=100, output_tokens=20,
                          cache_read_tokens=5, cache_creation_tokens=7),
        cost_usd=0.01, reasoning_effort="high", stop_reason="completed",
    )
    return AgentRun(patch="diff --git a b", metrics=m)


class SamplesTest(unittest.TestCase):
    """`_build_samples`: instances -> mira.Sample with our tags/metadata."""

    def test_tags_repo_and_sanitized_difficulty(self):
        sample = _study()._build_samples()[0]
        self.assertEqual(sample.id, "org__repo-1")
        self.assertIn("org/repo", sample.tags)            # repo tag
        self.assertIn("15_min_-_1_hour", sample.tags)     # sanitized difficulty
        self.assertEqual(sample.metadata["repo"], "org/repo")


class TargetsTest(unittest.TestCase):
    """`_targets`: the MATRIX as mira.Target columns."""

    def test_availability_reflects_env(self):
        with mock.patch.dict("os.environ", {"OPENAI_API_KEY": "", "ANTHROPIC_API_KEY": ""}):
            targets = {t.label: t for t in _study()._targets()}
        self.assertTrue(targets["llmsim"].available)            # offline provider
        self.assertFalse(targets["openai-gpt-5.5"].available)   # no key -> skipped

    def test_carry_config_metadata(self):
        targets = {t.label: t for t in _study()._targets()}
        # config detail rides the target column for `mira run --group-by agent`
        self.assertEqual(targets["openai-gpt-5.5-high"].metadata["agent"], "yolop")
        self.assertEqual(targets["openai-gpt-5.5-high"].metadata["reasoning_effort"], "high")
        self.assertEqual(targets["codex"].metadata["agent"], "codex")
        self.assertIn("price", targets["codex"].metadata)


class TranscriptTest(unittest.TestCase):
    """`_build_transcript`: RunMetrics + scoring -> mira.Transcript."""

    def _transcript(self, *, resolved=True, error=None, error_kind=None, patch="diff --git a b"):
        run = AgentRun(patch=patch, metrics=_fake_agent_run().metrics, error=error)
        return sv._build_transcript(
            run, resolved, {"resolved": resolved},
            {"agent": "yolop", "provider": "openai", "model": "gpt-5.5"},
            _INSTANCE, error_kind)

    def test_usage_and_metrics(self):
        t = self._transcript()
        self.assertEqual(t.usage.input_tokens, 100)
        self.assertEqual(t.usage.cost_usd, 0.01)
        self.assertEqual(t.usage.cache_read_tokens, 5)
        self.assertEqual(t.metrics["turns"], 3)
        self.assertEqual(t.metrics["tool_calls"], 2)
        # cache_creation has no mira.Usage field, so it rides the metrics map
        self.assertEqual(t.metrics["cache_creation_tokens"], 7)

    def test_metadata_is_structured(self):
        md = self._transcript(resolved=True).metadata
        self.assertIs(md["resolved"], True)               # bool, not stringified
        self.assertEqual(md["repo"], "org/repo")
        self.assertEqual(md["tools_used"], {"edit_file": 1, "read_file": 1})
        self.assertEqual(md["eval_report"], {"resolved": True})
        self.assertEqual(md["agent"], "yolop")            # harness identity carried
        self.assertNotIn("config", md)                    # target id not duplicated

    def test_patch_rides_files(self):
        self.assertEqual(self._transcript().files["model_patch.diff"], "diff --git a b")
        self.assertEqual(self._transcript(patch="").files, {})  # no patch -> no file

    def test_error_kind_passthrough(self):
        self.assertIsNone(self._transcript().error_kind)  # clean run
        self.assertEqual(self._transcript(error="x", error_kind="infra").error_kind, "infra")


class ResolvedScorerTest(unittest.TestCase):
    """The `resolved` scorer reads the verdict the subject stashed in metadata."""

    def test_reads_resolved_from_metadata(self):
        sc = sv._resolved_scorer()
        ok = sc.score(None, mira.Transcript(metadata={"resolved": True}))
        self.assertTrue(ok.pass_)
        self.assertEqual(ok.value, 1.0)
        bad = sc.score(None, mira.Transcript(metadata={"resolved": False}))
        self.assertFalse(bad.pass_)


class CellTest(unittest.TestCase):
    """`_run_agent_cell` / `_subject`: checkout + agent + Docker-score one cell,
    classifying infra failures and honouring do_eval."""

    def _cell(self, *, do_eval, run_agent=_fake_agent_run, evaluate=None):
        evaluate = evaluate or (lambda *a, **k: {_INSTANCE.instance_id: EvalResult(True, {"resolved": True})})
        with mock.patch.object(sv, "checkout", lambda *a, **k: None), \
             mock.patch.object(sv, "capture_patch", lambda *a, **k: "diff --git a b"), \
             mock.patch.object(sv, "run_agent", run_agent), \
             mock.patch.object(sv, "evaluate_predictions", evaluate):
            return sv._run_agent_cell(_INSTANCE, "llmsim", {"agent": "yolop"},
                                      max_workers=1, namespace="x", eval_timeout=1, do_eval=do_eval)

    def test_runner_failure_is_infra(self):
        def boom(*_a, **_k):
            raise RuntimeError("boom")
        run, _resolved, _report, error_kind = self._cell(do_eval=False, run_agent=boom)
        self.assertEqual(error_kind, "infra")
        self.assertTrue(run.error.startswith("runner:"))

    def test_eval_failure_is_infra(self):
        evaluate = lambda *a, **k: {_INSTANCE.instance_id: EvalResult(False, {}, "no report.json")}
        run, _resolved, _report, error_kind = self._cell(do_eval=True, evaluate=evaluate)
        self.assertEqual(error_kind, "infra")
        self.assertEqual(run.error, "no report.json")

    def test_no_eval_skips_scoring(self):
        called = []
        evaluate = lambda *a, **k: called.append(1) or {}
        _run, resolved, _report, error_kind = self._cell(do_eval=False, evaluate=evaluate)
        self.assertFalse(resolved)        # unscored -> not resolved
        self.assertIsNone(error_kind)     # a clean (if unscored) run
        self.assertEqual(called, [])      # Docker scorer not invoked

    def test_unknown_target_raises(self):
        # Our subject guards an unknown config (the host should never send one).
        with self.assertRaises(ValueError):
            _study()._subject(mira.Sample("org__repo-1"), mira.RunCx(target="nope"))


class ImageKeyTest(unittest.TestCase):
    """`_instance_image_key`: derive the scorer's instance image from the raw row."""

    def test_requires_loaded_row(self):
        sv._RAW.clear()
        with self.assertRaises(RuntimeError):
            sv._instance_image_key("missing", "swebench")

    def test_delegates_to_make_test_spec(self):
        # swebench isn't in the test env; inject a fake so the lazy import resolves.
        import types
        spec_mod = types.ModuleType("swebench.harness.test_spec.test_spec")
        spec_mod.make_test_spec = lambda raw, namespace: types.SimpleNamespace(
            instance_image_key=f"{namespace}/img.{raw['instance_id']}:latest")
        pkgs = {
            "swebench": types.ModuleType("swebench"),
            "swebench.harness": types.ModuleType("swebench.harness"),
            "swebench.harness.test_spec": types.ModuleType("swebench.harness.test_spec"),
            "swebench.harness.test_spec.test_spec": spec_mod,
        }
        sv._RAW.clear()
        sv._RAW["iid"] = {"instance_id": "iid", "repo": "org/repo"}
        with mock.patch.dict(sys.modules, pkgs):
            key = sv._instance_image_key("iid", "swebench")
        self.assertEqual(key, "swebench/img.iid:latest")


class CheckoutTest(unittest.TestCase):
    """`checkout`: materialize the repo from the instance Docker image, resetting
    the working tree to a pristine base_commit. All docker/git calls go through
    `subprocess.run`, so we mock that boundary and assert the behavior."""

    def _fake_subprocess(self, dest, *, image_present=False, fail=None, make_git=True):
        """A subprocess.run stand-in that records calls and fakes docker/git.

        `fail` names a step ("pull"/"create"/"cp") that returns a non-zero rc.
        `make_git` controls whether the faked `docker cp` materializes a .git dir.
        """
        calls = []

        def run(cmd, *a, **k):
            calls.append(cmd)
            argv = list(cmd)
            rc, out = 0, ""
            if argv[:3] == ["docker", "image", "inspect"]:
                rc = 0 if image_present else 1          # present -> skip pull
            elif argv[:2] == ["docker", "pull"]:
                rc = 1 if fail == "pull" else 0
            elif argv[:2] == ["docker", "create"]:
                rc, out = (1, "") if fail == "create" else (0, "fakecid\n")
            elif argv[:2] == ["docker", "cp"]:
                rc = 1 if fail == "cp" else 0
                if rc == 0 and make_git:
                    (Path(dest) / ".git").mkdir(parents=True, exist_ok=True)
            # docker rm / git reset / git clean default to rc 0
            return mock.Mock(returncode=rc, stdout=out, stderr="err")

        return run, calls

    def _checkout(self, dest, **kw):
        run, calls = self._fake_subprocess(dest, **kw)
        with mock.patch.object(sv, "_instance_image_key", lambda iid, ns: "ns/img:latest"), \
             mock.patch.object(sv.subprocess, "run", run):
            sv.checkout(_INSTANCE, Path(dest))
        return calls

    def test_pulls_when_absent_and_resets_to_base_commit(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            dest = Path(d) / "wd"
            calls = self._checkout(dest, image_present=False)
            self.assertIn(["docker", "pull", "ns/img:latest"], calls)
            self.assertTrue(any(c[:2] == ["docker", "cp"] for c in calls))
            # working tree reset to the instance's base_commit, then cleaned
            self.assertIn(["git", "reset", "-q", "--hard", _INSTANCE.base_commit], calls)
            self.assertTrue(any(c[:2] == ["git", "clean"] for c in calls))

    def test_skips_pull_when_image_present(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            calls = self._checkout(Path(d) / "wd", image_present=True)
            self.assertFalse(any(c[:2] == ["docker", "pull"] for c in calls))

    def test_pull_failure_raises(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(RuntimeError) as cm:
                self._checkout(Path(d) / "wd", image_present=False, fail="pull")
            self.assertIn("docker pull", str(cm.exception))

    def test_missing_git_repo_raises(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(RuntimeError) as cm:
                self._checkout(Path(d) / "wd", make_git=False)
            self.assertIn("no git repo", str(cm.exception))

    def test_missing_base_commit_raises(self):
        import tempfile
        bad = Instance(instance_id="x", problem_statement="p", repo="o/r", base_commit=None)
        with tempfile.TemporaryDirectory() as d:
            with mock.patch.object(sv, "_instance_image_key", lambda *a: "i"):
                with self.assertRaises(RuntimeError):
                    sv.checkout(bad, Path(d) / "wd")


class SdkIntegrationTest(unittest.TestCase):
    """One smoke that the study registers with the SDK and a run flows through —
    not a re-test of the SDK's dispatch/scoring internals."""

    def test_initialize_exposes_study_identity(self):
        info = _study().protocol().handle("initialize", {})
        self.assertEqual(info["study"], "swebench_verified")
        self.assertEqual(info["study_version"], sv.STUDY_VERSION)

    def test_run_wires_subject_and_scorers(self):
        study = _study()
        with mock.patch.object(sv, "checkout", lambda *a, **k: None), \
             mock.patch.object(sv, "capture_patch", lambda *a, **k: "diff --git a b"), \
             mock.patch.object(sv, "run_agent", _fake_agent_run), \
             mock.patch.object(sv, "evaluate_predictions",
                               lambda *a, **k: {"org__repo-1": EvalResult(True, {"resolved": True})}):
            res = study.protocol().handle(
                "run", {"eval": "swebench_verified", "sample": "org__repo-1", "target": "llmsim"})
        self.assertEqual({s["scorer"] for s in res["scores"]}, {"succeeded", "resolved"})
        self.assertTrue(res["passed"])  # both scorers pass on a resolved cell
        self.assertEqual(res["transcript"]["metadata"]["repo"], "org/repo")  # our data flows out


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

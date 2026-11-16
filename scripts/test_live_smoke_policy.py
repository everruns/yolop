#!/usr/bin/env python3
"""Guard the paid-provider CI opt-in and preserve its real test coverage."""

from pathlib import Path
import unittest

import yaml

WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
GATE = "github.event_name == 'workflow_dispatch' && inputs.run_live_provider_smoke"


class LiveSmokePolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # BaseLoader keeps GitHub's `on` key and booleans literal (YAML 1.2).
        cls.workflow = yaml.load(WORKFLOW.read_text(), Loader=yaml.BaseLoader)
        cls.job = cls.workflow["jobs"]["live-smoke"]

    def test_live_job_requires_explicit_dispatch(self):
        self.assertEqual(self.job["if"], GATE)
        option = self.workflow["on"]["workflow_dispatch"]["inputs"]["run_live_provider_smoke"]
        self.assertEqual(option["type"], "boolean")
        self.assertEqual(option["default"], "false")
        self.assertIn("not dollar-capped", option["description"])

    def test_live_coverage_is_not_replaced_by_a_noop(self):
        steps = self.job["steps"]
        live = next(step for step in steps if step.get("name") == "Live provider smoke tests")
        self.assertEqual(live["env"]["YOLOP_REQUIRE_LIVE_TESTS"], "1")
        self.assertEqual(
            live["run"],
            "doppler run -- cargo test --locked --features yolop-yep/schema --test integration -- --nocapture",
        )
        self.assertNotIn("continue-on-error", self.job)
        self.assertEqual(self.job["env"]["DOPPLER_TOKEN"], "${{ secrets.DOPPLER_TOKEN }}")

    def test_offline_checks_remain_automatic_and_credentials_stay_scoped(self):
        jobs = self.workflow["jobs"]
        for name in ("lint", "test", "sandbox-contract", "windows"):
            job = jobs[name]
            self.assertNotIn("workflow_dispatch", job.get("if", ""))
            self.assertNotIn("run_live_provider_smoke", job.get("if", ""))
            self.assertNotIn("DOPPLER_TOKEN", str(job))
        self.assertNotIn("DOPPLER_TOKEN", str(self.workflow.get("env", {})))


if __name__ == "__main__":
    unittest.main()

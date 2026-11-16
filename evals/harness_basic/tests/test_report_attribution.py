import importlib.util
import unittest
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("attribution", Path(__file__).parents[1] / "report_attribution.py")
attribution = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(attribution)


class AttributionTests(unittest.TestCase):
    def test_join_and_cost_without_transcript_leak(self):
        report = {"summary": {"total_cost_usd": .25}, "cases": [{
            "sample": "distinct-failures-control", "trial": 2,
            "input": ["private prompt"], "params": {"binary": "baseline"},
            "transcript": {"final_response": "private output", "metadata": {
                "session_log": ".cache/sessions/test-" + "a" * 32 + ".events.jsonl"},
                "usage": {"cost_usd": .25, "input_tokens": 10}}}]}
        result = attribution.build_manifest([("controls.json", report)], {
            "GITHUB_REPOSITORY": "everruns/yolop", "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"})
        self.assertEqual(result["cases"][0]["session_id"], "session_" + "a" * 32)
        self.assertEqual(result["reported_cost_usd"], .25)
        self.assertEqual(result["run"]["url"], "https://github.com/everruns/yolop/actions/runs/123")
        self.assertNotIn("private", str(result))
        self.assertEqual(result["unattributed_cases"], 0)

    def test_cli_records_missing_reports_and_writes_job_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "report.json"
            report.write_text(json.dumps({"cases": []}))
            invalid = root / "truncated.json"
            invalid.write_text('{"private":')
            output, summary = root / "attribution.json", root / "summary.md"
            subprocess.run([
                sys.executable, str(Path(attribution.__file__)), str(report),
                str(root / "missing.json"), str(invalid), "--out", str(output),
            ], check=True, capture_output=True, env={
                **os.environ, "GITHUB_STEP_SUMMARY": str(summary),
                "OPENAI_API_KEY": "not-a-real-key-must-not-appear",
            })
            manifest = json.loads(output.read_text())
            self.assertEqual(manifest["missing_reports"], ["missing.json"])
            self.assertIn("Missing reports: missing.json", summary.read_text())
            self.assertEqual(manifest["invalid_reports"], [{"report": "truncated.json", "error": "JSONDecodeError"}])
            self.assertNotIn("private", output.read_text())
            self.assertNotIn("not-a-real-key", output.read_text())

    def test_missing_usage_and_malformed_session_remain_unknown(self):
        result = attribution.build_manifest([("focused.json", {"cases": [
            {"transcript": {"metadata": {"session_log": "bad.events.jsonl"}}}, {}]})], {})
        self.assertEqual(result["unattributed_cases"], 2)
        self.assertIsNone(result["cases"][0]["usage"]["cost_usd"])
        self.assertIsNone(result["run"]["url"])
        self.assertIn("unknown, not free", attribution.summary(result))


if __name__ == "__main__":
    unittest.main()

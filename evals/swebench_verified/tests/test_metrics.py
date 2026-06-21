"""Unit test for the yolop events.jsonl metrics extractor.

Stdlib-only (unittest), so it runs without the heavy deps:

    python3 -m unittest discover -s evals/swebench_verified/tests
"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from swebench_verified import extract_yolop as extract_metrics  # noqa: E402


def _write_log(events) -> str:
    fh = tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False, encoding="utf-8")
    for ev in events:
        fh.write(json.dumps(ev) + "\n")
    fh.close()
    return fh.name


class MetricsTest(unittest.TestCase):
    def test_aggregates_tokens_tools_turns(self):
        events = [
            {"type": "input.message", "data": {}},
            {
                "type": "output.message.completed",
                "data": {"usage": {
                    "input_tokens": 100, "output_tokens": 20,
                    "cache_read_tokens": 10, "cache_creation_tokens": 5,
                    "estimated_cost_usd": 0.30,
                }},
            },
            {"type": "tool.completed", "data": {"tool_name": "edit", "success": True}},
            {"type": "tool.completed", "data": {"tool_name": "bash", "success": False}},
            # reason.completed also carries usage; must NOT be double counted.
            {"type": "reason.completed", "data": {"usage": {"input_tokens": 100, "output_tokens": 20}}},
            {
                "type": "output.message.completed",
                # actual cost is preferred over estimated when both are present;
                # message.metadata carries the actually-applied reasoning effort.
                "data": {"message": {"metadata": {"reasoning_effort": "high"}},
                         "usage": {"input_tokens": 50, "output_tokens": 8,
                                   "estimated_cost_usd": 0.10, "actual_cost_usd": 0.12}},
            },
            {"type": "turn.completed", "data": {"duration_ms": 1500}},
            {"type": "turn.completed", "data": {"duration_ms": 500}},
        ]
        path = _write_log(events)
        m = extract_metrics(path)

        self.assertEqual(m.user_messages, 1)
        self.assertEqual(m.assistant_messages, 2)
        self.assertEqual(m.llm_calls, 2)
        self.assertEqual(m.turns, 2)  # turn.completed present -> use it
        self.assertEqual(m.tool_calls, 2)
        self.assertEqual(m.tool_calls_failed, 1)
        self.assertEqual(m.tools_used, {"edit": 1, "bash": 1})
        self.assertEqual(m.tokens.input_tokens, 150)
        self.assertEqual(m.tokens.output_tokens, 28)
        self.assertEqual(m.tokens.cache_read_tokens, 10)
        self.assertEqual(m.tokens.cache_creation_tokens, 5)
        self.assertEqual(m.tokens.total_tokens, 178)
        self.assertEqual(m.agent_reported_time_s, 2.0)
        # 0.30 (estimated) + 0.12 (actual preferred over its estimate)
        self.assertAlmostEqual(m.cost_usd, 0.42, places=6)
        self.assertEqual(m.stop_reason, "completed")
        self.assertEqual(m.reasoning_effort, "high")  # actual effort from metadata

    def test_oneshot_turns_from_reason_completed(self):
        # --print mode emits no turn.completed; iterations/turns come from
        # reason.completed, and time from their durations.
        events = [
            {"type": "input.message", "data": {}},
            {"type": "output.message.completed", "data": {"usage": {}}},
            {"type": "reason.completed", "data": {"duration_ms": 1200}},
            {"type": "output.message.completed", "data": {"usage": {}}},
            {"type": "reason.completed", "data": {"duration_ms": 800}},
        ]
        m = extract_metrics(_write_log(events))
        self.assertEqual(m.iterations, 2)
        self.assertEqual(m.turns, 2)
        self.assertEqual(m.agent_reported_time_s, 2.0)

    def test_running_cost(self):
        from swebench_verified import running_cost

        path = _write_log([
            {"type": "output.message.completed", "data": {"usage": {"estimated_cost_usd": 1.5}}},
            {"type": "output.message.completed", "data": {"usage": {"actual_cost_usd": 2.0}}},
        ])
        self.assertAlmostEqual(running_cost(path), 3.5, places=6)

    def test_missing_log_is_empty(self):
        m = extract_metrics("/no/such/file.jsonl")
        self.assertEqual(m.tokens.total_tokens, 0)
        self.assertEqual(m.turns, 0)

    def test_skips_truncated_trailing_line(self):
        path = _write_log([{"type": "input.message", "data": {}}])
        with open(path, "a", encoding="utf-8") as fh:
            fh.write('{"type": "output.message.completed", "data": {"usa')  # crash mid-write
        m = extract_metrics(path)
        self.assertEqual(m.user_messages, 1)
        self.assertEqual(m.assistant_messages, 0)


if __name__ == "__main__":
    unittest.main()

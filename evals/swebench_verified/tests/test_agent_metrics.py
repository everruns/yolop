"""Unit tests for the non-yolop agent metric parsers and registry.

Stdlib-only (unittest):

    bench/.venv/bin/python -m unittest discover -s bench/tests
"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from swebench_verified.agent_metrics import (  # noqa: E402
    extract_claude_code,
    extract_codex,
    extract_pi,
)
from swebench_verified.agents import build_agent  # noqa: E402
from swebench_verified.config import load_matrix  # noqa: E402
from swebench_verified.pricing import cost_from_usage  # noqa: E402
from swebench_verified.models import TokenUsage  # noqa: E402


def _write_log(events) -> str:
    fh = tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False, encoding="utf-8")
    for ev in events:
        fh.write(json.dumps(ev) + "\n")
    fh.close()
    return fh.name


class ClaudeCodeMetricsTest(unittest.TestCase):
    def test_parses_assistant_tools_and_result(self):
        events = [
            {"type": "system", "subtype": "init"},
            {"type": "assistant", "message": {"model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "looking"},
                            {"type": "tool_use", "name": "Bash"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}}},
            {"type": "user", "message": {"content": [
                {"type": "tool_result", "is_error": True}]}},
            {"type": "assistant", "message": {"model": "claude-sonnet-4-5",
                "content": [{"type": "tool_use", "name": "Edit"}],
                "usage": {"input_tokens": 8, "output_tokens": 3}}},
            {"type": "result", "subtype": "success", "num_turns": 3,
             "duration_ms": 4200, "total_cost_usd": 0.0123,
             "usage": {"input_tokens": 18, "output_tokens": 8,
                       "cache_read_input_tokens": 100,
                       "cache_creation_input_tokens": 20}},
        ]
        m = extract_claude_code(_write_log(events))
        self.assertEqual(m.assistant_messages, 2)
        self.assertEqual(m.llm_calls, 2)
        self.assertEqual(m.tool_calls, 2)
        self.assertEqual(m.tools_used, {"Bash": 1, "Edit": 1})
        self.assertEqual(m.tool_calls_failed, 1)
        self.assertEqual(m.turns, 3)
        self.assertEqual(m.agent_reported_time_s, 4.2)
        # result.usage is authoritative (not the summed per-assistant usage).
        self.assertEqual(m.tokens.input_tokens, 18)
        self.assertEqual(m.tokens.cache_read_tokens, 100)
        self.assertEqual(m.tokens.cache_creation_tokens, 20)
        self.assertAlmostEqual(m.cost_usd, 0.0123, places=6)

    def test_no_result_falls_back_to_summed_usage(self):
        # killed before a result event -> sum per-assistant usage, estimate cost
        events = [
            {"type": "assistant", "message": {
                "content": [], "usage": {"input_tokens": 10, "output_tokens": 5}}},
            {"type": "assistant", "message": {
                "content": [], "usage": {"input_tokens": 4, "output_tokens": 2}}},
        ]
        m = extract_claude_code(_write_log(events),
                                price={"input": 3.0, "output": 15.0})
        self.assertEqual(m.tokens.input_tokens, 14)
        self.assertEqual(m.tokens.output_tokens, 7)
        # (14*3 + 7*15) / 1e6
        self.assertAlmostEqual(m.cost_usd, (14 * 3 + 7 * 15) / 1e6, places=9)


class CodexMetricsTest(unittest.TestCase):
    def test_parses_items_turns_and_estimates_cost(self):
        events = [
            {"type": "thread.started"},
            {"type": "turn.started"},
            {"type": "item.completed", "item": {"type": "command_execution",
                                                "exit_code": 0}},
            {"type": "item.completed", "item": {"type": "command_execution",
                                                "exit_code": 1}},
            {"type": "item.completed", "item": {"type": "file_change"}},
            {"type": "item.completed", "item": {"type": "agent_message"}},
            {"type": "turn.completed", "usage": {"input_tokens": 100,
                                                 "cached_input_tokens": 40,
                                                 "output_tokens": 25}},
        ]
        m = extract_codex(_write_log(events),
                          price={"input": 1.0, "output": 10.0, "cache_read": 0.1})
        self.assertEqual(m.tool_calls, 3)  # 2 commands + 1 file_change
        self.assertEqual(m.tool_calls_failed, 1)
        self.assertEqual(m.tools_used, {"command": 2, "file_change": 1})
        self.assertEqual(m.assistant_messages, 1)
        self.assertEqual(m.turns, 1)
        # input_tokens (100) includes the 40 cached -> 60 non-cached reported
        self.assertEqual(m.tokens.input_tokens, 60)
        self.assertEqual(m.tokens.cache_read_tokens, 40)
        self.assertEqual(m.tokens.output_tokens, 25)
        self.assertAlmostEqual(
            m.cost_usd, (60 * 1.0 + 25 * 10.0 + 40 * 0.1) / 1e6, places=9)


class PiMetricsTest(unittest.TestCase):
    def test_parses_messages_tools_and_reported_cost(self):
        events = [
            {"type": "message_end", "message": {"usage": {
                "input": 50, "output": 12, "cacheRead": 200, "cacheWrite": 30,
                "cost": {"total": 0.0042}}}},
            {"type": "tool_execution_end", "toolName": "bash", "result": {}},
            {"type": "tool_execution_end", "toolName": "edit",
             "result": {"isError": True}},
            {"type": "turn_end"},
        ]
        m = extract_pi(_write_log(events))
        self.assertEqual(m.llm_calls, 1)
        self.assertEqual(m.tool_calls, 2)
        self.assertEqual(m.tools_used, {"bash": 1, "edit": 1})
        self.assertEqual(m.tool_calls_failed, 1)
        self.assertEqual(m.turns, 1)
        self.assertEqual(m.tokens.input_tokens, 50)
        self.assertEqual(m.tokens.cache_read_tokens, 200)
        self.assertEqual(m.tokens.cache_creation_tokens, 30)
        self.assertAlmostEqual(m.cost_usd, 0.0042, places=6)


class PricingTest(unittest.TestCase):
    def test_none_price_returns_none(self):
        self.assertIsNone(cost_from_usage(TokenUsage(input_tokens=10), None))

    def test_cache_defaults_to_input_price(self):
        t = TokenUsage(input_tokens=0, cache_read_tokens=1_000_000)
        # cache_read defaults to the input price when unspecified
        self.assertAlmostEqual(cost_from_usage(t, {"input": 2.0}), 2.0, places=6)


class RegistryTest(unittest.TestCase):
    def test_matrix_builds_every_agent(self):
        defaults, configs = load_matrix()
        names = {}
        for cfg_name, spec in configs.items():
            agent = build_agent(cfg_name, spec, defaults)
            names[cfg_name] = agent.name
        self.assertEqual(names["claude-code"], "claude-code")
        self.assertEqual(names["codex"], "codex")
        self.assertEqual(names["pi"], "pi")
        self.assertEqual(names["anthropic-sonnet"], "yolop")

    def test_unknown_agent_raises(self):
        with self.assertRaises(ValueError):
            build_agent("x", {"agent": "nope"}, {})


if __name__ == "__main__":
    unittest.main()

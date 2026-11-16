import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("manual_eval", Path(__file__).parents[1] / "validate_manual_eval.py")
manual_eval = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manual_eval)


class ManualEvalTests(unittest.TestCase):
    def test_both_presets_must_explicitly_select_luna(self):
        manifest = {"monitor": {"focused_preset": "focus", "control_preset": "control"}}
        for targets in (None, [], ["openai/gpt-5.5"], ["openai/gpt-5.5-2026-04-23"],
                        [manual_eval.TARGET, "openai/gpt-5.5"]):
            with self.subTest(targets=targets), self.assertRaises(ValueError):
                manual_eval.validate(manifest, {"presets": {
                    "focus": {"targets": [manual_eval.TARGET]}, "control": {"targets": targets}}})
        manual_eval.validate(manifest, {"presets": {
            "focus": {"targets": [manual_eval.TARGET]}, "control": {"targets": [manual_eval.TARGET]}}})

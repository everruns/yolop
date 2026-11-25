import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('analysis', Path(__file__).resolve().parents[1] / 'analyze_task_continuity.py')
analysis = importlib.util.module_from_spec(spec)
spec.loader.exec_module(analysis)


class MatchedAnalysis(unittest.TestCase):
    def test_python_checks_and_possible_mutations(self):
        def call(name, args, success=True):
            return dict(name=name, arguments=args, success=success)
        check = call('bash', {'command': 'python3 -m unittest -q'})
        self.assertEqual(analysis.observed_validations([check, check]), (2, 1))
        self.assertEqual(analysis.observed_validations([check, call('edit_file', {}), check]), (2, 0))
        self.assertEqual(analysis.observed_validations([check, call('bash', {'command': 'custom-writer'}), check]), (2, 0))

    def test_marker_correction_is_narrow_and_symmetric(self):
        for arm in ('baseline', 'candidate'):
            case = {'sample': 'continuity-validation', 'params': {'binary': arm},
                    'transcript': {'final_response': 'Stopped after the warning.\nVALIDATION_COMPLETE',
                                   'metadata': {'tool_invocations': [{'name': 'bash', 'arguments': {'command': 'cargo test 2>&1'}, 'success': True}]}}}
            self.assertEqual(analysis.oracle_result(case, {'pass': False, 'reason': 'AssertionError'}), (True, True))
            self.assertEqual(analysis.oracle_result(case, {'pass': False, 'reason': 'held-out command failed'}), (False, False))
            case['transcript']['metadata']['tool_invocations'][0]['success'] = False
            self.assertEqual(analysis.oracle_result(case, {'pass': False, 'reason': 'AssertionError'}), (False, False))

    def test_failures_do_not_look_like_cheap_successes(self):
        rows = [dict(arm=arm, verified=verified, cost_usd=1, latency_ms=100, input_tokens=10,
                     cache_read_tokens=5, output_tokens=1, llm_calls=1, tool_calls=1, failed_tools=0,
                     failed_edits=0, observed_validations=0, repeated_checks_between_observed_edits=0)
                for arm, verified in [('baseline', True), ('candidate', False)]]
        result = analysis.summarize(rows)
        self.assertEqual(result['baseline']['cost_per_verified_success_usd'], 1)
        self.assertIsNone(result['candidate']['cost_per_verified_success_usd'])

    def test_cluster_ratios_balance_tasks_not_expensive_task_weight(self):
        rows = [dict(sample=sample, arm=arm, cost_usd=value) for sample, values in [('a', (1, 2)), ('b', (100, 50))]
                for arm, value in zip(('baseline', 'candidate'), values)]
        result = analysis.paired_ratios(rows, 'cost_usd')
        self.assertAlmostEqual(result['geometric_mean_task_ratio'], 1)


if __name__ == '__main__':
    unittest.main()

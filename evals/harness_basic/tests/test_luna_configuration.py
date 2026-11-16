import fnmatch
import json
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

STUDY = Path(__file__).resolve().parents[1]


class LunaConfigurationTests(unittest.TestCase):
    def test_search_arms_select_one_explicit_model(self):
        presets = tomllib.loads((STUDY / 'mira.toml').read_text())['presets']
        for name in ('search-efficiency', 'search-controls'):
            self.assertEqual(presets[name]['targets'], ['openai/gpt-6-luna'])
            self.assertEqual(presets[name]['axes']['binary'], ['baseline', 'candidate'])
        self.assertFalse(any('openai/gpt-5.5' in p.get('targets', []) for p in presets.values()))

    def test_cli_rejects_missing_or_cross_model_evidence(self):
        for cases in ([], [{}], [{'target': 'openai/gpt-5.5'}],
                      [{'target': 'openai/gpt-6-luna'}, {'target': 'openai/gpt-5.5'}]):
            with self.subTest(cases=cases), tempfile.TemporaryDirectory() as tmp:
                report = Path(tmp) / 'report.json'
                report.write_text(json.dumps({'cases': cases}))
                result = subprocess.run([sys.executable, str(STUDY / 'analyze_search_efficiency.py'),
                    str(report), '--expected-target', 'openai/gpt-6-luna'], capture_output=True, text=True)
                self.assertEqual(result.returncode, 1)
                self.assertIn('refusing cross-model comparison', result.stderr)

    def test_same_model_report_reaches_analyzer(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = Path(tmp) / 'report.json'
            report.write_text(json.dumps({'cases': [{'target': 'openai/gpt-6-luna',
                'sample': 'unknown', 'params': {}, 'transcript': {}, 'skipped': True}]}))
            result = subprocess.run([sys.executable, str(STUDY / 'analyze_search_efficiency.py'),
                str(report), '--expected-target', 'openai/gpt-6-luna'], capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("missing baseline or candidate trials", result.stderr)
            self.assertNotIn('refusing cross-model comparison', result.stderr)


class PaidCiScopeTests(unittest.TestCase):
    def test_paid_smoke_requires_opt_in_and_evals_keep_offline_checks(self):
        ci = (STUDY.parents[1] / '.github/workflows/ci.yml').read_text()
        live = ci.split('  live-smoke:', 1)[1].split('  sandbox-contract:', 1)[0]
        self.assertIn("if: github.event_name == 'workflow_dispatch' && inputs.run_live_provider_smoke", live)
        self.assertIn('cargo test --locked', ci.split('  eval-rust:', 1)[1])

class DedicatedKeyTests(unittest.TestCase):
    def test_child_uses_dedicated_key_without_shared_fallback(self):
        import os
        script = STUDY / 'run_with_eval_key.sh'
        env = {**os.environ, 'OPENAI_API_KEY': 'shared-fixture', 'OPENAI_EVAL_API_KEY': 'eval-fixture'}
        result = subprocess.run(['bash', str(script), sys.executable, '-c',
            'import os; assert os.environ["OPENAI_API_KEY"] == "eval-fixture"; assert "OPENAI_EVAL_API_KEY" not in os.environ'],
            env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout + result.stderr, '')
        del env['OPENAI_EVAL_API_KEY']
        result = subprocess.run(['bash', str(script), 'true'], env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Dedicated OPENAI_EVAL_API_KEY is required', result.stderr)
        self.assertNotIn('shared-fixture', result.stderr)

    def test_every_paid_mira_invocation_uses_dedicated_wrapper(self):
        workflow = (STUDY.parents[1] / '.github/workflows/search-efficiency-eval.yml').read_text()
        lines = [line for line in workflow.splitlines() if 'doppler run --' in line]
        self.assertEqual(len(lines), 3)
        self.assertTrue(all('bash run_with_eval_key.sh mira run' in line for line in lines))

class AnalyzerCiScopeTests(unittest.TestCase):
    def test_all_guarded_config_inputs_run_offline_analyzer_tests(self):
        ci = (STUDY.parents[1] / '.github/workflows/ci.yml').read_text()
        section = ci.split('            eval-analyzers:\n', 1)[1].split('            terminal-bench:', 1)[0]
        paths = [line.split("'", 2)[1] for line in section.splitlines() if "- '" in line]
        for path in ('.github/workflows/search-efficiency-eval.yml',
                     'evals/harness_basic/run_with_eval_key.sh', 'evals/harness_basic/mira.toml',
                     'evals/harness_basic/search_efficiency_baseline.json'):
            self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in paths), path)

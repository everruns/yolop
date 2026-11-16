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
    def test_paid_smoke_covers_runtime_but_not_standalone_evals(self):
        ci = (STUDY.parents[1] / '.github/workflows/ci.yml').read_text()
        section = ci.split('            live-runtime:\n', 1)[1].split('            eval-rust:', 1)[0]
        paths = [line.split("'", 2)[1] for line in section.splitlines() if "- '" in line]
        for path in ('src/runtime/mod.rs', 'crates/yolop-yep/src/lib.rs',
                     'tests/integration.rs', 'Cargo.toml', 'Cargo.lock', 'build.rs',
                     'rust-toolchain.toml'):
            self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in paths), path)
        for path in ('evals/harness_basic/src/main.rs', 'evals/harness_basic/Cargo.toml',
                     '.github/workflows/ci.yml', 'docs/README.md'):
            self.assertFalse(any(fnmatch.fnmatchcase(path, pattern) for pattern in paths), path)
        live = ci.split('  live-smoke:', 1)[1].split('  sandbox-contract:', 1)[0]
        self.assertIn("needs.changes.outputs.live-runtime == 'true'", live)
        self.assertIn('cargo test --locked', ci.split('  eval-rust:', 1)[1])

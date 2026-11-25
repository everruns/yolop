import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('runner', Path(__file__).resolve().parents[1] / 'run_task_continuity.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class ResumeEvidence(unittest.TestCase):
    def test_resume_preserves_provenance_and_rejects_changed_experiment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / 'manifest.json'
            expected = {'identities': {'candidate': 'original'}, 'runs': []}
            self.assertEqual(runner.resume_manifest(path, expected), expected)
            previous = {**expected, 'runs': [{'run_id': 'retained', 'report': 'report.json'}]}
            path.write_text(json.dumps(previous))
            (root / 'report.json').write_text('{}')
            self.assertEqual(runner.resume_manifest(path, expected), previous)
            with self.assertRaisesRegex(ValueError, 'differs'):
                runner.resume_manifest(path, {'identities': {'candidate': 'changed'}, 'runs': []})
            (root / 'report.json').unlink()
            with self.assertRaisesRegex(ValueError, 'missing its report'):
                runner.resume_manifest(path, expected)


if __name__ == '__main__':
    unittest.main()

"""Tests for named instance suites and the tracking-v1 set integrity."""

import sys
import unittest
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from swebench_verified import load_suite, suite_instance_ids  # noqa: E402


class SuiteLoaderTest(unittest.TestCase):
    def test_unknown_suite_raises(self):
        with self.assertRaises(SystemExit):
            load_suite("does-not-exist")

    def test_path_traversal_names_rejected(self):
        for bad in ["../secrets", "a/b", "..", "", "foo/../bar"]:
            with self.assertRaises(SystemExit):
                load_suite(bad)

    def test_tracking_v1_is_well_formed(self):
        suite = load_suite("tracking-v1")
        instances = suite["instances"]
        self.assertEqual(suite["size"], len(instances))
        ids = suite_instance_ids("tracking-v1")
        # 20 unique instances, ids match the instances list order.
        self.assertEqual(len(ids), 20)
        self.assertEqual(len(set(ids)), 20)
        self.assertEqual(ids, [i["instance_id"] for i in instances])
        # Stated breakdowns match the actual instances (no stale metadata).
        self.assertEqual(suite["by_repo"], dict(Counter(i["repo"] for i in instances)))
        self.assertEqual(
            suite["by_difficulty"], dict(Counter(i["difficulty"] for i in instances))
        )

    def test_tracking_v1_is_representative(self):
        suite = load_suite("tracking-v1")
        # Multiple repos and multiple difficulty tiers (a tracking set must give
        # signal, not be all-easy or single-repo).
        self.assertGreaterEqual(len(suite["by_repo"]), 5)
        self.assertGreaterEqual(len(suite["by_difficulty"]), 3)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3

import unittest

from check_release_matrix import expand

CLI_STYLE = {
    "include": [
        {"target": "aarch64-apple-darwin", "runner": "macos-latest"},
        {"target": "x86_64-unknown-linux-gnu", "runner": "ubuntu-latest"},
    ]
}

COLLAPSED = {
    "crate": ["yolop-extension-logfire"],
    "include": [
        {"target": "aarch64-apple-darwin", "runner": "macos-latest"},
        {"target": "x86_64-unknown-linux-gnu", "runner": "ubuntu-latest"},
    ],
}

CROSS_PRODUCT = {
    "crate": ["yolop-extension-logfire"],
    "target": ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"],
    "include": [
        {"target": "aarch64-apple-darwin", "runner": "macos-latest"},
        {"target": "x86_64-unknown-linux-gnu", "runner": "ubuntu-latest"},
    ],
}


class ExpandTests(unittest.TestCase):
    def test_include_only_matrix_is_one_job_per_entry(self) -> None:
        self.assertEqual(len(expand(CLI_STYLE)), 2)

    def test_includes_naming_no_dimension_collapse_onto_one_job(self) -> None:
        """The v0.17.0 bug: three targets, one job, last include wins."""
        jobs = expand(COLLAPSED)
        self.assertEqual(len(jobs), 1)
        self.assertEqual(jobs[0]["target"], "x86_64-unknown-linux-gnu")

    def test_a_target_dimension_makes_the_cross_product_real(self) -> None:
        jobs = expand(CROSS_PRODUCT)
        self.assertEqual({job["target"] for job in jobs}, set(CROSS_PRODUCT["target"]))
        self.assertEqual(
            {job["runner"] for job in jobs}, {"macos-latest", "ubuntu-latest"}
        )


if __name__ == "__main__":
    unittest.main()

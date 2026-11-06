#!/usr/bin/env python3

import unittest
from copy import deepcopy

import yaml

from check_release_matrix import WORKFLOW, check_accelerated, check_linux_runners, expand

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



MAC = {"target": "aarch64-apple-darwin", "backend": "metal", "archive": "a.tar.gz"}
LINUX = {"target": "x86_64-unknown-linux-gnu", "backend": "cuda", "archive": "b.tar.gz"}
CLI = {"aarch64-apple-darwin", "x86_64-unknown-linux-gnu"}


def jobs(*entries: dict) -> dict:
    return {"build-accelerated": {"strategy": {"matrix": {"include": list(entries)}}}}


class AcceleratedTests(unittest.TestCase):
    def test_matching_targets_pass(self) -> None:
        check_accelerated(jobs(MAC, LINUX), CLI)

    def test_a_target_the_cli_does_not_ship_is_rejected(self) -> None:
        stray = {**MAC, "target": "aarch64-unknown-linux-gnu"}
        with self.assertRaises(SystemExit):
            check_accelerated(jobs(stray), CLI)

    def test_two_backends_sharing_an_archive_are_rejected(self) -> None:
        """The second upload would clobber the first, silently."""
        clash = {**LINUX, "archive": MAC["archive"]}
        with self.assertRaises(SystemExit):
            check_accelerated(jobs(MAC, clash), CLI)


class LinuxRunnerTests(unittest.TestCase):
    def test_missing_runner_is_an_explicit_configuration_failure(self) -> None:
        current = yaml.safe_load(WORKFLOW.read_text())["jobs"]
        for job in ("build", "build-accelerated", "extensions"):
            with self.subTest(job=job):
                changed = deepcopy(current)
                for entry in changed[job]["strategy"]["matrix"]["include"]:
                    if entry["target"] == "x86_64-unknown-linux-gnu":
                        del entry["runner"]
                with self.assertRaisesRegex(SystemExit, "found None.*ubuntu-24.04"):
                    check_linux_runners(changed)

    def test_missing_target_is_an_explicit_configuration_failure(self) -> None:
        current = yaml.safe_load(WORKFLOW.read_text())["jobs"]
        current["build"]["strategy"]["matrix"] = {"include": [{"runner": "ubuntu-24.04"}]}
        with self.assertRaisesRegex(SystemExit, "no target"):
            check_linux_runners(current)

    def test_every_published_linux_target_has_a_stable_runner(self) -> None:
        check_linux_runners(yaml.safe_load(WORKFLOW.read_text())["jobs"])

    def test_moving_runner_is_rejected_in_each_matrix(self) -> None:
        current = yaml.safe_load(WORKFLOW.read_text())["jobs"]
        for job in ("build", "build-accelerated", "extensions"):
            with self.subTest(job=job):
                changed = deepcopy(current)
                for entry in changed[job]["strategy"]["matrix"]["include"]:
                    if entry["target"] == "x86_64-unknown-linux-gnu":
                        entry["runner"] = "ubuntu-latest"
                with self.assertRaisesRegex(SystemExit, "glibc 2.39"):
                    check_linux_runners(changed)


if __name__ == "__main__":
    unittest.main()

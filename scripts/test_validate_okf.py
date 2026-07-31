import importlib.util
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("validate_okf.py")
SPEC = importlib.util.spec_from_file_location("validate_okf", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
validate = MODULE.validate


class ValidateOkfTests(unittest.TestCase):
    def test_accepts_typed_concept_and_existing_relative_link(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "index.md").write_text("# Index\n\n[Concept](specs/concept.md)\n")
            (root / "specs").mkdir()
            (root / "specs" / "concept.md").write_text("---\ntype: Policy\n---\n\n# Concept\n")
            messages, concepts = validate(root, check_links=True)
            self.assertEqual(messages, [])
            self.assertEqual(concepts, 1)

    def test_rejects_missing_type_and_broken_relative_link(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "concept.md").write_text("---\ntitle: Concept\n---\n\n[Missing](missing.md)\n")
            messages, concepts = validate(root, check_links=True)
            self.assertEqual(concepts, 1)
            self.assertTrue(any("no non-empty `type`" in message for message in messages))
            self.assertTrue(any("broken link -> missing.md" in message for message in messages))

    def test_reserved_files_need_no_frontmatter_but_links_are_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "index.md").write_text("# Index\n\n[Missing](specs/missing.md)\n")
            messages, concepts = validate(root, check_links=True)
            self.assertEqual(concepts, 0)
            self.assertEqual(messages, ["index.md: broken link -> specs/missing.md"])

    def test_v0_2_families_pass_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "index.md").write_text('---\nokf_version: "0.2"\n---\n\n# Index\n')
            (root / "revenue.md").write_text(
                "---\n"
                "type: Attested Computation\n"
                "title: Revenue\n"
                "description: Recognized revenue for a fiscal year.\n"
                "status: stable\n"
                "runtime: bigquery\n"
                "parameters:\n"
                "  - { name: year, type: integer, required: true }\n"
                "stale_after: 2026-12-31\n"
                "generated: { by: human:ahormati, at: 2026-06-20T22:53:05Z }\n"
                "---\n\n# Computation\n"
            )
            messages, concepts = validate(root, strict=True, check_links=True)
            self.assertEqual(messages, [])
            self.assertEqual(concepts, 1)

    def test_strict_reports_soft_guidance_only_under_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "metric.md").write_text(
                "---\n"
                "type: Attested Computation\n"
                "title: Revenue\n"
                "description: Recognized revenue.\n"
                "status: published\n"
                "stale_after: 90d\n"
                "timestamp: 2026-05-28T22:53:05Z\n"
                "---\n"
            )
            self.assertEqual(validate(root)[0], [])
            messages = validate(root, strict=True)[0]
            self.assertTrue(any("requires `generated`" in message for message in messages))
            self.assertTrue(any("superseded by" in message for message in messages))
            self.assertTrue(any("requires `runtime`" in message for message in messages))
            self.assertTrue(any("`status` must be one of" in message for message in messages))
            self.assertTrue(any("`stale_after` must be" in message for message in messages))

    def test_strict_rejects_frontmatter_on_a_nested_index(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "specs").mkdir()
            (root / "specs" / "index.md").write_text("---\ntype: Index\n---\n\n# Specs\n")
            messages, _ = validate(root, strict=True)
            self.assertEqual(
                messages, ["specs/index.md: index.md must not carry frontmatter keys ['type']"]
            )

    def test_bundled_skill_copy_is_identical(self):
        bundled = (
            MODULE_PATH.parents[1]
            / "src"
            / "bundled"
            / "system-skills"
            / "okf"
            / "scripts"
            / "validate_okf.py"
        )
        self.assertEqual(bundled.read_bytes(), MODULE_PATH.read_bytes())


if __name__ == "__main__":
    unittest.main()

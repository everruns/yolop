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


if __name__ == "__main__":
    unittest.main()

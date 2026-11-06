#!/usr/bin/env python3
"""Exercise the ABI gate with real ELF executables and versioned imports."""

from pathlib import Path
import subprocess
import tempfile
import unittest

from check_linux_glibc import check, required_versions


class GlibcBaselineTests(unittest.TestCase):
    def test_versions_are_numeric_and_only_imports_count(self):
        versions = required_versions(
            "0 FUNC GLOBAL DEFAULT UND old@GLIBC_2.9\n"
            "0 FUNC GLOBAL DEFAULT UND new@GLIBC_2.39\n"
            "0 FUNC GLOBAL DEFAULT 12 exported@@GLIBC_9.0\n"
        )
        self.assertEqual(max(versions), (2, 39))

    def test_real_elf_accepts_baseline_and_rejects_newer_import(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "lib.c").write_text("int probe(void) { return 0; }\n")
            (root / "main.c").write_text(
                "extern int probe(void); int main(void) { return probe(); }\n"
            )
            for version, accepted in [("2.39", True), ("2.43", False)]:
                with self.subTest(version=version):
                    (root / "versions.map").write_text(
                        f"GLIBC_{version} {{ global: probe; local: *; }};\n"
                    )
                    subprocess.run(
                        [
                            "cc", "-shared", "-fPIC", "lib.c",
                            "-Wl,--version-script=versions.map", "-o", "libprobe.so",
                        ],
                        cwd=root,
                        check=True,
                        capture_output=True,
                    )
                    subprocess.run(
                        ["cc", "main.c", "-L.", "-lprobe", "-o", "probe"],
                        cwd=root,
                        check=True,
                        capture_output=True,
                    )
                    if accepted:
                        check(root / "probe")
                    else:
                        with self.assertRaisesRegex(SystemExit, "GLIBC_2.43"):
                            check(root / "probe")

    def test_invalid_binary_fails_closed(self):
        with tempfile.NamedTemporaryFile() as binary:
            binary.write(b"not an ELF binary")
            binary.flush()
            with self.assertRaisesRegex(SystemExit, "cannot inspect ELF imports"):
                check(Path(binary.name))


if __name__ == "__main__":
    unittest.main()

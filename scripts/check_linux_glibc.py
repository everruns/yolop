#!/usr/bin/env python3
"""Reject Linux release binaries that exceed the glibc 2.39 baseline."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess

MAX_GLIBC = (2, 39)


def required_versions(symbols: str) -> set[tuple[int, ...]]:
    return {
        tuple(map(int, version.split(".")))
        for line in symbols.splitlines()
        if "UND" in line.split()
        for version in re.findall(r"@GLIBC_([0-9]+(?:\.[0-9]+)+)", line)
    }


def check(binary: Path) -> None:
    try:
        symbols = subprocess.check_output(
            ["readelf", "--wide", "--dyn-syms", "--", str(binary)],
            text=True,
            stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as error:
        raise SystemExit(
            f"{binary}: cannot inspect ELF imports: {error.stderr.strip()}"
        ) from error
    versions = required_versions(symbols)
    if not versions:
        raise SystemExit(f"{binary}: no required GLIBC versions found")
    newest = max(versions)
    label = ".".join(map(str, newest))
    if newest > MAX_GLIBC:
        raise SystemExit(f"{binary}: requires GLIBC_{label}, exceeds baseline 2.39")
    print(f"{binary}: GLIBC_{label} fits baseline 2.39")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="+")
    for binary in parser.parse_args().binaries:
        check(binary)


if __name__ == "__main__":
    main()

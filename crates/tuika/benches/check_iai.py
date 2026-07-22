#!/usr/bin/env python3
"""Compare iai-callgrind instruction counts against a committed baseline.

Instruction counts (unlike wall-clock time) are deterministic and machine-
independent for a fixed toolchain + libc, so they can be committed to the repo
and enforced in CI. This script reads the `summary.json` files iai-callgrind
writes under `target/iai/`, compares each benchmark's instruction count (`Ir`)
against the baseline stored next to the benchmark, and fails on a regression
beyond the tolerance.

Usage:
    # after `cargo bench --bench <iai bench> -- --save-summary=json`
    python3 crates/tuika/benches/check_iai.py            # check (exit 1 on regression)
    python3 crates/tuika/benches/check_iai.py --update   # (re)write the baselines

The baseline lives at `<crate>/benches/iai-baseline.json`. Regenerate it (with
--update) whenever a change legitimately shifts instruction counts — a renderer
change, a dependency bump, or a toolchain upgrade — and commit it alongside the
code change, the way you would a snapshot test.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

BASELINE_NAME = "iai-baseline.json"
DEFAULT_TOLERANCE_PCT = 2.0


def find_repo_root() -> Path:
    """Workspace root — the nearest ancestor with a Cargo.lock. Lets this script
    live anywhere in the tree (it sits next to the tuika benches it checks)."""
    for parent in Path(__file__).resolve().parents:
        if (parent / "Cargo.lock").is_file():
            return parent
    return Path(__file__).resolve().parent


REPO_ROOT = find_repo_root()
IAI_DIR = REPO_ROOT / "target" / "iai"


def extract_current(metrics: dict) -> int:
    """Pull the current-run instruction count from an iai metrics value.

    iai-callgrind encodes it as `{"Left": {"Int": n}}` on a fresh run (no prior
    result to diff against) or `{"Both": [{"Int": current}, {"Int": old}]}` when
    a previous run exists. The current value is the left/first element either
    way.
    """
    if "Both" in metrics:
        return int(metrics["Both"][0]["Int"])
    if "Left" in metrics:
        return int(metrics["Left"]["Int"])
    (only,) = metrics.values()
    return int((only[0] if isinstance(only, list) else only)["Int"])


def collect_measurements() -> dict[Path, dict[str, int]]:
    """Map each crate dir to its {bench_key: instruction_count} from target/iai."""
    if not IAI_DIR.is_dir():
        sys.exit(f"error: {IAI_DIR} not found — run the iai benches first")

    by_crate: dict[Path, dict[str, int]] = {}
    for summary_path in sorted(IAI_DIR.rglob("summary.json")):
        summary = json.loads(summary_path.read_text())
        key = f"{summary['function_name']}.{summary['id']}"
        metrics = summary["profiles"][0]["summaries"]["parts"][0]["metrics_summary"][
            "Callgrind"
        ]["Ir"]["metrics"]
        crate_dir = Path(summary["package_dir"])
        by_crate.setdefault(crate_dir, {})[key] = extract_current(metrics)
    if not by_crate:
        sys.exit(f"error: no summary.json under {IAI_DIR} — did the benches run?")
    return by_crate


def baseline_path(crate_dir: Path) -> Path:
    return crate_dir / "benches" / BASELINE_NAME


def load_baseline(crate_dir: Path) -> dict | None:
    path = baseline_path(crate_dir)
    if not path.is_file():
        return None
    return json.loads(path.read_text())


def write_baseline(crate_dir: Path, benchmarks: dict[str, int]) -> None:
    path = baseline_path(crate_dir)
    doc = {
        "_comment": (
            "iai-callgrind instruction-count baseline. Deterministic and "
            "machine-independent for a fixed toolchain + libc. Regenerate with "
            "`python3 crates/tuika/benches/check_iai.py --update` and commit alongside the "
            "code change that shifted the counts."
        ),
        "tolerance_pct": DEFAULT_TOLERANCE_PCT,
        "benchmarks": dict(sorted(benchmarks.items())),
    }
    path.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"wrote {path.relative_to(REPO_ROOT)} ({len(benchmarks)} benchmarks)")


def rel(crate_dir: Path) -> str:
    try:
        return str(crate_dir.relative_to(REPO_ROOT))
    except ValueError:
        return str(crate_dir)


def check(by_crate: dict[Path, dict[str, int]]) -> int:
    failed = False
    for crate_dir, measured in sorted(by_crate.items(), key=lambda kv: str(kv[0])):
        baseline = load_baseline(crate_dir)
        if baseline is None:
            print(
                f"error: no baseline for {rel(crate_dir)} — run "
                f"`python3 crates/tuika/benches/check_iai.py --update` and commit it"
            )
            failed = True
            continue
        tol = float(baseline.get("tolerance_pct", DEFAULT_TOLERANCE_PCT))
        expected = baseline.get("benchmarks", {})
        print(f"\n{rel(crate_dir)}  (tolerance ±{tol:g}%)")
        print(f"  {'benchmark':<28} {'baseline':>14} {'current':>14} {'diff':>9}")
        for key in sorted(set(expected) | set(measured)):
            base = expected.get(key)
            cur = measured.get(key)
            if base is None:
                print(f"  {key:<28} {'—':>14} {cur:>14,} {'NEW':>9}  (unbaselined)")
                failed = True
                continue
            if cur is None:
                print(f"  {key:<28} {base:>14,} {'—':>14} {'MISSING':>9}")
                failed = True
                continue
            diff_pct = (cur - base) / base * 100 if base else 0.0
            status = "ok"
            if diff_pct > tol:
                status = "REGRESSED"
                failed = True
            elif diff_pct < -tol:
                status = "improved"  # not a failure; baseline is stale
            print(f"  {key:<28} {base:>14,} {cur:>14,} {diff_pct:>+8.3f}%  {status}")
    if failed:
        print(
            "\nFAILED: instruction counts regressed or the baseline is out of "
            "date.\nIf the change is intentional, run "
            "`python3 crates/tuika/benches/check_iai.py --update` and commit the baseline."
        )
        return 1
    print("\nOK: all benchmarks within tolerance of the committed baseline.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--update",
        action="store_true",
        help="rewrite the committed baselines from the current run",
    )
    args = parser.parse_args()

    by_crate = collect_measurements()
    if args.update:
        for crate_dir, measured in by_crate.items():
            write_baseline(crate_dir, measured)
        return 0
    return check(by_crate)


if __name__ == "__main__":
    raise SystemExit(main())

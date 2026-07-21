#!/usr/bin/env bash
#
# Regenerate the tuika component demo GIFs under crates/tuika/docs/demos/.
#
# The scene registry in crates/tuika/examples/demo.rs is the single source of
# truth. This script asks the example to emit one VHS tape per scene into a temp
# dir (tapes are generated, not committed), records each, and verifies the
# result. GIFs are recorded at 2x pixel density and displayed at half width, so
# they stay crisp on HiDPI screens.
#
# Requirements: vhs (https://github.com/charmbracelet/vhs), which needs ttyd and
# ffmpeg on PATH. Run from anywhere:
#   crates/tuika/scripts/gen-tuika-demos.sh [scene ...]   (no args = all).

set -euo pipefail

crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "${crate_dir}/../.." && pwd)"
cd "${repo_root}"

if ! command -v vhs >/dev/null 2>&1; then
  echo "error: vhs not found on PATH (see https://github.com/charmbracelet/vhs)" >&2
  exit 1
fi

echo "Building the demo example…"
cargo build -q -p tuika --example demo

tapes_dir="$(mktemp -d)"
trap 'rm -rf "${tapes_dir}"' EXIT

echo "Emitting tapes…"
cargo run -q -p tuika --example demo -- tapes "${tapes_dir}"

for tape in "${tapes_dir}"/*.tape; do
  name="$(basename "${tape}" .tape)"
  if [[ $# -gt 0 ]] && ! printf '%s\n' "$@" | grep -qxF "${name}"; then
    continue
  fi
  echo "Recording ${name}…"
  # Tape Output paths are relative to the crate dir.
  (cd "${crate_dir}" && vhs "${tape}")
done

echo "Verifying gallery assets…"
cargo run -q -p tuika --example demo -- check

echo "Done. GIFs written to crates/tuika/docs/demos/"

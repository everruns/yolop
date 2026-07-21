#!/usr/bin/env bash
#
# Regenerate the component demo GIFs under crates/tuika/docs/demos/.
#
# Requirements: vhs (https://github.com/charmbracelet/vhs), which in turn needs
# ttyd and ffmpeg on PATH. Install with `brew install vhs` (macOS) or see the
# VHS README for Linux.
#
# Each GIF is recorded from the matching tape in docs/tapes/, which launches the
# `demo` example against the freshly built binary. Run from anywhere; the script
# cds to the repository root so the tape paths resolve.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../../.." && pwd)"
cd "${repo_root}"

if ! command -v vhs >/dev/null 2>&1; then
  echo "error: vhs not found on PATH (see https://github.com/charmbracelet/vhs)" >&2
  exit 1
fi

echo "Building the demo example…"
cargo build -p tuika --example demo

tapes=("${script_dir}"/tapes/*.tape)
if [[ "${1:-}" != "" ]]; then
  # Optional filter: `generate.sh spinner progress_bar` records just those.
  tapes=()
  for name in "$@"; do
    tapes+=("${script_dir}/tapes/${name}.tape")
  done
fi

for tape in "${tapes[@]}"; do
  echo "Recording $(basename "${tape}" .tape)…"
  vhs "${tape}"
done

echo "Done. GIFs written to crates/tuika/docs/demos/"

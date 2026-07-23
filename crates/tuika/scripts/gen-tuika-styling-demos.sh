#!/usr/bin/env bash
#
# Regenerate the stylesheet demo GIFs under crates/tuika/docs/styling/styling-*.gif.
#
# Each GIF is the shared scene from crates/tuika/examples/styling.rs — one
# markdown block plus panels — painted under a different `tuika::StyleSheet`, so
# the set is an honest side-by-side of what one central styling policy does to
# real components. `styling-cycle.gif` swaps the sheet live so the whole tree
# visibly restyles at once. The variant list is the single source of truth in
# the example's `variants()`; the tapes are generated here, not committed.
#
# Requirements: vhs (https://github.com/charmbracelet/vhs), which needs ttyd and
# ffmpeg on PATH. Run from anywhere:
#   crates/tuika/scripts/gen-tuika-styling-demos.sh [variant ...]   (no args = all).

set -euo pipefail

crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "${crate_dir}/../.." && pwd)"
cd "${repo_root}"

if ! command -v vhs >/dev/null 2>&1; then
  echo "error: vhs not found on PATH (see https://github.com/charmbracelet/vhs)" >&2
  exit 1
fi

echo "Building the styling example…"
cargo build -q -p tuika --example styling
bin="${repo_root}/target/debug/examples/styling"
out_dir="${crate_dir}/docs/styling"
mkdir -p "${out_dir}"

bg="$("${bin}" bg)"
fg="#ebe6e6"

# Each held variant plus one live-cycling capture. `run <name>` holds a sheet;
# `run` alone cycles through every variant.
records=("default:run default" "vivid:run vivid" "mono:run mono" "cycle:run")

record() {
  local name="$1" cmd="$2" sleep_for="$3"
  local tape
  tape="$(mktemp --suffix=.tape)"
  cat >"${tape}" <<EOF
Output "${out_dir}/styling-${name}.gif"

Set Shell bash
Set FontSize 24
Set Width 1480
Set Height 940
Set Padding 28
Set WindowBar Colorful
Set Theme { "background": "${bg}", "foreground": "${fg}" }
Set Framerate 24

Hide
Type "TERM=xterm-256color ${bin} ${cmd}"
Enter
Sleep 900ms
Show
Sleep ${sleep_for}
EOF
  echo "Recording styling-${name}.gif…"
  vhs "${tape}"
  rm -f "${tape}"
}

for entry in "${records[@]}"; do
  name="${entry%%:*}"
  cmd="${entry#*:}"
  if [[ $# -gt 0 ]] && ! printf '%s\n' "$@" | grep -qxF "${name}"; then
    continue
  fi
  # The cycling capture needs long enough to show every sheet; held ones are short.
  if [[ "${name}" == "cycle" ]]; then
    record "${name}" "${cmd}" "9s"
  else
    record "${name}" "${cmd}" "4s"
  fi
done

echo "Done. GIFs written to ${out_dir}/styling-*.gif"

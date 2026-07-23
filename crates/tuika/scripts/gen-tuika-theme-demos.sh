#!/usr/bin/env bash
#
# Regenerate the per-theme demo GIFs under crates/tuika/docs/themes/theme-*.gif.
#
# Each GIF is the shared gallery scene from crates/tuika/examples/screenshot.rs
# (the same `scene()` behind docs/hero.gif) animated in one bundled theme, so the
# set is an honest side-by-side of what each palette does to real components. The
# theme list is the single source of truth in `tuika::themes::PRESETS`; the tapes
# are generated here, not committed.
#
# Requirements: vhs (https://github.com/charmbracelet/vhs), which needs ttyd and
# ffmpeg on PATH. Run from anywhere:
#   crates/tuika/scripts/gen-tuika-theme-demos.sh [theme ...]   (no args = all).

set -euo pipefail

crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "${crate_dir}/../.." && pwd)"
cd "${repo_root}"

if ! command -v vhs >/dev/null 2>&1; then
  echo "error: vhs not found on PATH (see https://github.com/charmbracelet/vhs)" >&2
  exit 1
fi

echo "Building the screenshot example…"
cargo build -q -p tuika --example screenshot
bin="${repo_root}/target/debug/examples/screenshot"

# The bundled theme ids, straight from the crate so the script never drifts.
themes=(solarized-dark solarized-light gruvbox-dark light dracula)

for name in "${themes[@]}"; do
  if [[ $# -gt 0 ]] && ! printf '%s\n' "$@" | grep -qxF "${name}"; then
    continue
  fi
  bg="$("${bin}" bg "${name}")"
  fg="#ebe6e6"
  case "${name}" in
    solarized-light | light) fg="#1e1e1e" ;;
  esac
  tape="$(mktemp --suffix=.tape)"
  # The scene fills the terminal; window sized to yield ~92×30 cells. Padding
  # background matches the theme so VHS's window bar blends into the app.
  cat >"${tape}" <<EOF
Output "${crate_dir}/docs/themes/theme-${name}.gif"

Set Shell bash
Set FontSize 26
Set Width 1500
Set Height 1040
Set Padding 30
Set WindowBar Colorful
Set Theme { "background": "${bg}", "foreground": "${fg}" }
Set Framerate 24

Hide
Type "TERM=xterm-256color ${bin} run ${name}"
Enter
Sleep 900ms
Show
Sleep 6s
EOF
  echo "Recording theme-${name}.gif (bg ${bg})…"
  vhs "${tape}"
  rm -f "${tape}"
done

echo "Done. GIFs written to crates/tuika/docs/themes/theme-*.gif"

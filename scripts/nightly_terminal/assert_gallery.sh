#!/usr/bin/env bash
# Assert that a captured terminal screen shows `yolop tuika-gallery` rendered
# correctly. Used by the nightly cross-terminal workflow's text-capture legs
# (tmux, kitty) to turn a real emulator's on-screen text into a pass/fail gate.
#
# Usage: assert_gallery.sh <capture-file>
set -euo pipefail

capture="${1:?usage: assert_gallery.sh <capture-file>}"
if [[ ! -s "$capture" ]]; then
  echo "::error::capture file '$capture' is empty — the terminal produced no output"
  exit 1
fi

fail=0
need() {
  if ! grep -qF -- "$1" "$capture"; then
    echo "::error::expected gallery text not found: $1"
    fail=1
  fi
}

# Box titles, the Braille spinner's label, and the footer URL must all be on
# screen — proof the layout, components, and hyperlink footer painted.
need "spinners"
need "progress"
need "Braille"
need "https://github.com/everruns/yolop"

# A Braille glyph (U+2800 block) must actually be painted, not just its label —
# the "Braille glyphs" row of the manual matrix, checked in a real emulator.
# Match the block's UTF-8 encoding (U+2800..U+28FF => E2 A0..A3 80..BF) in the C
# locale so it is portable across GNU and BSD grep without a UTF-8 locale.
if ! LC_ALL=C grep -qE "$(printf '\xe2[\xa0-\xa3][\x80-\xbf]')" "$capture"; then
  echo "::error::no Braille glyph (U+2800 block) found on screen"
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  echo "----- capture -----"
  cat "$capture"
  exit 1
fi
echo "gallery rendered correctly in the captured terminal output"

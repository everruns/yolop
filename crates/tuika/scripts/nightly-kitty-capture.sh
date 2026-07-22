#!/usr/bin/env bash
# Best-effort: drive `yolop tuika-gallery` inside kitty (a GPU terminal) and
# capture the on-screen text via kitty's remote control, plus a screenshot for
# human inspection. Meant to be wrapped in `xvfb-run` so kitty has a display:
#
#   xvfb-run -a --server-args="-screen 0 1200x800x24" \
#     bash crates/tuika/scripts/nightly-kitty-capture.sh "$PWD/target/debug/yolop"
#
# Writes capture.txt (screen text) and kitty.png (screenshot) to the cwd. Never
# hard-fails on capture problems — the nightly leg is `continue-on-error` and
# assertions run separately over capture.txt.
set -x
bin="${1:?usage: kitty_capture.sh <yolop-binary>}"
sock="unix:/tmp/kitty-$$"

# allow_remote_control lets us read the screen and send the quit key; software
# GL keeps kitty off a real GPU under Xvfb.
LIBGL_ALWAYS_SOFTWARE=1 kitty \
  -o allow_remote_control=yes \
  -o enable_audio_bell=no \
  --listen-on "$sock" \
  bash -lc "YOLOP_HYPERLINKS=1 '$bin' tuika-gallery" &
kpid=$!

sleep 6
kitty @ --to "$sock" get-text --extent screen > capture.txt 2>/dev/null || true

# Screenshot the virtual root, whichever tool the image is captured with.
if command -v import >/dev/null 2>&1; then
  import -window root kitty.png 2>/dev/null || true
elif command -v xwd >/dev/null 2>&1 && command -v convert >/dev/null 2>&1; then
  xwd -root 2>/dev/null | convert xwd:- kitty.png 2>/dev/null || true
fi

kitty @ --to "$sock" send-text q 2>/dev/null || true
sleep 1
kill "$kpid" 2>/dev/null || true
wait "$kpid" 2>/dev/null || true

echo "----- kitty capture -----"
cat capture.txt 2>/dev/null || echo "(no capture)"

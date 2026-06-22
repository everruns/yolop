#!/usr/bin/env bash
# Named recipe: the cross-agent comparison — one cell per agent flavor on a
# single SWE-bench instance, grouped by agent and saved under results/<run_id>/.
#
# The target set is the `compare` preset in mira.toml; this just adds the
# instance, --group-by agent, and --save.
#
#   ./compare.sh astropy__astropy-12907                 # the compare preset
#   ./compare.sh django__django-11099 -j 4              # + extra mira flags
#   COMPARE_TARGETS=codex,pi,claude-code-sonnet-4.5 ./compare.sh <id>   # override the set
#
# Needs: mira on PATH, the agent CLIs (claude/codex/pi) installed, Docker up,
# and provider keys (wrapped in `doppler run --`). MIRA overrides the binary.
set -euo pipefail

INSTANCE="${1:?usage: compare.sh <instance_id> [extra mira args...]}"; shift || true
MIRA="${MIRA:-mira}"

# Default to the named preset; COMPARE_TARGETS overrides it with an explicit set.
if [ -n "${COMPARE_TARGETS:-}" ]; then
  SELECT=(--targets "$COMPARE_TARGETS")
else
  SELECT=(--preset compare)
fi

cd "$(dirname "$0")"   # so mira.toml (presets + results dir) is found
exec doppler run -- "$MIRA" --cmd "uv run swebench_verified.py" \
  run "$INSTANCE" "${SELECT[@]}" --group-by agent --save "$@"

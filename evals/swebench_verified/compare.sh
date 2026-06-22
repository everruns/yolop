#!/usr/bin/env bash
# Named recipe: the cross-agent comparison — one cell per agent flavor on a
# single SWE-bench instance, grouped by agent and saved under results/<run_id>/.
#
#   ./compare.sh astropy__astropy-12907                 # the default 8-config set
#   ./compare.sh django__django-11099 -j 4              # + extra mira flags
#   COMPARE_MODELS=codex,pi,claude-code ./compare.sh <id>   # override the set
#
# Needs: mira on PATH, the agent CLIs (claude/codex/pi) installed, Docker up,
# and provider keys (wrapped in `doppler run --`). MIRA overrides the binary.
set -euo pipefail

INSTANCE="${1:?usage: compare.sh <instance_id> [extra mira args...]}"; shift || true

# The comparison set: yolop across providers/effort + the three other agents,
# with Opus 4.8 on both yolop and claude-code. Override via COMPARE_MODELS.
MODELS="${COMPARE_MODELS:-anthropic-sonnet,openai-default,openai-high,anthropic-opus,claude-code,claude-code-opus,codex,pi}"
MIRA="${MIRA:-mira}"

cd "$(dirname "$0")"   # so mira.toml is found and --save lands in ./results
exec doppler run -- "$MIRA" --cmd "uv run swebench_verified.py" \
  run "$INSTANCE" --models "$MODELS" --group-by agent --save "$@"

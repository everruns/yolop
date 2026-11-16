#!/usr/bin/env bash
# Scope the dedicated eval key to this child only; never fall back to shared keys.
set +x
set -euo pipefail
: "${OPENAI_EVAL_API_KEY:?Dedicated OPENAI_EVAL_API_KEY is required}"
export OPENAI_API_KEY="${OPENAI_EVAL_API_KEY}"
unset OPENAI_EVAL_API_KEY
exec "$@"

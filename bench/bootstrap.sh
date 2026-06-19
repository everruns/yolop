#!/usr/bin/env bash
# Bootstrap the yoloeval benchmark environment.
#
# Installs everything needed to run the matrix: the Python harness venv, a yolop
# release build, and the three external agent CLIs (claude-code, codex, pi)
# pinned to the versions we validate against. Re-runnable / idempotent.
#
#   bench/bootstrap.sh            # full setup
#   AGENTS=codex,pi bench/bootstrap.sh   # only those agent CLIs (+ venv/yolop)
#
# Pinned agent versions are also recorded in every result's `agent` block, so a
# committed result is always traceable to the binary that produced it. Bump these
# deliberately when upgrading.
set -euo pipefail

CLAUDE_CODE_VERSION="${CLAUDE_CODE_VERSION:-2.1.181}"
CODEX_VERSION="${CODEX_VERSION:-0.141.0}"
PI_VERSION="${PI_VERSION:-0.79.7}"

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"
AGENTS="${AGENTS:-claude-code,codex,pi}"

have() { command -v "$1" >/dev/null 2>&1; }
note() { printf '\n[bootstrap] %s\n' "$*"; }

# --- Python harness venv ---------------------------------------------------- #
note "Python venv + requirements"
if [ ! -d "$BENCH_DIR/.venv" ]; then
  python3 -m venv "$BENCH_DIR/.venv"
fi
"$BENCH_DIR/.venv/bin/pip" install -q --upgrade pip
"$BENCH_DIR/.venv/bin/pip" install -q -r "$BENCH_DIR/requirements.txt"

# --- yolop release build ---------------------------------------------------- #
note "yolop release build"
if have cargo; then
  ( cd "$REPO_DIR" && cargo build --release --quiet )
else
  echo "  ! cargo not found; skipping yolop build (install Rust to benchmark yolop)"
fi

# --- External agent CLIs (npm, pinned) -------------------------------------- #
npm_install() {  # name  package@version  binary
  case ",$AGENTS," in *",$1,"*) ;; *) return 0 ;; esac
  if ! have npm; then echo "  ! npm not found; cannot install $1"; return 0; fi
  note "$1 ($2)"
  npm install -g "$2"
  have "$3" && "$3" --version 2>&1 | head -1 || echo "  ! $3 not on PATH after install"
}
npm_install claude-code "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" claude
npm_install codex       "@openai/codex@${CODEX_VERSION}"                    codex
npm_install pi          "@earendil-works/pi-coding-agent@${PI_VERSION}"     pi

# --- Post-install notes ----------------------------------------------------- #
cat <<'EOF'

[bootstrap] Done. Before running the matrix:

  Provider keys (export in the environment, e.g. via `doppler run --`):
    OPENAI_API_KEY        # yolop openai, codex, pi (gpt-5.5)
    ANTHROPIC_API_KEY     # yolop anthropic, claude-code
    OPENROUTER_API_KEY    # yolop openrouter configs (nvidia / openai routing)

  codex auth: codex ignores OPENAI_API_KEY for requests; log in once with
    printenv OPENAI_API_KEY | codex login --with-api-key

  Docker daemon must be running for SWE-bench evaluation.

  Smoke test:  cd bench && .venv/bin/python -m yoloeval run --config llmsim --limit 1 --no-eval
EOF

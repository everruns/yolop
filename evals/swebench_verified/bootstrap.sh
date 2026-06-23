#!/usr/bin/env bash
# Bootstrap the swebench_verified benchmark environment.
#
# Installs everything needed to run the matrix: pre-warms the uv dependency
# cache for the single-file study, builds yolop release, installs the mira host
# CLI, and installs the external agent CLIs (claude-code, codex, pi) pinned to
# the versions we validate against. Re-runnable / idempotent.
#
#   evals/swebench_verified/bootstrap.sh          # full setup
#   AGENTS=codex,pi evals/swebench_verified/bootstrap.sh   # only those agent CLIs
#
# Pinned agent versions are also recorded in every result's `agent` block, so a
# committed result is always traceable to the binary that produced it. Bump these
# deliberately when upgrading.
set -euo pipefail

CLAUDE_CODE_VERSION="${CLAUDE_CODE_VERSION:-2.1.181}"
CODEX_VERSION="${CODEX_VERSION:-0.141.0}"
PI_VERSION="${PI_VERSION:-0.79.7}"

STUDY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$STUDY_DIR/../.." && pwd)"
AGENTS="${AGENTS:-claude-code,codex,pi}"

have() { command -v "$1" >/dev/null 2>&1; }
note() { printf '\n[bootstrap] %s\n' "$*"; }

# --- Python deps (uv, inline) ----------------------------------------------- #
# The study is a single file with PEP 723 inline deps; `uv run swebench_verified.py`
# builds the ephemeral env on first use. Pre-warm it here so the first real run
# isn't slowed by dependency installation.
note "Python deps via uv (inline PEP 723)"
if have uv; then
  ( cd "$STUDY_DIR" && echo '{"id":1,"method":"initialize"}' | uv run swebench_verified.py >/dev/null )
else
  echo "  ! uv not found; install it: https://docs.astral.sh/uv/  (curl -LsSf https://astral.sh/uv/install.sh | sh)"
fi

# --- yolop release build ---------------------------------------------------- #
note "yolop release build"
if have cargo; then
  ( cd "$REPO_DIR" && cargo build --release --quiet )
else
  echo "  ! cargo not found; skipping yolop build (install Rust to benchmark yolop)"
fi

# --- Mira host CLI ---------------------------------------------------------- #
# The study is driven by `mira` (matrix, selection, checkpoints, reporting).
note "mira host CLI"
if have mira; then
  mira --version 2>&1 | head -1 || true
elif have brew; then
  brew install everruns/tap/mira || echo "  ! mira install failed; install manually"
elif have cargo-binstall || have cargo; then
  # Prefer a prebuilt binary (seconds) over compiling mira-cli from source
  # (minutes). mira-cli ships no [package.metadata.binstall], and its release
  # assets are named after the binary (mira-<target>.tar.gz, tag v<version>),
  # not the crate, so cargo-binstall needs explicit URL/layout overrides.
  if have cargo-binstall && cargo binstall -y mira-cli \
       --pkg-url "{ repo }/releases/download/v{ version }/mira-{ target }.tar.gz" \
       --pkg-fmt tgz --bin-dir "mira{ binary-ext }"; then
    :
  else
    cargo install mira-cli --locked \
      || echo "  ! mira install failed; install manually"
  fi
else
  echo "  ! no brew/cargo; install mira manually (brew install everruns/tap/mira)"
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

  Run from this study directory (so mira.toml is found and runs save here):
    cd evals/swebench_verified

  Smoke test (offline, skips Docker):
    SWEBENCH_NO_EVAL=1 mira --cmd "uv run swebench_verified.py" \
      run astropy__astropy-12907 --models llmsim
EOF

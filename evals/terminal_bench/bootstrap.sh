#!/usr/bin/env bash
# Bootstrap the terminal_bench benchmark environment.
#
# Installs everything needed to run the matrix: pre-warms the uv dependency
# cache for the single-file study, builds the static musl yolop binary that gets
# uploaded into task containers, installs the mira host CLI, and pre-downloads
# the Terminal-Bench 2.1 task tree. Re-runnable / idempotent.
#
#   evals/terminal_bench/bootstrap.sh
set -euo pipefail

STUDY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$STUDY_DIR/../.." && pwd)"
MUSL_TARGET="x86_64-unknown-linux-musl"

have() { command -v "$1" >/dev/null 2>&1; }
note() { printf '\n[bootstrap] %s\n' "$*"; }

# --- Python deps (uv, inline) ----------------------------------------------- #
# The study is a single file with PEP 723 inline deps (mira-eval + harbor);
# `uv run terminal_bench.py` builds the ephemeral env on first use. Pre-warm it
# so the first real run isn't slowed by dependency installation.
note "Python deps via uv (inline PEP 723)"
if have uv; then
  ( cd "$STUDY_DIR" && echo '{"id":1,"method":"initialize"}' | uv run terminal_bench.py >/dev/null )
else
  echo "  ! uv not found; install it: https://docs.astral.sh/uv/  (curl -LsSf https://astral.sh/uv/install.sh | sh)"
fi

# --- yolop musl build ------------------------------------------------------- #
# Harbor always runs the agent inside the task's container, and Terminal-Bench
# images are mostly Debian bookworm (glibc 2.36) — older than a typical dev box,
# so a host glibc build will not start there. A static musl binary runs in every
# image, which is what the adapter uploads.
note "yolop static build ($MUSL_TARGET)"
if have cargo; then
  if rustup target list --installed 2>/dev/null | grep -q "$MUSL_TARGET" \
     || rustup target add "$MUSL_TARGET" 2>/dev/null; then
    if have musl-gcc || { have apt-get && sudo apt-get install -y musl-tools >/dev/null 2>&1; }; then
      ( cd "$REPO_DIR" && cargo build --release --quiet --target "$MUSL_TARGET" )
    else
      echo "  ! musl-tools not available; install it, or point TB_YOLOP_BIN at a binary"
      echo "    built for the task image ABI"
    fi
  fi
else
  echo "  ! cargo not found; skipping yolop build (install Rust to benchmark yolop)"
fi

# --- Mira host CLI ---------------------------------------------------------- #
# Require a MINIMUM version, not merely any `mira` on PATH: the 0.3.0 preset
# `samples` key in mira.toml is silently ignored by older hosts, which then run
# every task instead of the pinned ones. Keep this in lockstep with the
# `mira-eval` SDK pinned in the study's PEP 723 deps.
MIRA_MIN_VERSION="${MIRA_MIN_VERSION:-0.3.0}"
note "mira host CLI (>= $MIRA_MIN_VERSION)"
mira_version() { mira --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }
version_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }  # $1 >= $2
if have mira && version_ge "$(mira_version)" "$MIRA_MIN_VERSION"; then
  mira --version 2>&1 | head -1 || true
elif have brew; then
  brew install everruns/tap/mira || brew upgrade everruns/tap/mira \
    || echo "  ! mira install failed; install/upgrade manually"
elif have cargo-binstall && cargo binstall -y --force mira-cli; then
  :
elif have cargo; then
  cargo install mira-cli --locked --force || echo "  ! mira install failed; install manually"
else
  echo "  ! no brew/cargo; install mira >= $MIRA_MIN_VERSION manually (brew install everruns/tap/mira)"
fi

# --- Terminal-Bench 2.1 task tree ------------------------------------------- #
# The study downloads this on first use too; doing it here keeps the first real
# run from paying for it (and surfaces a Harbor Hub outage before a paid run).
note "Terminal-Bench 2.1 tasks"
if have uv; then
  ( cd "$STUDY_DIR" && uv run --with harbor harbor dataset download \
      terminal-bench/terminal-bench-2-1 -o .cache/dataset >/dev/null \
    && echo "  $(find .cache/dataset/terminal-bench-2-1 -maxdepth 1 -mindepth 1 -type d | wc -l) tasks cached" ) \
    || echo "  ! dataset download failed; the study will retry on first run"
fi

cat <<'EOF'

[bootstrap] Done. Before running the matrix:

  Provider keys (export in the environment, e.g. via `doppler run --`):
    OPENAI_API_KEY        # yolop openai configs, codex, terminus-2
    ANTHROPIC_API_KEY     # yolop anthropic configs, claude-code

  Docker daemon must be running (every task is a container).

  Run from this study directory (so mira.toml is found, its default_launcher
  drives the study, and runs save here):
    cd evals/terminal_bench

  Smoke test (offline, no API key — exercises upload/run/verify, solves nothing):
    mira run --samples fix-git --targets llmsim --dry-run

  Cheap real run, capped at $5 across the whole run:
    TB_MAX_COST_USD=5 doppler run -- mira run --preset smoke
EOF

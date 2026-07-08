#!/usr/bin/env bash
# Bootstrap for the lsp_integration study: the yolop binary under test plus
# the language servers the samples need. Each step is idempotent and
# best-effort — a missing server only N/A's the cases that need it.
set -uo pipefail

cd "$(dirname "$0")"
repo_root="$(cd ../.. && pwd)"

echo "==> yolop under test (release build)"
(cd "$repo_root" && cargo build --release)

echo "==> mira host CLI"
if ! command -v mira >/dev/null 2>&1; then
    cargo install mira-cli --locked || echo "WARN: mira install failed — brew install everruns/tap/mira"
fi

echo "==> rust-analyzer (diagnose-type-mismatch-rust)"
if ! command -v rust-analyzer >/dev/null 2>&1; then
    rustup component add rust-analyzer || echo "WARN: rust-analyzer unavailable; its cases will report infra-N/A"
fi

echo "==> pyright (python samples)"
if ! command -v pyright-langserver >/dev/null 2>&1; then
    # The npm package ships pyright-langserver; `uv tool install pyright` or
    # `pip install pyright` work too (both wrap the same distribution).
    if command -v npm >/dev/null 2>&1; then
        npm install -g pyright || echo "WARN: pyright install failed; python cases will report infra-N/A"
    elif command -v uv >/dev/null 2>&1; then
        uv tool install pyright || echo "WARN: pyright install failed; python cases will report infra-N/A"
    else
        echo "WARN: no npm/uv found to install pyright; python cases will report infra-N/A"
    fi
fi

echo "==> done. Try: doppler run -- mira run --preset smoke --group-by harness"

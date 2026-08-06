#!/usr/bin/env bash
# Live reproduction helpers for MCP activation / OAuth issues.
# Writes evidence under /opt/cursor/artifacts/mcp-activation-repro/
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="/opt/cursor/artifacts/mcp-activation-repro"
mkdir -p "$OUT"

echo "== repro: open_browser path does not surface URL to caller ==" | tee "$OUT/01-open-browser.txt"
# Prove mcp_login only prints "opening browser" and delegates URL open inside login().
rg -n "opening browser|open_browser|auth_url" \
  "$ROOT/src/tui/mod.rs" \
  "$ROOT/src/auth/mcp_oauth_login.rs" \
  "$ROOT/src/auth/oauth_flow.rs" | tee -a "$OUT/01-open-browser.txt"

echo | tee -a "$OUT/01-open-browser.txt"
echo "Expected gap: no transcript line carries the authorize URL before wait_for_callback." \
  | tee -a "$OUT/01-open-browser.txt"

echo "== repro: /tools uses frozen startup.tool_names ==" | tee "$OUT/02-tools-frozen.txt"
rg -n "ShowTools|startup.tool_names" "$ROOT/src/tui/mod.rs" | tee -a "$OUT/02-tools-frozen.txt"

echo "== repro: run_command queues without host output ==" | tee "$OUT/03-queued.txt"
rg -n "queued for the interactive terminal host|ManageMcp" \
  "$ROOT/src/capabilities/client_commands.rs" \
  "$ROOT/src/tui/host_ui.rs" | tee -a "$OUT/03-queued.txt"

echo "== repro: model/effort open overlays; mcp does not ==" | tee "$OUT/04-no-overlay.txt"
rg -n "OpenModelOverlay|OpenEffortOverlay|ManageMcp" \
  "$ROOT/src/capabilities/client_commands.rs" \
  "$ROOT/src/tui/host_ui.rs" \
  "$ROOT/src/tui/mod.rs" | head -40 | tee -a "$OUT/04-no-overlay.txt"

echo "== running ignored repro tests (expect failures) ==" | tee "$OUT/05-cargo-ignored.txt"
cd "$ROOT"
cargo test --all-features repro_ -- --ignored --nocapture 2>&1 | tee -a "$OUT/05-cargo-ignored.txt" || true

echo "Wrote evidence to $OUT"

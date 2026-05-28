#!/usr/bin/env bash
# SessionStart hook: ensure git identity is the real human user, not the
# Claude Code agent.
#
# Without this, commits written inside a Claude Code session land with
# author = "Claude <noreply@anthropic.com>" (the agent's environment
# default), which AGENTS.md forbids ("Commit attribution must be a real
# human user"). GitHub's squash-merge then propagates that author as a
# Co-authored-by trailer on the squash commit on main.
#
# This script runs once per session start. It only overrides the identity
# when the current value is missing or matches a known agent pattern —
# real human identities are left alone, so a contributor running yolop
# locally with their own git config is unaffected.

set -euo pipefail

# No-op outside a git repo (e.g. someone running yolop in /tmp).
git rev-parse --git-dir >/dev/null 2>&1 || exit 0

REAL_NAME="Mykhailo Chalyi"
REAL_EMAIL="mike@chaliy.name"

# Prefer the repo-local config, fall back to whatever git would resolve.
current_email="$(git config --local user.email 2>/dev/null || true)"
if [ -z "$current_email" ]; then
    current_email="$(git config user.email 2>/dev/null || true)"
fi

is_agent_identity() {
    case "$1" in
        "" \
        | noreply@anthropic.com \
        | *@anthropic.com \
        | claude@* \
        | noreply@github.com \
        | *bot@users.noreply.github.com)
            return 0
            ;;
        *) return 1 ;;
    esac
}

if is_agent_identity "$current_email"; then
    git config --local user.name "$REAL_NAME"
    git config --local user.email "$REAL_EMAIL"
    echo "[claude] git identity set to ${REAL_NAME} <${REAL_EMAIL}>" >&2
fi

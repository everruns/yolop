"""Tests for the local Claude Code compatibility adapter."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harbor_claude import ClaudeCodeCompatibility


class TestClaudeCodeCompatibility(unittest.IsolatedAsyncioTestCase):
    async def test_installs_with_nvm_and_exposes_stable_binary_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = ClaudeCodeCompatibility(Path(tmp), version="2.1.260")
            agent._installed_claude_satisfies_version = mock.AsyncMock(return_value=False)
            agent.ensure_system_dependencies = mock.AsyncMock()
            agent.exec_as_agent = mock.AsyncMock()

            await agent.install(mock.AsyncMock())

        agent.ensure_system_dependencies.assert_awaited_once_with(
            mock.ANY, ("curl", "bash", "ripgrep", "procps")
        )
        install_call = agent.exec_as_agent.await_args
        command = install_call.kwargs["command"]
        self.assertIn("nvm install 22", command)
        self.assertIn("npm install -g @anthropic-ai/claude-code@2.1.260", command)
        self.assertIn('ln -sf "$(command -v node)" "$HOME/.local/bin/node"', command)
        self.assertIn('ln -sf "$(command -v claude)" "$HOME/.local/bin/claude"', command)

    async def test_quotes_the_configured_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = ClaudeCodeCompatibility(Path(tmp), version="2.1.260; false")
            agent._installed_claude_satisfies_version = mock.AsyncMock(return_value=False)
            agent.ensure_system_dependencies = mock.AsyncMock()
            agent.exec_as_agent = mock.AsyncMock()

            await agent.install(mock.AsyncMock())

        command = agent.exec_as_agent.await_args.kwargs["command"]
        self.assertIn("npm install -g '@anthropic-ai/claude-code@2.1.260; false'", command)

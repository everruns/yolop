"""Tests for the local Codex compatibility adapter."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harbor_codex import CodexCompatibility


class TestCodexCompatibility(unittest.IsolatedAsyncioTestCase):
    def test_uses_a_non_temporary_codex_home(self):
        self.assertEqual(str(CodexCompatibility._REMOTE_CODEX_HOME), "/root/.codex")

    async def test_installs_with_nvm_and_exposes_stable_binary_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = CodexCompatibility(Path(tmp), version="0.104.0")
            agent._installed_codex_satisfies_version = mock.AsyncMock(return_value=False)
            agent.ensure_system_dependencies = mock.AsyncMock()
            agent.exec_as_agent = mock.AsyncMock()
            agent.exec_as_root = mock.AsyncMock()

            await agent.install(mock.AsyncMock())

        agent.ensure_system_dependencies.assert_awaited_once_with(
            mock.ANY, ("curl", "bash", "ripgrep")
        )
        install_command = agent.exec_as_agent.await_args.kwargs["command"]
        self.assertIn("nvm install 22", install_command)
        self.assertIn("npm install -g @openai/codex@0.104.0", install_command)
        link_command = agent.exec_as_root.await_args.kwargs["command"]
        self.assertIn("/usr/local/bin/$bin", link_command)

    async def test_quotes_the_configured_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            agent = CodexCompatibility(Path(tmp), version="0.104.0; false")
            agent._installed_codex_satisfies_version = mock.AsyncMock(return_value=False)
            agent.ensure_system_dependencies = mock.AsyncMock()
            agent.exec_as_agent = mock.AsyncMock()
            agent.exec_as_root = mock.AsyncMock()

            await agent.install(mock.AsyncMock())

        command = agent.exec_as_agent.await_args.kwargs["command"]
        self.assertIn("npm install -g '@openai/codex@0.104.0; false'", command)

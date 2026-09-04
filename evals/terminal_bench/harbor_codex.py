"""Harbor's Codex adapter with a persistent home inside task containers."""

from __future__ import annotations

import shlex
from pathlib import PurePosixPath

from harbor.environments.base import BaseEnvironment
from harbor.agents.installed.codex import Codex


class CodexCompatibility(Codex):
    """Keep Codex's code-mode helper binaries outside the temporary directory."""

    # Harbor's built-in adapter chooses /tmp/codex-home. Codex rejects that
    # location when creating code-mode helper binaries, so all workspace tools
    # fail before the agent can start the task.
    _REMOTE_CODEX_HOME = PurePosixPath("/root/.codex")

    async def install(self, environment: BaseEnvironment) -> None:
        """Install Node with nvm, without Ubuntu's broken npm package chain."""
        if await self._installed_codex_satisfies_version(environment):
            self.logger.debug("Codex is already available at the requested version")
            return

        # Harbor's stock adapter installs the distro `nodejs` and `npm` before
        # nvm replaces them. On Terminal-Bench's Noble image that package set
        # can fail while configuring node-gyp, before Codex itself starts.
        # nvm provides Node and npm, so only its small prerequisites are needed.
        await self.ensure_system_dependencies(environment, ("curl", "bash", "ripgrep"))
        version_spec = f"@{self._version}" if self._version else "@latest"
        package_spec = shlex.quote(f"@openai/codex{version_spec}")
        await self.exec_as_agent(
            environment,
            command=(
                "set -euo pipefail; "
                "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash && "
                'export NVM_DIR="$HOME/.nvm" && '
                '\\. "$NVM_DIR/nvm.sh" && '
                "nvm install 22 && nvm alias default 22 && npm -v && "
                f"npm install -g {package_spec} && codex --version"
            ),
            env={"NVM_NODEJS_ORG_MIRROR": "https://nodejs.org/dist"},
        )
        await self.exec_as_root(
            environment,
            command=(
                "for bin in node codex; do "
                'BIN_PATH="$(which "$bin" 2>/dev/null || true)"; '
                'if [ -n "$BIN_PATH" ] && [ "$BIN_PATH" != "/usr/local/bin/$bin" ]; then '
                'ln -sf "$BIN_PATH" "/usr/local/bin/$bin"; fi; done'
            ),
        )

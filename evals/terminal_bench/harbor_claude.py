"""Harbor's Claude Code adapter with an emulation-friendly installer."""

from __future__ import annotations

import shlex

from harbor.agents.installed.claude_code import ClaudeCode
from harbor.environments.base import BaseEnvironment


class ClaudeCodeCompatibility(ClaudeCode):
    """Install Claude Code through npm instead of its native bootstrap binary."""

    async def install(self, environment: BaseEnvironment) -> None:
        if await self._installed_claude_satisfies_version(environment):
            self.logger.debug("Claude Code is already available at the requested version")
            return

        # Harbor selects Anthropic's native bootstrap on glibc. Terminal-Bench
        # uses x86 images, and that installer takes more than 20 minutes under
        # Apple Silicon emulation. The supported npm package avoids that path.
        await self.ensure_system_dependencies(
            environment, ("curl", "bash", "ripgrep", "procps")
        )
        version_spec = f"@{self._version}" if self._version else ""
        package_spec = shlex.quote(f"@anthropic-ai/claude-code{version_spec}")
        await self.exec_as_agent(
            environment,
            command=(
                "set -euo pipefail; "
                "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash && "
                'export NVM_DIR="$HOME/.nvm" && '
                '\\. "$NVM_DIR/nvm.sh" && '
                "nvm install 22 && nvm alias default 22 && "
                f"npm install -g {package_spec} && "
                'mkdir -p "$HOME/.local/bin" && '
                'ln -sf "$(command -v node)" "$HOME/.local/bin/node" && '
                'ln -sf "$(command -v claude)" "$HOME/.local/bin/claude" && '
                "claude --version"
            ),
            env={"NVM_NODEJS_ORG_MIRROR": "https://nodejs.org/dist"},
        )

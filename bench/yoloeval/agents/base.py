"""Agent abstraction.

An agent receives an :class:`Instance` plus a checked-out working copy of the
repo at ``base_commit`` and must leave its solution as uncommitted changes in
that working tree. The runner captures the diff and the agent's metrics.

This boundary is what lets us benchmark *other* coding agents (aider, claude
code, swe-agent, …) against yolop: implement :meth:`Agent.run` for each.
"""

from __future__ import annotations

import abc
from typing import Any

from ..models import AgentRun, Instance


class Agent(abc.ABC):
    #: Stable short name used in result paths (e.g. ``yolop``).
    name: str

    #: Config metadata recorded with every result (provider, model, …).
    config: dict[str, Any]

    @abc.abstractmethod
    def run(self, instance: Instance, workdir: str, session_dir: str) -> AgentRun:
        """Solve ``instance`` inside ``workdir`` (a git checkout at base_commit).

        ``session_dir`` is a writable scratch dir for raw logs. Implementations
        return the produced patch plus :class:`RunMetrics`. The runner measures
        wall time and computes the git diff, so an agent may leave
        ``AgentRun.patch`` empty and rely on the working tree.
        """

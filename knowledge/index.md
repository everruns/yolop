# Yolop Knowledge

This directory is Yolop's Open Knowledge Format (OKF) bundle: the durable
product, architecture, policy, and development-process memory used by
maintainers and coding agents.

Read this index first, then open only the concepts relevant to the task and
follow their links. Public documentation lives in [`README.md`](../README.md)
and [`docs/`](../docs/); it must not link back into this internal bundle.

## Product direction and interaction

- [Product goal](specs/goal.md) — autonomous completion loops.
- [Conversational control](specs/conversational-control.md) — steering and interruption behavior.
- [Presentation](specs/presentation.md) — terminal-independent presentation semantics.
- [Commands](specs/commands.md) — client command behavior.
- [User ask](specs/user-ask.md) — request tracking and turn-end validation.
- [Mid-turn steering](specs/steering.md) — input received during execution.

## Capabilities and integrations

- [Approval](specs/approval.md) — confirmation for consequential actions.
- [Background execution](specs/background.md) — asynchronous work and monitoring.
- [Checkpointing](specs/checkpointing.md) — session rewind and workspace restoration.
- [Configuration](specs/configuration.md) — schema-driven Yolop settings.
- [Connectors](specs/connectors.md) — remote sandbox integrations.
- [Extensions](specs/extensions.md) — installable capability packages.
- [Hooks](specs/hooks.md) — lifecycle automation.
- [Memory](specs/memory.md) — durable user personalization.
- [Session history](specs/session-history.md) — discovery of earlier sessions.
- [Session titles](specs/session-titles.md) — automatic conversation titles.
- [Skills](specs/skills.md) — reusable instruction packs.
- [Tool search](specs/tool-search.md) — deferred capability loading.
- [Worktrees](specs/worktrees.md) — isolated Git workspace behavior.
- [Yolop framing](specs/yolop.md) — requests addressed to Yolop itself.

## Protocols and optional integrations

- [ACP](specs/acp.md) — Agent Client Protocol support.
- [AST editing](specs/ast_edit.md) — previewed structural rewrites.
- [Herdr](specs/herdr.md) — cloud execution integration.
- [LSP](specs/lsp.md) — language-server integration.
- [MCP](specs/mcp.md) — Model Context Protocol client support.
- [OKF](specs/okf.md) — Open Knowledge Format integration.
- [Trajectory export](specs/trajectory.md) — ATIF trajectory export.
- [Tuika images](specs/tuika-images.md) — terminal image rendering.
- [Tuika keymap](specs/keymap.md) — declarative key-binding dispatch.

## Safety and execution

- [Crash reporting](specs/crash-reporting.md) — local privacy-preserving panic diagnostics.
- [Sandboxing](specs/sandboxing.md) — filesystem and process boundaries.

## Engineering processes

- [Shipping](specs/shipping.md) — requirements for landing a change.
- [Maintenance](specs/maintenance.md) — repository health and release readiness.
- [Release](specs/release.md) — publishing crates and Homebrew releases.
- [Documentation](specs/documentation.md) — public/internal documentation contract.
- [Agent context](specs/agent-context.md) — how AGENTS.md, knowledge, and skills are layered.

---
type: Product Specification
title: Skills Specification
description: Defines the skills specification contract for Yolop.
---

# Skills Specification

## Abstract

Skills are instruction packages (`SKILL.md` files) the agent can discover and
activate at runtime, manage through the detached `yolop skills` CLI, and see in
the system-prompt listing.

yolop uses the upstream `ScopedSkillsCapability` from `everruns-core` (0.12.0+).
That capability owns discovery, precedence, the model tools (`list_skills` and
`activate_skill`), validation, and `SKILL.md` substitution, all driven strictly
through the session `SessionFileSystem`. yolop
supplies only the embedder-specific glue (`crate::capabilities::skills`):

1. **The scope set and writability**: workspace, profile, global, environment,
   extension, and system (below), passed as `SkillScope`s with their physical
   directory paths.
2. **A host-path `SkillDirResolver`**: so `${SKILL_DIR}` and displayed paths stay
   usable by the host `bash` tool.
3. **Bundled system skills**: pre-packed in the binary and materialized once.
4. **File-store routing**: `CodingCliSessionFileStore` permits reads from physical
   skill directories outside the workspace while rejecting normal file-tool
   writes to them.

This spec owns the scope set and the yolop wiring; the capability contract and
`SKILL.md` format are owned upstream (see `everruns` `specs/skills-registry.md`).

> History: yolop previously **vendored** the whole capability because the
> upstream `SkillsCapability` scanned a single VFS path and the embedded runtime
> could not inject multi-scope sources. The multi-source resolver was pushed
> upstream as `ScopedSkillsCapability` (everruns#2185), so the vendored copy was
> retired and only the glue above remains.

## Scopes

A skill is a directory named for the skill, containing `SKILL.md`. Each scope is
a labeled **real on-disk directory**:

1. **Workspace**: `<workspace>/.agents/skills/<name>/`.
   Lives in the project under version control; ships with the repo it belongs to.
   Writable.
2. **Profile**: the active `--profile`'s skills directory, `skills_dir` or the
   conventional `<config_dir>/yolop/profiles/<name>/skills/`. Present only while that profile is selected.
   Writable. A profile is chosen per run, so it outranks the user's global
   skills and never the repository's own; see
   [configuration](configuration.md).
3. **Global**: `~/.agents/skills/<name>/`, installed once per user and shared across every
   workspace. Writable. Overridable with `YOLOP_GLOBAL_SKILLS_DIR`. The prior
   `<config_dir>/yolop/skills/` root is imported temporarily for compatibility;
   existing primary entries are never overwritten.
4. **Environment**: optional, integration-owned skills materialized under the
   session directory. Always read-only through normal file tools. Herdr currently
   contributes this scope when its inherited environment contract is valid.
5. **Extension**: skills already installed inside enabled extension packages.
   Their declared skill directories are injected directly and are read-only
   through normal file tools.
6. **System**: pre-packed inside the yolop binary and materialized once to
   `<data_dir>/yolop/system-skills/<name>/`. Always
   available, **read-only**. Overridable with `YOLOP_SYSTEM_SKILLS_DIR` (used
   verbatim, no materialization).

## Management and model surfaces

The detached `yolop skills` command owns list, read, delete, registry search,
and registry install operations. It reuses the scoped skills,
yolop skill management, and registry services. The assembled model tool surface
contains exactly `list_skills` and `activate_skill`; CLI duplication of those two
operations is intentional.

## Required Behavior

1. **Merge.** `list_skills` and the system-prompt listing see skills from all
   active scopes as one set; each entry is tagged with its scope.
2. **Precedence.** When the same skill directory name exists in more than one
   scope, the most specific wins: workspace shadows profile, which shadows
   global, which shadows environment, which shadows system.
   Discovery de-duplicates by directory name in that order; `activate_skill`
   resolves the same way.
3. **Usable paths.** Every scope expands `${SKILL_DIR}` to a real path the host
   `bash` tool can read, so bundled and generated files work consistently.
4. **No command execution on activation.** The `!`cmd`` substitution is never
   expanded, activating a skill must not spawn a shell on the host (mirrors the
   upstream trust gate; see `everruns-core` skills / EVE-388).
5. **Writes are explicit.** Normal file tools edit only the workspace. The
   `yolop skills write` command may write workspace or global skills when the
   user asks Yolop to install or modify skills. System skills are read-only.
6. **Hot install.** Workspace and global scope paths are kept even when the
   directories do not exist yet. Discovery reads the filesystem on each
   `list_skills`, CLI `read`, and `activate_skill` call, so a skill installed
   after Yolop starts is available without restarting.
7. **Inspect workspace/global skills.** `yolop skills read` returns an installed
   skill's `SKILL.md` and file manifest. Registry installation validates the
   skill name and `SKILL.md`, bounds files, rejects path traversal, and never
   writes system skills.
8. **Uninstall.** The `yolop skills delete` command removes an installed skill
   from a writable scope (`workspace` or `global`). It validates the name as a
   single path segment (no separators, `.`, or `..`), refuses a directory with no
   `SKILL.md`, and never touches the read-only system scope. The upstream capability has no removal.
9. **Absent scopes are silent.** A missing workspace/global directory is simply
   empty until a skill is installed. A failure to materialize system skills
   disables that scope without failing the session.
10. **Materialization is safe.** System-skill materialization is idempotent and
    concurrency-safe (atomic per-file writes, skipped when bytes are unchanged),
    so parallel processes do not race on the shared cache directory.
11. **Management guidance is bundled.** Yolop ships a `skill-management`
    system skill that directs operators to `yolop skills` for listing, reading,
    searching, installing, and deleting skills.
12. **Registry search and install.** `yolop skills search` queries skills.sh and
    returns structured matches. `yolop skills install` downloads and validates a
    snapshot into a writable scope. Neither operation is exposed as a model tool.
13. **User guide is bundled.** Yolop ships a `yolop` system skill from the
    private bundled asset tree as the durable reference for slash commands,
    keyboard shortcuts, CLI flags, and session controls. `/help` in the TUI
    summarizes the live command registry and shortcuts; the skill carries the
    full guide for conversational help.
14. **Environment skills are session-owned.** An environment integration may
    materialize a read-only skill scope beneath the current session directory.
    Herdr does this only when its inherited pane/socket contract is present.
    Precedence is workspace, profile, global, environment, then system.

## Ownership Boundary

- `everruns_core::capabilities::ScopedSkillsCapability` owns discovery,
  precedence, the skills tools, validation, and substitution, all through the
  session `SessionFileSystem`.
- `crate::capabilities::skills` owns the yolop wiring: the scope set, the
  host-path `SkillDirResolver`, embedded and generated skill materialization, and
  the operator-only `SkillManagementCapability` behind `yolop skills`.
- `crate::capabilities::skill_registry` owns the skills.sh HTTP client used by
  the CLI `search` and `install` operations.
- `crate::runtime` (`CodingCliSessionFileStore`) owns read-only access to physical
  skill directories outside the workspace.
- `everruns_core::skill` owns the `SKILL.md` format, parsing, validation, and
  substitution.

## Management surface

The model-visible skill surface is exactly `list_skills` and `activate_skill`.
Reading, deleting, searching, and installing skills are operator actions exposed
through top-level `yolop skills list|read|delete|search|install`; they are not
model tools.

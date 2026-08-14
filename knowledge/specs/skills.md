---
type: Product Specification
title: Skills Specification
description: Defines the skills specification contract for Yolop.
---

# Skills Specification

## Abstract

Skills are instruction packages (`SKILL.md` files) the agent can discover,
activate, and manage at runtime via skills tools, plus a system-prompt listing
of what's available.

yolop uses the upstream `ScopedSkillsCapability` from `everruns-core` (0.12.0+).
That capability owns discovery, precedence, the skills tools (`list_skills`,
`activate_skill`, `read_skill`, `write_skill`), validation, and `SKILL.md`
substitution — all driven strictly through the session `SessionFileSystem`. yolop
supplies only the embedder-specific glue (`crate::capabilities::skills`):

1. **The scope set and writability** — workspace, global, environment, and
   system (below), passed as `SkillScope`s with labeled **VFS roots** (never
   host paths).
2. **A host-path `SkillDirResolver`** — so `${SKILL_DIR}` and the displayed paths
   expand to real on-disk paths the host `bash` tool can read. (The core default
   keeps them in the VFS, which is correct only when the shell shares that
   namespace; yolop's `bash` runs on the host.)
3. **Bundled system skills** — pre-packed in the binary and materialized once.
4. **File-store routing** — `CodingCliSessionFileStore` maps each scope's VFS root
   onto the real directory or ephemeral store below, so the capability reaches
   global/environment/system skills through the VFS without ever being handed a
   host path.

This spec owns the scope set and the yolop wiring; the capability contract and
`SKILL.md` format are owned upstream (see `everruns` `specs/skills-registry.md`).

> History: yolop previously **vendored** the whole capability because the
> upstream `SkillsCapability` scanned a single VFS path and the embedded runtime
> could not inject multi-scope sources. The multi-source resolver was pushed
> upstream as `ScopedSkillsCapability` (everruns#2185), so the vendored copy was
> retired and only the glue above remains.

## Scopes

A skill is a directory named for the skill, containing `SKILL.md`. Each scope is
a labeled VFS root that yolop's file store maps to a **real on-disk directory**:

1. **Workspace** — `<workspace>/.agents/skills/<name>/` (VFS `/.agents/skills`).
   Lives in the project under version control; ships with the repo it belongs to.
   Writable.
2. **Global** — `~/.agents/skills/<name>/` (VFS
   `/.yolop/global-skills`), installed once per user and shared across every
   workspace. Writable. Overridable with `YOLOP_GLOBAL_SKILLS_DIR`. The prior
   `<config_dir>/yolop/skills/` root is imported temporarily for compatibility;
   existing primary entries are never overwritten.
3. **Environment** — optional, integration-owned skills mounted into the
   session-only VFS (VFS `/.yolop/environment-skills`). Always read-only and
   never materialized on disk. Herdr currently contributes this scope when its
   inherited environment contract is valid.
4. **System** — pre-packed inside the yolop binary and materialized once to
   `<data_dir>/yolop/system-skills/<name>/` (VFS `/.yolop/system-skills`). Always
   available, **read-only**. Overridable with `YOLOP_SYSTEM_SKILLS_DIR` (used
   verbatim, no materialization).

## Required Behavior

1. **Merge.** `list_skills` and the system-prompt listing see skills from all
   active scopes as one set; each entry is tagged with its scope.
2. **Precedence.** When the same skill directory name exists in more than one
   scope, the most specific wins: workspace shadows global, which shadows
   environment, which shadows system.
   Discovery de-duplicates by directory name in that order; `activate_skill`
   resolves the same way.
3. **Usable paths.** Disk-backed scopes expand `${SKILL_DIR}` to paths the host
   `bash` tool can read, so bundled files work. Environment skills remain
   VFS-only and must not advertise shell-side assets.
4. **No command execution on activation.** The `!`cmd`` substitution is never
   expanded — activating a skill must not spawn a shell on the host (mirrors the
   upstream trust gate; see `everruns-core` skills / EVE-388).
5. **Writes are explicit.** Normal file tools edit only the workspace. The
   dedicated `write_skill` tool may write workspace or global skills when the
   user asks Yolop to install or modify skills. System skills are read-only.
6. **Hot install.** Workspace and global scope paths are kept even when the
   directories do not exist yet. Discovery reads the filesystem on each
   `list_skills`, `read_skill`, and `activate_skill` call, so a skill installed
   after Yolop starts is available without restarting.
7. **Manage workspace/global skills.** `read_skill` returns an installed
   skill's `SKILL.md` and file manifest. `write_skill` installs or updates a
   skill in the workspace (`workspace`/`local`) or global (`global`) scope.
   `write_skill` validates the skill name and `SKILL.md`, requires the
   frontmatter `name` to match the directory name, bounds extra files, rejects
   path traversal, and never writes system skills.
8. **Uninstall.** The yolop-owned `delete_skill` tool removes an installed skill
   from a writable scope (`workspace` or `global`). It validates the name as a
   single path segment (no separators, `.`, or `..`), refuses a directory with no
   `SKILL.md`, and never touches the read-only system scope. This is the
   conversational uninstall path (see [`conversational-control.md`](./conversational-control.md));
   the upstream capability has no removal.
9. **Absent scopes are silent.** A missing workspace/global directory is simply
   empty until a skill is installed. A failure to materialize system skills
   disables that scope without failing the session.
10. **Materialization is safe.** System-skill materialization is idempotent and
    concurrency-safe (atomic per-file writes, skipped when bytes are unchanged),
    so parallel processes do not race on the shared cache directory.
11. **Management guidance is bundled.** Yolop ships a `skill-management` system
    skill that tells the agent how to inspect, install, search for, and upgrade
    skills. Prefer the yolop-owned `search_skills` / `install_skill` tools for
    the public skills.sh registry (search → ask which to install → install).
    Reconstruct `npx skill add ...` style installs that are not on skills.sh by
    fetching source files directly and writing them with `write_skill`.
12. **Registry search and install.** `search_skills` queries skills.sh and
    returns structured matches (id, source, installs, URL). `install_skill`
    downloads a skills.sh snapshot into a writable scope (`workspace` or
    `global`), validates `SKILL.md` and relative paths, and makes the skill
    available immediately. Both are conversational control surfaces (see
    [`conversational-control.md`](./conversational-control.md)): present search
    results and ask before installing. System skills remain read-only.
13. **User guide is bundled.** Yolop ships a `yolop` system skill from the
    private bundled asset tree as the durable reference for slash commands,
    keyboard shortcuts, CLI flags, and session controls. `/help` in the TUI
    summarizes the live command registry and shortcuts; the skill carries the
    full guide for conversational help.
14. **Environment skills are ephemeral.** An environment integration may mount
    a read-only, VFS-only skill scope for the current session. Herdr does this
    only when its inherited pane/socket contract is present. Precedence is
    workspace, global, environment, then system; the mount never writes any
    skill directory.

## Ownership Boundary

- `everruns_core::capabilities::ScopedSkillsCapability` owns discovery,
  precedence, the skills tools, validation, and substitution — all through the
  session `SessionFileSystem`.
- `crate::capabilities::skills` owns the yolop wiring: the scope set, the
  host-path `SkillDirResolver`, the embedded system skills + materialization, the
  VFS-root constants the file store routes on, and the `SkillManagementCapability`
  that contributes `search_skills`, `install_skill`, and `delete_skill`. That
  capability must be both registered *and* enabled in the default coding
  harness; registering it alone leaves its tools out of every session while the
  skill-management skill still instructs the model to call them.
- `crate::capabilities::skill_registry` owns the skills.sh HTTP client used by
  `search_skills` / `install_skill`.
- `crate::runtime` (`CodingCliSessionFileStore`) owns mapping the scope VFS roots
  to real on-disk directories.
- `everruns_core::skill` owns the `SKILL.md` format, parsing, validation, and
  substitution.

# Skills Specification

## Abstract

Skills are instruction packages (`SKILL.md` files) the agent can discover,
activate, and manage at runtime via skills tools, plus a system-prompt listing
of what's available.

yolop **vendors** the upstream `skills` capability from `everruns-core`. The
upstream capability scans a single virtual-filesystem path (`/.agents/skills`)
through the session filesystem; that is too narrow for global and system scopes,
and the embedded in-process runtime does not apply the `mounts()` mechanism the
server uses to inject extra skills. yolop therefore owns the discovery loop and
reads the scope folders directly from disk, while **reusing** the upstream
`SKILL.md` parser, validator, and argument/variable substitution verbatim
(`everruns_core::skill`). This spec owns the scope set and behavior; the
`SKILL.md` format is owned upstream.

> Forward plan: once this multi-source resolver is stable, the intent is to push
> it upstream (a capability that accepts multiple labeled skill sources, and/or
> the planned in-process "mount-overlay resolver") so the vendoring can be
> retired and duplication removed.

## Scopes

A skill is a directory named for the skill, containing `SKILL.md`. Every scope
resolves to a **real on-disk directory**:

1. **Workspace** — `<workspace>/.agents/skills/<name>/`. Lives in the project
   under version control; ships with the repo it belongs to.
2. **Global** — `<config_dir>/yolop/skills/<name>/` (e.g.
   `~/.config/yolop/skills/` on Linux), installed once per user and shared
   across every workspace. Overridable with `YOLOP_GLOBAL_SKILLS_DIR`.
3. **System** — pre-packed inside the yolop binary and materialized once to
   `<data_dir>/yolop/system-skills/<name>/`. Always available. Overridable with
   `YOLOP_SYSTEM_SKILLS_DIR` (used verbatim, no materialization).

## Required Behavior

1. **Merge.** `list_skills` and the system-prompt listing see skills from all
   three scopes as one set; each entry is tagged with its scope.
2. **Precedence.** When the same skill directory name exists in more than one
   scope, the most specific wins: workspace shadows global shadows system.
   Discovery de-duplicates by directory name in that order; `activate_skill`
   resolves the same way.
3. **Real paths.** Because every scope is a real directory, `${SKILL_DIR}` in an
   activated skill expands to a path the host `bash` tool can read, so bundled
   files work for all scopes.
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
8. **Absent scopes are silent.** A missing workspace/global directory is simply
   empty until a skill is installed. A failure to materialize system skills
   disables that scope without failing the session.
9. **Materialization is safe.** System-skill materialization is idempotent and
   concurrency-safe (atomic per-file writes, skipped when bytes are unchanged),
   so parallel processes do not race on the shared cache directory.
10. **Management guidance is bundled.** Yolop ships a `skill-management` system
    skill that tells the agent how to inspect, install, search for, and upgrade
    skills, including reconstructing `npx skill add ...` style installs by
    fetching source files directly and writing them with `write_skill`.

## Ownership Boundary

- This spec and `crate::skills` own scope resolution, discovery, precedence, and
  the skills tools.
- `everruns_core::skill` owns the `SKILL.md` format, parsing, validation, and
  substitution.

# Skills Specification

## Abstract

Skills are instruction packages (`SKILL.md` files) the agent can discover and
activate at runtime via the `list_skills` and `activate_skill` tools, plus a
system-prompt listing of what's available.

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
5. **Writes stay in the workspace.** Global and system folders are read-only
   inputs; only the workspace scope is edited through the agent's file tools.
6. **Absent scopes are silent.** A missing global directory, or a failure to
   materialize system skills, disables that scope without failing the session.
7. **Materialization is safe.** System-skill materialization is idempotent and
   concurrency-safe (atomic per-file writes, skipped when bytes are unchanged),
   so parallel processes do not race on the shared cache directory.

## Ownership Boundary

- This spec and `crate::skills` own scope resolution, discovery, precedence, and
  the two tools.
- `everruns_core::skill` owns the `SKILL.md` format, parsing, validation, and
  substitution.

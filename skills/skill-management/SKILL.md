---
name: skill-management
description: Install, inspect, update, or search for Yolop skills in workspace and global scopes. Use when the user asks to manage skills, import a skill from a registry or GitHub, or upgrade skills.
user-invocable: true
---

# Skill Management

Use this skill when the user wants to customize Yolop with skills.

## Ground Rules

- Installed skills are directories with a `SKILL.md`.
- Workspace skills live under `.agents/skills/<name>/`.
- Global skills live under Yolop's config directory, normally
  `<config_dir>/yolop/skills/<name>/`.
- System skills are built into Yolop and are read-only.
- Workspace skills shadow global skills; global skills shadow system skills.
- New or changed workspace/global skills are available immediately through
  `list_skills` and `activate_skill`; do not ask the user to restart Yolop.

## Inspect

1. Call `list_skills` to see installed skills and scopes.
2. Call `read_skill` before modifying an existing skill.
3. Prefer editing the nearest scope that matches the user's intent:
   workspace/local for project-specific behavior, global for cross-project
   behavior.

## Install Or Modify

Use `write_skill` with:

- `scope`: `workspace`/`local` or `global`
- `name`: the skill directory name
- `skill_md`: the full `SKILL.md` contents
- `files`: optional bundled files keyed by relative path

`write_skill` validates the skill name, parses `SKILL.md`, rejects path
traversal in bundled files, and writes the skill atomically.

## Import From npx-Style Sources

If the user mentions an `npx skill add ...` command, reconstruct the install
without requiring `npx` or the package:

1. Identify the package, registry, or repository that command would fetch.
2. Fetch the relevant `SKILL.md` and bundled files from the source.
3. Preserve the upstream skill directory name unless the user asks to rename it.
4. Install with `write_skill`.
5. Call `list_skills` afterward and report the installed scope/path.

If the source is ambiguous or private and cannot be fetched, ask for the exact
repository, package, archive, or file contents.

## Search

For public skills, search in this order:

1. Known registry or site named by the user, such as `skills.sh`.
2. GitHub repositories and paths containing `SKILL.md`.
3. General web search for the skill topic plus `SKILL.md`.

Prefer source URLs that expose the actual files. Do not install from a summary
page unless you can retrieve the real `SKILL.md` and any referenced files.

## Upgrade

For one skill:

1. `read_skill` to capture the current scope, path, and any local changes.
2. Fetch the current upstream version.
3. Compare with the installed version when practical.
4. Use `write_skill` to update the same scope.
5. `activate_skill` or `list_skills` to verify it loads.

For all skills:

1. `list_skills`.
2. Upgrade only skills with an identifiable upstream source or enough user
   context to locate one.
3. Skip read-only system skills unless Yolop itself is being updated.
4. Report skipped skills with the reason, especially when no source is known.

Never silently overwrite a clearly user-customized skill with unrelated
registry content. If the intended upstream is uncertain, ask first.

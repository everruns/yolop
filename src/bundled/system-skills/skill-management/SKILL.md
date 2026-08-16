---
name: skill-management
description: Install, inspect, update, or search for Yolop skills in workspace and global scopes. Use when the user asks to manage skills, find a skill for a task, import a skill from skills.sh or GitHub, or upgrade skills.
user-invocable: true
---

# Skill Management

Use this skill when the user wants to customize Yolop with skills, especially
when they ask to *find* a skill for a task.

## Ground Rules

- Installed skills are directories with a `SKILL.md`.
- Workspace skills live under `.agents/skills/<name>/`.
- Global skills live under `~/.agents/skills/<name>/`. The former
  `<config_dir>/yolop/skills/<name>/` location is supported temporarily for
  compatibility.
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

## Search (conversational)

When the user asks to find a skill for something:

1. Call `search_skills` with a short query (and optional `owner`).
2. Present the top matches: name, source repo, install count, and skills.sh URL.
3. **Ask which skill to install** (and whether workspace or global) before
   writing anything.
4. Call `install_skill` with the chosen `install_source` / id from the search
   result (`owner/repo/skillId`).
5. Confirm with `list_skills` and offer to `activate_skill` when useful.

Do not install on a guess. If search returns nothing useful, say so and offer
the GitHub / web fallback below.

## Install From Registry

`install_skill` fetches a skills.sh snapshot and writes it into a writable
scope:

- `source`: `owner/repo/skillId`, `owner/repo@skill`, or a skills.sh URL
- `scope`: `workspace` (default) or `global`
- `overwrite`: defaults to true

After install the skill is live, no restart.

## Install Or Modify By Hand

Use `write_skill` when you already have the `SKILL.md` (and optional files), or
when the source is not on skills.sh:

- `scope`: `workspace`/`local` or `global`
- `name`: the skill directory name
- `skill_md`: the full `SKILL.md` contents
- `files`: optional bundled files keyed by relative path

`write_skill` validates the skill name, parses `SKILL.md`, rejects path
traversal in bundled files, and writes the skill atomically.

## Import From npx-Style Or GitHub Sources

If the user mentions an `npx skill add ...` command or a GitHub skill that
`install_skill` cannot resolve:

1. Prefer `install_skill` with `owner/repo@skill` when the package is on
   skills.sh.
2. Otherwise identify the repository or files that command would fetch.
3. Fetch the relevant `SKILL.md` and bundled files (`web_fetch` /
   `free_web_search`).
4. Preserve the upstream skill directory name unless the user asks to rename it.
5. Install with `write_skill`.
6. Call `list_skills` afterward and report the installed scope/path.

If the source is ambiguous or private and cannot be fetched, ask for the exact
repository, package, archive, or file contents.

## Search Fallback Order

1. `search_skills` (skills.sh), preferred.
2. GitHub repositories and paths containing `SKILL.md` via `free_web_search` /
   `web_fetch`.
3. General web search for the skill topic plus `SKILL.md`.

Prefer source URLs that expose the actual files. Do not install from a summary
page unless you can retrieve the real `SKILL.md` and any referenced files.

## Upgrade

For one skill:

1. `read_skill` to capture the current scope, path, and any local changes.
2. Prefer `install_skill` when the upstream is a known skills.sh source.
3. Otherwise fetch the current upstream version and `write_skill`.
4. Compare with the installed version when practical.
5. `activate_skill` or `list_skills` to verify it loads.

For all skills:

1. `list_skills`.
2. Upgrade only skills with an identifiable upstream source or enough user
   context to locate one.
3. Skip read-only system skills unless Yolop itself is being updated.
4. Report skipped skills with the reason, especially when no source is known.

Never silently overwrite a clearly user-customized skill with unrelated
registry content. If the intended upstream is uncertain, ask first.

## Uninstall

Use `delete_skill` with `name` and `scope` (`workspace` or `global`). System
skills cannot be deleted.

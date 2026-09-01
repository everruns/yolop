---
name: skill-management
description: Manage installed skills and the public skills.sh registry through yolop's CLI.
---
# Skill Management

Use the attached `yolop skills` CLI for package management:

- `yolop skills list`
- `yolop skills read <name>`
- `yolop skills search <query>`
- `yolop skills install <source> [--scope workspace|global]`
- `yolop skills delete <name> [--scope workspace|global]`

Search results return canonical sources such as `owner/repo/skillId`. Ask the
user which result and scope they want before installing or deleting. Workspace
scope is project-owned; global scope is personal. System skills are read-only.

The model-facing skill tools are only `list_skills` and `activate_skill`.

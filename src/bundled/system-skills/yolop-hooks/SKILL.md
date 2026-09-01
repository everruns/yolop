---
name: yolop-hooks
description: Configure yolop lifecycle hooks with the attached CLI and scoped JSON manifests.
---
# Yolop Hooks

Manage hooks through `yolop config hooks`:

- `yolop config hooks list`
- `yolop config hooks get <id>`
- `yolop config hooks set <file> [--scope global|workspace]`
- `yolop config hooks remove <id> [--scope global|workspace]`

Global hooks live in the yolop config directory. Workspace hooks live in
`.agents/hooks.json` and override global hooks by id. Hook management is host
administration, not a model tool surface. Runtime execution remains the upstream
`user_hooks` capability.

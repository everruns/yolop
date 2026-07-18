---
name: author-extension
description: Write a new yolop extension end-to-end — scaffold, implement, install, verify, and enable it — when the user asks yolop to add a capability, integrate with something, or "build an extension" for a need that no installed extension covers.
metadata:
  internal: true
user-invocable: true
---

# Author an extension

Goal: turn a capability request ("integrate with X", "block Y", "add a tool
that Z") into a working, installed yolop extension — built by yolop itself.

An extension is a capability package served over YEP (the yolop extension
protocol): a `plugin.json` manifest plus a capability server that speaks
newline-delimited JSON-RPC over stdio. See [`specs/extensions.md`](../../../specs/extensions.md).

## When to use

Use this when the user wants a *new, persistent* capability and no installed
extension provides it — something worth keeping across sessions, not a one-off
shell command. Signals: "make yourself a tool for…", "add an extension that…",
"integrate yolop with…", "from now on, block/allow…".

If an installed extension already covers it, use that instead
(`list_extensions`). If the need is a single throwaway action, just do it — do
not author an extension for it.

## What an extension can contribute

Declare only what you need; a manifest must contribute at least one of:

- **tools** — functions the model can call (the model sees name + schema).
- **hooks** — `pre_tool_use` (can **block** a tool call, e.g. deny git) or
  `post_tool_use` (observe-only).
- **prompt** — a static system-prompt contribution.
- **mcpServers** — contributed MCP servers (declare in the manifest directly).
- **status** — a status-bar field the server updates live by pushing
  `status/changed` (e.g. a counter). Scaffold with `status: true`; the generated
  server gets an `emit_status(text)` helper (empty text clears the field). Shows
  in the inline and full-screen TUIs; a no-op in `--print`/ACP.
- **skills** — a `skills/` directory of `SKILL.md` files, mounted read-only for
  the enabled extension (same discovery as workspace/global skills). Scaffold
  with `skills: true` to get a starter `skills/<name>/SKILL.md`.
- **commands** — slash commands, registered namespaced as `/<ext>:<cmd>` in the
  palette and dispatched to the server over `command/execute` (the result
  message is shown to the user). Scaffold with `commands: ["name", …]`; the
  generated server gets a `handle_command(name, arguments)` seam.

Not yet available: providers, `ui/ask`.

## The loop

1. **Scaffold.** `scaffold_extension name=<name> [description=…] [language=python|typescript|rust]`
   with the facets you need — `tools=[…]`, `hooks=[…]`, and/or `prompt=…`.
   This writes a package (manifest + server source) with the handler bodies
   stubbed. `python` and `typescript` (a dependency-free Node.js server) are
   single-file and need no build step; `rust` emits a `serde_json`-only crate.
   Pick the one whose toolchain the environment has; Python is the default.

   For `rust`, the scaffold result includes a `build` command — run it after
   editing to compile the binary into `bin/` before installing. The zero-build
   templates skip that.

2. **Implement.** Open the generated server (the tool result prints its path)
   and fill in the `handle_*` bodies:
   - `handle_tool(name, args)` — return the tool result dict.
   - `handle_hook(event, tool_name, args)` — return `{}` to allow, or
     `{"block": True, "reason": "…"}` to deny (pre_tool_use only). The server
     receives *every* subscribed tool call, so gate on `tool_name`/`args`.
   - Keep stdout for protocol JSON only; log to stderr.
   Edit only the marked handler bodies — leave the protocol plumbing alone.

3. **Install.** `install_extension source=<package dir>` — copies the package
   into the store and pins it. Installing runs third-party code; since *you*
   authored it here, the sharp edge is enabling — see step 5.

4. **Verify.** `doctor_extension name=<name>` — spawns the server, runs the
   handshake, and checks the served tools/prompt against the manifest. Fix any
   `fail`/`warn` before enabling. If the server won't spawn, check the shebang
   and that the command is executable.

5. **Enable (ask first).** Show the user the contribution summary and ask
   before `enable_extension name=<name>` — enabling adds it to the harness and
   runs its server every session. **Enabling takes effect on the next
   session**, so tell the user to restart yolop to load it.

## Acceptance check

The loop is proven when yolop, unaided, produces an extension that actually
works — e.g. a `pre_tool_use` hook that blocks any `git` command, or a tool
that returns a computed value — installed, doctor-green, and effective after a
restart. The end-to-end deny path is covered by
`scaffolded_extension_blocks_git_end_to_end` in `src/extensions/mod.rs`.

## Notes

- Names: ascii letters, digits, `-`, `_`. The dir name must equal the manifest
  `name`.
- Do not hardcode absolute paths in the manifest — the package is copied on
  install. The scaffold resolves the server via the package's `bin/` on PATH.
- Iterate by editing the server and re-running `doctor_extension`; no reinstall
  is needed while working from the scaffolded dir, but re-run `install_extension`
  to pick up edits into the store.

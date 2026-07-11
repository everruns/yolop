---
name: yolop
description: Built-in guide to using yolop — slash commands, keyboard shortcuts, CLI flags, tools, and session controls. Use when the user asks how yolop works, what commands or shortcuts are available, what yolop can do, or wants help using the terminal UI.
user-invocable: true
---

# Yolop user guide

This skill is the durable reference for how to use yolop interactively. The live
command list always comes from the session registry (`/help` in the TUI, or
`list_commands` for the agent); this document explains what those commands do
and how the UI behaves.

## Quick reference in the TUI

- `/help` — commands with descriptions plus keyboard shortcuts
- `/tools` — comma-separated list of tools available this session
- `/mcp` — configured MCP servers (or where to add them)
- `/cwd` — workspace root
- Startup banner — workspace, model, tools, command names, and the most common
  shortcuts

## Slash commands

Commands are contributed by capabilities. The interactive TUI registers all of
the following; `--print` and ACP omit the terminal-only client commands.

### Terminal client commands

These act on the TUI itself (clear transcript, open overlays, quit). The agent
can also run them via `run_yolop_command` when the user asks in plain language.

| Command | Description |
| --- | --- |
| `/help` | Show commands, descriptions, and keyboard shortcuts |
| `/tools` | List available tools for this session |
| `/mcp` | List configured MCP servers |
| `/cwd` | Print the workspace root |
| `/status [compact\|expanded\|toggle]` | Change session status bar layout (clicking the status bar toggles too) |
| `/model [id]` | Open the model picker; an argument pre-fills selection |
| `/effort [level]` | Open the reasoning-effort picker; an argument pre-fills selection |
| `/clear` | Clear the transcript and re-show the startup banner |
| `/shell <command>` | Run a shell command from the workspace root with bounded inline output |
| `/quit` or `/exit` | Exit yolop |

Shell aliases: `!<command>` and `!shell <command>` run the same bounded shell
path as `/shell` (handy for quick one-offs).

### Session and agent commands

| Command | Description |
| --- | --- |
| `/setup [subcommand]` | Guided provider/model setup, or direct forms: `status`, `provider`, `model`, `effort`, `token`, `url`, `attribution`, `approval` |
| `/goal [condition]` | Set a completion condition and keep working until met; `pause`/`resume`, `clear`, or omit for status |
| `/background` | List background tasks and their status |
| `/btw <question>` | Ask a side question with session context — no tools, not added to history |
| `/worktree [off]` | Show worktree status, or `off` to disable auto worktree activation |

User-invocable skills (for example `/yolop`, `/yolop-config`, `/skill-management`)
also appear in the registry when installed.

## Keyboard shortcuts

### Composer (idle)

| Shortcut | Action |
| --- | --- |
| `Enter` | Send message |
| `Shift-Enter` | Insert newline in the composer |
| `Alt-Shift-Enter` | Send (alternative submit) |
| `Tab` | Accept the first slash-command suggestion, or insert a literal tab |
| `←` / `→` | Move cursor in the composer |
| `/` then type | Command palette suggestions (accept with Tab) |
| `Ctrl+V` | Paste a clipboard image as an attachment, or attach large text as a placeholder |
| Terminal paste | Large pastes (>1000 chars) attach as `[Pasted Content N chars]` placeholders |

### Session control

| Shortcut | Action |
| --- | --- |
| `Ctrl-C` (once, empty input) | Arm exit — press again to quit |
| `Ctrl-C` (with draft text) | Clear the composer |
| `Ctrl-D` | Exit immediately |
| `Esc` twice (while a turn runs) | Cancel the current turn |
| `Ctrl+B` | Toggle the read-only background-tasks panel |

### Background-tasks panel

| Shortcut | Action |
| --- | --- |
| `↑` / `↓` or `k` / `j` | Scroll the task list |
| `Esc` or `q` | Close the panel |

Transcript history scrolls through the terminal's own scrollback (ratatui inline
viewport); there is no in-app page-up/page-down.

### Setup overlays (`/setup`, `/model`, `/effort`)

| Shortcut | Action |
| --- | --- |
| `↑` / `↓` or `k` / `j` | Move selection |
| `Enter` | Confirm |
| `Esc` | Cancel / go back |
| `0`–`9` | Jump to numbered entry (where shown) |

## What yolop can do

yolop is an autonomous coding agent for the workspace it was started in.

- **Files** — read, write, edit, grep, delete, and map the repository
- **Shell** — `bash -lc` from the workspace root (120 s timeout, capped output)
- **AST search** — structural `ast_grep` across common languages (default harness)
- **AST edit** *(opt-in)* — previewed `ast_edit` rewrites; enable with `[[capabilities]] ref = "ast_edit"`
- **Background work** — detached shell commands and focused sub-agents
- **Web** — `web_fetch` and `duckduckgo_search`
- **Memory** — durable cross-session notes via `remember` / `recall` / `forget`
- **Skills** — `SKILL.md` packages in workspace, global, and bundled system scopes
- **MCP** — extra tools from `.mcp.json` / global `mcp.json`
- **Hooks** — block, allow, or audit tool calls (see `yolop-hooks` skill)
- **Goal loops** — `/goal` keeps working until an evaluator confirms the condition
- **Soft approval** — optional spoken consent before destructive steps (`/setup approval`)
- **Sessions** — resume with `--session <id>`; every run logs to `events.jsonl`

Natural-language control: the agent can switch model, effort, or provider, clear
the screen, show help, and more without the user typing slash commands — see
`specs/conversational-control.md`.

## CLI flags (non-interactive)

| Flag | Description |
| --- | --- |
| `-C, --cwd <PATH>` | Workspace root (default: current directory) |
| `--provider <P>` | Force a provider |
| `-m, --model <ID>` | Override model for the chosen provider |
| `-p, --print <PROMPT>` | One-shot mode — print result and exit |
| `--acp` | Agent Client Protocol over stdio (editors like Zed) |
| `--session <ID>` | Resume a previous session |
| `--session-dir <PATH>` | Override where session folders are stored |
| `--reasoning-effort <E>` | Reasoning effort when the model profile supports it |

Subcommands: `yolop version`, `yolop into zed`, `yolop worktree list|prune`.

## Related skills

| Skill | When to use |
| --- | --- |
| `yolop-config` | Change `settings.toml` — provider, tokens, models, capabilities |
| `yolop-hooks` | Configure behavioral hooks |
| `skill-management` | Install, upgrade, or remove skills |
| `ast-grep` | Structural code search and rewrite patterns |

## Answering help requests

When the user asks what yolop can do or how to use it:

1. Summarize the relevant section from this skill (commands, shortcuts, or
   features — match what they asked).
2. For the **live** command or tool list, prefer `/help`, `/tools`, or
   `run_yolop_command` / `list_skills` rather than guessing.
3. Point power users at `README.md` and `specs/` for protocol and configuration
   depth; keep conversational answers short.

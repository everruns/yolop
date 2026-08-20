# Yolop

[![CI](https://github.com/everruns/yolop/actions/workflows/ci.yml/badge.svg)](https://github.com/everruns/yolop/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/yolop.svg)](https://crates.io/crates/yolop)
[![Homebrew](https://img.shields.io/badge/Homebrew-everruns%2Ftap-blue)](https://github.com/everruns/homebrew-tap)
[![Repo: Agent Friendly](https://img.shields.io/badge/Repo-Agent%20Friendly-blue)](AGENTS.md)

A terminal coding agent built on
[`everruns-runtime`](https://crates.io/crates/everruns-runtime). One binary
that plans, edits, runs, and verifies code in your repository, autonomous by
default, with persistent sessions, agent skills, MCP servers, and editor
integration over the Agent Client Protocol.

![yolop upgrading a Rust CLI, adding JSON output, and running its tests](https://raw.githubusercontent.com/everruns/yolop/main/docs/demo.gif)

## Origin of the name

`yolop` comes from the Ukrainian `Йолоп`: a dummy, fool, or not-too-bright
person. The name was meant to sound clever and funny in Ukrainian while also
describing the agent's starting point: Yolop avoids per-tool approval pop-ups.
Routine workspace work runs automatically; hard prompts are reserved for the
configured shell boundary, with [soft approval](#soft-approval) for critical
moments that need spoken consent and an audit trail.

## Install

```bash
brew install everruns/tap/yolop
```

Works on macOS (arm64/x86_64) and Linux (x86_64). If your Homebrew enforces
tap trust checks, run `brew trust --tap everruns/tap` once first. Building from
source instead? `cargo install yolop --locked`.

## Quick start

```bash
cd your/repo
yolop
```

First launch with no credentials opens a guided, keyboard-driven setup:
provider → credentials → model. Or set a provider key and go:

```bash
OPENAI_API_KEY=sk-… yolop

yolop -C /path/to/repo                  # work in a different workspace
yolop -p "summarize the build setup"    # one-shot, prints to stdout, no TUI
yolop --provider llmsim -p "hi"         # offline demo, no API key required
```

## Features

### Agent core

- **Unrestricted by default**: shell commands get full host access unless you
  opt into `workspace-write` containment with `--sandbox` or the `sandbox_mode`
  setting. A standing write blocklist always rejects writes into `.git/`,
  `node_modules/`, `target/`, and similar build/dependency directories; reads
  inside the workspace stay unrestricted.
- **Shell sandboxing**: sandboxed commands run under Seatbelt on macOS or
  Landlock + seccomp on Linux. Choose a sandbox mode (`read-only`,
  `workspace-write`, or `danger-full-access`, the default) and an approval
  policy (`untrusted`, `on-failure`, `on-request`, the default, or `never`)
  independently. See
  [Shell sandboxing](./docs/features/sandboxing/sandboxing.md).
- **Soft approval**: an optional spoken-consent layer: yolop batches safe work
  and pauses, in plain chat, only before destructive or outward-facing steps;
  you approve by replying "yes". Set the level with
  `/setup approval <protective|normal|off>` or just tell yolop to be more or
  less careful. See [Soft approval](#soft-approval) and
  [Approvals](./docs/features/approvals.md).
- **TUI chat** (ratatui), scrolling transcript with tree-sitter syntax
  highlighting of fenced code blocks, a multiline composer, a live status bar,
  slash commands (`/help`, `/tools`, `/mcp`, `/setup`, `/model`, `/goal`,
  `/shell`, `/background`, …), a `Ctrl+B` activity rail for agents, background
  commands, and monitors with branch usage and cooperative cancellation,
  `!<command>` as a direct shell shortcut,
  `@`-triggered file-path completion, and shell-style
  history recall (`↑`/`↓`, `Ctrl+R`) persisted across sessions. Start with
  `--compact-work` to replace per-turn narration and tool lines with one live
  summary; `Ctrl+O` expands or collapses the retained details. Compact work is
  fullscreen-only because native inline scrollback cannot repaint old rows.
- **Side questions**: `/btw <question>` answers out-of-band using the current
  session context, with no tools and nothing added to conversation history.
- **Goal loops**: `/goal <condition>` keeps working across turns until a
  separate evaluator model confirms the condition; `/goal clear` stops early.
  Works in `--print` mode too.
- **User-ask tracking** *(experimental, off by default)*, records the current
  request across turns and applies bounded completion checks. Enable with
  `[[capabilities]] ref = "yolop_user_ask"` in `settings.toml`.
- **Planning**: `write_todos` keeps multi-step tasks on track, and loop
  detection stops the model from retrying the same failing tool call.
- **One-shot mode**: `--print` runs a single prompt non-interactively, for
  scripts and CI.

### Tools

- **Filesystem**: `read_file`, `write_file`, `edit_file`, `list_directory`,
  `grep_files`, `delete_file`, `stat_file`, backed by the real workspace disk.
- **Repo map**: `repo_map` and `repo_symbols` build an on-demand multi-language
  symbol overview for orientation before targeted grep/read. See
  [Repo map](./docs/features/repo-map/repo-map.md).
- **AST grep**: `ast_grep` runs read-only structural pattern search across
  Rust, Python, TypeScript/TSX, JavaScript/JSX, C#, Go, CSS, HTML, and Bash.
- **AST edit** *(optional, off by default)*, `ast_edit` applies pattern
  rewrites with a preview-first `dry_run` flow. Enable with
  `[[capabilities]] ref = "ast_edit"` in `settings.toml`.
- **LSP** *(optional, off by default)*, real language servers (rust-analyzer,
  typescript-language-server, pyright, gopls, clangd, or any configured binary)
  behind `lsp_diagnostics`, `lsp_definition`, `lsp_references`, `lsp_hover`,
  `lsp_rename` (workspace-wide), `lsp_symbols`, and `lsp_code_actions`. Enable
  with `[[capabilities]] ref = "lsp"` in `settings.toml`.
- **Shell**: `bash -lc` from the workspace root, with a 120 s timeout and a
  1 MiB per-stream output cap (overflow spills to the session folder and stays
  readable for tool calls). Run a command directly with `/shell <command>` or
  `!<command>`.
- **Background tasks**: `spawn_background` runs a shell command detached from
  the current turn (e.g. watching CI): it streams to a log, writes a
  `result.json`, and tracks a session task you inspect with `list_tasks`,
  `get_task`, and `cancel_task` (or the `/background` command and interactive
  `Ctrl+B` activity rail). Detached commands may run up to 24 hours and their results
  survive a restart. `spawn_background` can also schedule one-shot or recurring
  monitors; when a schedule fires or a task finishes while the session is idle,
  yolop proactively wakes the agent (disable with `proactive_wake`).
- **Sub-agents**: `spawn_agent` delegates independent work into child context
  windows. A two-level hierarchy supports broad swarms without exceeding the
  per-session fan-out limit; `Ctrl+B` shows the live activity rail with branch
  token and cost rollups. See [Parallel sub-agents](./docs/features/subagents/subagents.md).
- **Session coordination**: a purpose-built coordinator profile can discover
  live, opt-in Yolop workers in separate Git worktrees, dispatch durable tasks,
  and receive explicit completion wakes through the attached
  `yolop coordination` CLI, without adding model-tool schemas. Presence and
  delivery are local and restart-safe; ordinary sessions start drained. See
  [Session coordination](./docs/session-coordination.md).
- **Web**: `free_web_search`, `web_fetch` (HTTP GET/HEAD with markdown/text
  conversion and DNS-pinned SSRF protection), and `duckduckgo_instant_answer`,
  all working without an API key. Set `EVERRUNS_SYSTEM_ALLOWLIST_ENABLED=true`
  to restrict `web_fetch` to a curated allowlist of well-known public resources.

### Context engineering

- **`AGENTS.md`**: project instructions re-read every turn.
- **Workspace context**: root, shell, local date/timezone, and Git
  identity/branch injected automatically.
- **Memory**: a structured `MEMORY.md` of durable, cross-session memories,
  managed in natural language ("remember that I prefer terse answers") via the
  `remember` / `recall` / `forget` tools. Only titles are injected each turn;
  bodies are recalled on demand, so the prompt stays small however much you
  remember.
- **Skills**: `SKILL.md` files discovered from workspace (`.agents/skills/`),
  global, ephemeral environment, and bundled scopes, exposed via `list_skills`,
  `read_skill`, `write_skill`, `activate_skill`, plus `search_skills` /
  `install_skill` for the public skills.sh registry and `delete_skill` for
  uninstall. Skills installed after startup are available immediately. See
  [Registry skills and Mermaid diagrams](./docs/features/show-me/show-me.md).
- **OKF knowledge**: a bundled `okf` skill for the
  [Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md):
  read, author, convert to, and
  validate knowledge bundles. See [OKF](./docs/features/okf/okf.md).
- **Herdr-aware sessions**: when launched in a Herdr pane, Yolop reports
  `working` / `idle` / `blocked` lifecycle states and exposes read-only Herdr
  guidance without installing a global skill.
- **Infinity context**: older history is trimmed out of the live prompt but
  stays queryable via `query_history`, so long sessions don't hit the wall.
- **Tool search**: core file/shell tools stay loaded while long-tail tools are
  hidden until the model pulls them in, saving input tokens on every provider.
- **Compaction & caching**: at 85% of the model's context window, supported
  providers replace old input with a native compact checkpoint plus the new
  suffix (raw history stays lossless and searchable), and Anthropic
  prompt-caching markers are emitted out of the box.

### Extensibility

- **Extensions**: installable capability packages that add tools, prompt
  guidance, MCP servers, and hooks without rebuilding yolop, over the yolop
  extension protocol (YEP). Install from crates.io with no Rust toolchain, a git
  URL, or a local path with `yolop extensions`; direct foreground invocations
  from a running Yolop session also update that session without exposing
  management tools to the model. The built-in `/extensions` command uses the
  same grammar inside interactive clients. Author one with the
  [`yolop-yep`](./crates/yolop-yep) SDK. See
  [docs/extensions.md](./docs/extensions.md).
- **MCP servers**: extra tools from local (stdio) or remote (HTTP)
  [Model Context Protocol](https://modelcontextprotocol.io) servers. See
  [MCP servers](#mcp-servers).
- **Hooks**: workspace and global hooks can block, mutate, or audit agent
  actions. Configure them by chatting with yolop or by editing `hooks.json`. See
  [docs/features/hooks.md](./docs/features/hooks.md).
- **Editor integration**: `--acp` speaks the
  [Agent Client Protocol](https://agentclientprotocol.com) over stdio, so
  editors such as Zed can drive yolop. See
  [Editor integration](#editor-integration-acp).
- **Sessions & rewind**: a durable per-session event log with auto-generated
  titles; resume any conversation with `--session <id>`. Every turn gets a
  checkpoint, and `/undo`, `/redo`, and `/rewind` restore conversation or
  workspace state without touching `HEAD` or the Git index. See
  [Session persistence](#session-persistence).
- **Session search**: `search_sessions` finds recent local sessions by a
  distinctive phrase, keeping investigations grounded in the saved event log.

### Providers

| Provider   | Credential                            | Default model     |
| ---------- | ------------------------------------- | ----------------- |
| OpenAI     | `OPENAI_API_KEY`                      | `gpt-5.6-sol`     |
| Codex subscription | browser/device ChatGPT login or `CODEX_ACCESS_TOKEN` | `gpt-5.6-sol` |
| Anthropic  | `ANTHROPIC_API_KEY`                   | `claude-opus-4-8` |
| Meta Model API | `MODEL_API_KEY`                   | `muse-spark-1.2`  |
| OpenRouter | browser PKCE login or `OPENROUTER_API_KEY` | `openai/gpt-5.6-sol` |
| Google     | `GEMINI_API_KEY` / `GOOGLE_API_KEY`   | `gemini-2.5-flash` |
| Ollama     | `OLLAMA_BASE_URL` / `OLLAMA_API_KEY`  | `llama3.2`        |
| Local (in-process) | none (weights pulled with `yolop models pull`) | `Qwen3-30B-A3B-Instruct` Q4 GGUF |
| Custom     | `CUSTOM_BASE_URL` (+ optional `CUSTOM_API_KEY`) |, (set via `/setup`) |
| llmsim     | none (offline simulator)              |, |

Pick explicitly with `--provider`, override the model with `-m/--model`. **Codex
subscription** signs in through ChatGPT and uses the Codex backend directly.
**OpenRouter** can mint a user-controlled API key through the same `/setup`
browser login, or you can paste/`OPENROUTER_API_KEY` as before. **Custom** is
any OpenAI-compatible Chat Completions endpoint (vLLM, llama.cpp, LM Studio,
hosted gateways, …).

**Local (in-process)** is experimental. Unlike Ollama it needs no server: the
inference engine is linked into yolop. Model specs are Hugging Face repos
(`Qwen/Qwen3-8B`) or a GGUF file inside one
(`unsloth/Qwen3-8B-GGUF::Qwen3-8B-Q4_K_M.gguf`). Pull the weights before use —
a turn never blocks on a download:

```bash
M=unsloth/Qwen3-8B-GGUF::Qwen3-8B-Q4_K_M.gguf
yolop models pull "$M"      # several GB, with progress
yolop models list           # what is on disk, and how much
yolop models rm "$M"        # reclaim it
yolop --provider local -m "$M"
```

The default is `Qwen3-30B-A3B-Instruct` at Q4 (~19 GB): a mixture of experts
with only ~3B parameters active per token, so it runs at roughly small-model
speed on Apple Silicon, where memory bandwidth rather than compute is the
limit. Budget 32 GB of RAM. `/setup` also lists an 8B at ~5 GB, which is
quicker to pull and quicker to load when you are iterating.

`Qwen3-Coder-30B-A3B` is deliberately *not* the default despite being the
better-aimed model: its chat template emits tool calls as
`<tool_call><function=…>` XML, and the engine only parses JSON inside
`<tool_call>`, so none of its tool calls would be understood.

Weights live under `<data_dir>/yolop/models/`. The engine is compiled into the
Homebrew and GitHub release binaries; a build from source needs `cargo install
yolop --features local-inference`, which is much slower to compile. How well a
local model actually drives the agent loop is the open question this is here to
answer.

Those builds run on the CPU. GPU acceleration needs a vendor toolchain, so it is
a separate feature that implies `local-inference`:

```bash
cargo install yolop --features metal   # macOS
cargo install yolop --features cuda    # NVIDIA, needs the CUDA toolkit
```

### Git attribution

On by default. When yolop creates commits it keeps your git identity and appends
`Co-Authored-By: yolop <yolop@everruns.com>` once; PR descriptions created through
`gh` get a `Produced by [yolop](https://everruns.com/yolop)` footer. Disable with
`/setup attribution off`.

### Soft approval

Soft approval is prompt-engineering, not a hard gate: yolop is told to batch
safe work and pause for **spoken** consent only at critical moments. The model
decides what is critical; you approve in plain language ("yes", "approved"); each
grant is recorded to the session event log for audit. There is no separate
approval UI, consent lives in the conversation.

A single `approval_mode` level tunes how cautious yolop is:

| Level        | yolop pauses before…                                                  |
|--------------|------------------------------------------------------------------------|
| `protective` | any state-changing action (writes, commits, pushes, installs)          |
| `normal`     | only destructive/irreversible or outward-facing actions (the default)  |
| `off`        | nothing, fully autonomous, no soft-approval prompt                     |

The current level shows in the status bar. Change it with
`/setup approval <level>`, or just ask yolop to be more or less careful ("yolo
mode"); the choice persists in `settings.toml`. Soft approval is judgement, not
a guarantee, native shell sandboxing supplies the kernel boundary and hooks can
block specific operations. See
[Shell sandboxing](./docs/features/sandboxing/sandboxing.md) and
[Approvals](./docs/features/approvals.md).

## Editor integration (ACP)

yolop implements the agent side of the [Agent Client
Protocol](https://agentclientprotocol.com). Launch it with `--acp` and it speaks
newline-delimited JSON-RPC 2.0 over stdin/stdout: the editor handshakes, opens a
session, and sends turns; yolop streams back assistant text, reasoning, tool
calls, and plans. Editors can also load an existing session with `session/load`,
replaying the same persisted history used by CLI `--session`.

ACP model pickers list only providers that are currently connected. A stale
saved provider no longer prevents session creation: yolop starts with another
usable provider (or local `llmsim`) and updates the picker live after `/setup`
or authentication changes. Clients that support ACP authentication can launch
Yolop's browser-based ChatGPT/Codex sign-in directly from the agent UI. API-key
providers must receive their keys through the ACP process environment; Yolop
does not accept secrets through ACP prompt text.

An experimental opt-in adds a second route for those providers: an extra ACP
authentication method that opens a short-lived setup page on `127.0.0.1`, where
an API key, a custom endpoint, or a model can be entered in the browser instead
of the editor. The key goes straight into yolop's settings, never through the
editor or the session log. Sessions started with nothing connected also get a
link to that page. Turn it on for one run with `yolop --acp --acp-setup-page`,
or for every launch by setting `acp_setup_page` (ask yolop to turn it on, or use
`/yolop-config`). Editors own the spawn arguments, so the setting is usually the
practical one. It is off by
default, and it needs the editor to be on the same machine as yolop.

To set up Zed:

```bash
yolop into zed
```

That adds a custom ACP agent server to `~/.config/zed/settings.json` using the
current yolop executable (preserving any existing `env` on re-run). Then pick
**yolop** in Zed's agent panel.

To set up Buzz Desktop:

```bash
yolop into buzz
```

That adds a custom harness under Buzz's application data directory using the
current yolop executable and `--acp`. Re-running updates the managed fields while
preserving any custom `env` values and extension fields.

## MCP servers

Yolop pulls in extra tools from MCP servers, remote (Streamable **HTTP**) and
local (**stdio**), configured in the standard `.mcp.json` shape. Two scopes are
merged, workspace overriding global by name:

- **workspace**: `<workspace_root>/.mcp.json`
- **global**: `<config_dir>/yolop/mcp.json` (e.g. `~/.config/yolop/mcp.json`)

```json
{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" }
    },
    "fs": {
      "type": "stdio",
      "command": "mcp-server-filesystem",
      "args": ["${WORKSPACE}"],
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

- `type` defaults to `http`; HTTP servers need a `url`, stdio servers a `command`.
- String values support `${VAR}` expansion from the environment, so secrets stay
  out of the file (an unset `${VAR}` is left as-is).
- Discovered tools are exposed as `mcp_<server>__<tool>`.

Manage servers from the CLI with `yolop mcp`, or from a session with `/mcp`:

```bash
yolop mcp list --scope effective
yolop mcp add linear --scope global --type http --url https://mcp.linear.app/mcp --auth-mode oauth --oauth-provider-id linear
yolop mcp enable linear --scope global
yolop mcp remove linear --scope global
```

In a session, `/mcp` lists startup servers and
`/mcp enable|disable|remove <name> [global|workspace]` updates the config;
changes take effect in the next session.

**Trust model:** HTTP requests keep yolop's DNS-pinned SSRF protection; stdio
servers run local processes you listed yourself, so authoring `.mcp.json` is the
act of consent. MCP tools run autonomously like the rest of yolop's tools.

## Reference

### Flags

| Flag                       | Description                                                          |
| -------------------------- | -------------------------------------------------------------------- |
| `-C, --cwd <PATH>`         | Workspace root (default: current dir)                                |
| `--provider <P>`           | Force `anthropic`, `meta`, `codex`, `openai`, `google`, `openrouter`, `ollama`, `local`, `custom`, or `llmsim` |
| `--profile <NAME>`         | Overlay a named execution profile from the platform config directory |
| `-m, --model <ID>`         | Override the model id for the chosen provider                        |
| `-p, --print <PROMPT>`     | Run one prompt non-interactively and print the result                |
| `--acp`                    | Speak the Agent Client Protocol over stdio (for editors like Zed)    |
| `--session <ID>`           | Resume a previous session by id                                      |
| `--session-dir <PATH>`     | Override the parent directory for session folders                    |
| `--config-dir <PATH>`      | Override the global config directory (settings, profiles, extensions) |
| `--data-dir <PATH>`        | Override the global data directory (sessions, logs, models)          |
| `--sandbox`                | One-run `workspace-write` containment without changing settings      |
| `--compact-work`           | Collapse each fullscreen turn's work into an expandable summary row  |
| `--reasoning-effort <E>`   | Reasoning effort override when the model profile supports it          |
| `--trajectory-out <PATH>`  | Write the session as an [ATIF](https://github.com/harbor-framework/harbor/blob/main/rfcs/0001-trajectory-format.md) v1.7 trajectory JSON at end of run |

### Commands

| Command            | Description                                     |
| ------------------ | ----------------------------------------------- |
| `yolop version`    | Print yolop, commit, and runtime versions       |
| `yolop into zed`   | Configure yolop as a custom ACP agent in Zed    |
| `yolop into buzz`  | Configure yolop as a custom ACP harness in Buzz |
| `yolop mcp …`      | Manage MCP servers (see [MCP servers](#mcp-servers)) |

`RUST_LOG` is honored for the underlying tracing layer. Non-interactive modes
write traces to stderr. Interactive sessions keep terminal rendering clean and
write traces to private, rotating files under `<data_dir>/yolop/logs/` (the five
most recent files are retained, capped at 4 MiB each).

### Provider env vars

| Env var                             | Effect                                                       |
| ----------------------------------- | ------------------------------------------------------------ |
| `OPENAI_API_KEY`                    | Select OpenAI unless `--provider` overrides                  |
| `CODEX_ACCESS_TOKEN`                | Select Codex subscription auth                               |
| `ANTHROPIC_API_KEY`                 | Select Anthropic when OpenAI is not configured               |
| `MODEL_API_KEY`                     | Select Meta Model API when earlier providers are not configured |
| `OPENROUTER_API_KEY`                | Select OpenRouter when earlier providers are not configured  |
| `GEMINI_API_KEY` / `GOOGLE_API_KEY` | Select Google Gemini via its OpenAI-compatible endpoint      |
| `OLLAMA_BASE_URL`                   | Select Ollama, defaults to `http://127.0.0.1:11434/v1`       |
| `CUSTOM_BASE_URL`                   | Select the custom OpenAI-compatible endpoint                 |
| `EVERRUNS_CLI_MODEL`                | Override the auto-selected default model                     |
| `EVERRUNS_CLI_REASONING_EFFORT`     | Reasoning effort override when the model profile supports it  |

Each provider also accepts an optional base-URL / key override
(`OPENROUTER_BASE_URL`, `GOOGLE_BASE_URL`, `OLLAMA_API_KEY`, `CUSTOM_API_KEY`).
At runtime the per-provider env var always beats a saved token, so a per-run
override is always possible.

### Settings

A TOML file at `<config_dir>/yolop/settings.toml` persists your preferred
provider, per-provider model picks, custom base URLs, the soft-approval level,
sandbox mode and approval policy, Codex login metadata, and (optionally) API
tokens (`~/.config/yolop/settings.toml` on Linux,
`~/Library/Application Support/yolop/settings.toml` on macOS,
`%APPDATA%\yolop\settings.toml` on Windows).

You rarely edit it by hand: the TUI's `/setup`, `/model`, and `/effort` commands
update the active provider, keys, model, and reasoning effort, and yolop can read
and edit the file itself through the `get_config` / `set_config` tools or the
`yolop-config` skill (e.g. "use anthropic by default", "store my OpenAI key").
Unknown keys are ignored, so the format stays forward-compatible.

Both global directories can be moved: `--config-dir <PATH>` (settings, profiles,
hooks, extensions) and `--data-dir <PATH>` (sessions, logs, models, prompt
history), or `YOLOP_CONFIG_DIR` / `YOLOP_DATA_DIR` for a whole shell. Point both
somewhere private to run isolated or side-by-side yolop identities without
moving `HOME` (global skills stay in the cross-agent `~/.agents/skills`, which
`YOLOP_GLOBAL_SKILLS_DIR` moves separately):

```bash
yolop --config-dir ~/agents/triage/config --data-dir ~/agents/triage/data
```

Named profiles are sparse execution overlays stored under
`<config_dir>/yolop/profiles/<name>.toml` and selected explicitly with
`--profile <name>`. A profile bundles provider/model choices, endpoint base
URLs, approval and sandbox settings, worktree behavior, the harness capability
set (which is also how extensions are enabled or disabled), MCP servers, extra
skills, and standing instructions, so one profile can define a purpose-built
agent without duplicating global settings:

```toml
# profiles/triage.toml
default_provider = "openai"
approval_policy = "never"
sandbox_mode = "read-only"
instructions_file = "triage/INSTRUCTIONS.md"

[models]
openai = "gpt-5.6 xhigh"

# Only these MCP servers, not the global set.
mcp_mode = "replace"

[mcp.servers.inbox]
type = "http"
url = "https://inbox.example/mcp"

# Turn an extension on for this profile and a default capability off.
[[capabilities]]
ref = "ext:triage"

[[capabilities]]
ref = "yolop_skill_management"
enabled = false

# Make this profile a coordinator for opt-in local Yolop workers.
[[capabilities]]
ref = "session_coordination"
role = "coordinator"
accept_work = false
```

Structural keys layer by default: `[[capabilities]]` entries run after the
global ones and `[mcp.servers.<name>]` entries merge by name. Set
`capabilities_mode = "replace"` or `mcp_mode = "replace"` when the profile
should be the whole set. `instructions` (inline) and `instructions_file` are
appended to the system prompt. Skills in `profiles/<name>/skills/` (or a
`skills_dir` you name) become a scope ranked between the workspace and global
ones. Paths resolve relative to the profiles directory.

CLI and ACP live model choices override the selected profile; the profile
overrides `settings.toml`. Credentials, theme, attribution, and background-wake
behavior remain global and are rejected in a profile. When a profile is active,
`/setup`, `set_config`, and extension enable/disable persist back to that
profile while credential changes still update the global settings file.
Workspace `.mcp.json` still overlays a profile's servers by name, and `yolop
mcp` / `/mcp` writes stay scoped to global settings or the workspace file.

**Provider resolution at startup:** `--provider` wins, then the saved
profile `default_provider`, then the global `default_provider`, then auto-detect in the order **OpenAI → Codex → Anthropic → Meta →
OpenRouter → Google → Ollama → Custom** (first with a matching env var or saved
credential), then a fall back to OpenAI's default with setup opened.

The settings file is written `0o600` on Unix and token values are never echoed,
but it is plain text on disk, treat it like `~/.aws/credentials`.

### Session persistence

Every run writes a durable per-session folder under the platform user-data
directory:

| OS      | Default location                                                 |
|---------|------------------------------------------------------------------|
| Linux   | `$XDG_DATA_HOME/yolop/sessions/<session_id>/` (typically `~/.local/share/…`) |
| macOS   | `~/Library/Application Support/yolop/sessions/<session_id>/` |
| Windows | `%APPDATA%\yolop\sessions\<session_id>\`                   |

The folder holds the event log (`events.jsonl`), compaction checkpoints, the
saved workspace root, and spilled tool output, everything needed to restore the
transcript and provider continuation state on resume. On Unix the folder is
`0o700` and its files `0o600`. Resume a conversation with:

```bash
yolop --session session_019e3db018a17450aba5407af5777237
```

Resume reuses the workspace root saved for that session unless you pass
`-C <PATH>`; `--session-dir <PATH>` overrides where session folders are stored
(handy for per-workspace histories in `<workspace>/.yolop/sessions/`).

Interactive sessions checkpoint before every turn. `/undo` previews a restore to
before the latest turn, `/rewind` lists and restores earlier checkpoints, and
`/redo` previews the last abandoned branch; each restore returns a token you
confirm with `confirm <token>`. Workspace restore is available only in a
Yolop-owned worktree and never changes the branch, `HEAD`, commits, or index.

> **Treat session logs as you would shell history.** They contain every prompt,
> every string passed to `bash` / `write_file` / `web_fetch`, tool output, and
> reasoning artifacts, deliberately unredacted, with no retention policy. If a
> session should not be persisted, point `--session-dir` at a wipeable path (e.g.
> a `tmpfs`) or delete the JSONL after the run.

### Crash reports

If Yolop panics, it writes a local report containing the version, bounded panic
message, source location, and bounded backtrace. Yolop does not add prompts,
tool payloads, environment variables, credentials, or tracing logs. Because a
panic message can still contain application data, review a report before
sharing it. Yolop keeps the newest five:

| OS      | Default location                                      |
|---------|-------------------------------------------------------|
| Linux   | `$XDG_DATA_HOME/yolop/crashes/`                       |
| macOS   | `~/Library/Application Support/yolop/crashes/`        |
| Windows | `%APPDATA%\yolop\crashes\`                            |

On Unix the directory is `0o700` and reports are `0o600`. After restoring the
terminal, Yolop prints the report path before preserving the original panic.

## Contributing

Development setup, validation commands, and local smoke tests live in
[`CONTRIBUTING.md`](./CONTRIBUTING.md). Please report vulnerabilities through
[`SECURITY.md`](./SECURITY.md), and follow the
[`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) when participating.

## Benchmarking

[`evals/`](./evals/) holds yolop's [Mira](https://github.com/everruns/mira) eval
studies. [`evals/swebench_verified/`](./evals/swebench_verified/) benchmarks
yolop on **SWE-bench Verified** and, behind the same interface, other terminal
agents for head-to-head comparison. See [`evals/README.md`](./evals/README.md).

## Releases

Yolop ships to the `everruns/homebrew-tap` Homebrew tap and to crates.io as the
`yolop` crate. See [`CHANGELOG.md`](./CHANGELOG.md) for what shipped in each
version.

## License

MIT, see [`LICENSE`](./LICENSE).

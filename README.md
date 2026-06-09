<p align="center">
  <img src="https://raw.githubusercontent.com/everruns/yolop/main/logo.svg" width="96" alt="Yolop logo">
</p>

# Yolop

A terminal coding agent built on
[`everruns-runtime`](https://crates.io/crates/everruns-runtime). One binary
that plans, edits, runs, and verifies code in your repository — autonomous by
default, with persistent sessions, agent skills, MCP servers, and editor
integration over the Agent Client Protocol.

![yolop upgrading a project's dependencies](https://raw.githubusercontent.com/everruns/yolop/main/assets/demo.gif)

## Install

```bash
brew install everruns/tap/yolop
```

Works on macOS (arm64/x86_64) and Linux (x86_64). If your Homebrew enforces
tap trust checks, trust the tap once first with `brew trust --tap everruns/tap`.
Building from source instead? `cargo install yolop --locked`.

## Quick start

```bash
cd your/repo
yolop
```

First launch with no credentials opens a guided, keyboard-driven setup:
provider → API key → model. Or set a provider key and go:

```bash
OPENAI_API_KEY=sk-… yolop

yolop -C /path/to/repo        # work in a different workspace
yolop -p "summarize the build setup"   # one-shot, no TUI, prints to stdout
yolop --provider llmsim -p "hi"        # offline demo, no API key required
```

## Features

### Agent core

- **Autonomous by default** — yolop runs writes, edits, deletes, and bash
  commands without prompting. A standing **write blocklist** rejects writes
  into `.git/`, `node_modules/`, `target/`, `dist/`, `build/`, `.next/`,
  `.venv/`, `venv/`, `.tox/`, `.gradle/` at any depth; reads are unrestricted
  inside the workspace.
- **TUI chat** (ratatui): scrolling transcript, multiline composer, status
  bar, slash commands (`/help`, `/tools`, `/mcp`, `/cwd`, `/setup`, `/model`,
  `/effort`, `/clear`, `/quit`).
- **Planning** — `write_todos` keeps multi-step tasks on track, and
  loop detection stops the model from retrying the same failing tool call.
- **One-shot mode** — `--print` runs a single prompt non-interactively, for
  scripts and CI.

### Tools

- **Filesystem** — `read_file`, `write_file`, `edit_file`, `list_directory`,
  `grep_files`, `delete_file`, `stat_file`, backed by the real workspace disk.
- **Shell** — `bash -lc` from the workspace root, with a 120 s wall-clock
  timeout and per-stream 1 MiB output cap; large output is spilled to disk
  under the session folder and stays readable.
- **Web** — `web_fetch` (HTTP GET/HEAD with markdown/text conversion, DNS-pinned
  SSRF protection) and `duckduckgo_search` (free, no API key).

### Context engineering

- **`AGENTS.md`** — project instructions re-read every turn.
- **Workspace context** — root, shell, local date/timezone, Git identity and
  branch injected automatically.
- **Memory** — a central `MEMORY.md` of durable, cross-session user
  preferences, edited in natural language ("remember that I prefer terse
  answers") and injected every turn under a managed size budget. See
  [`specs/your.md`](./specs/your.md).
- **Skills** — `SKILL.md` files discovered from workspace
  (`.agents/skills/`), global (`<config_dir>/yolop/skills/`), and system
  (bundled) scopes, exposed via `list_skills` / `activate_skill`. See
  [`specs/skills.md`](./specs/skills.md).
- **Infinity context** — older history is trimmed out of the live prompt but
  stays queryable via `query_history`, so long sessions don't hit the wall.
- **Tool search** — provider-agnostic deferred tool loading: core file/shell
  tools stay loaded, long-tail tools are hidden until the model pulls them in
  on demand, saving input tokens on every provider. See
  [`specs/tool-search.md`](./specs/tool-search.md).
- **Prompt caching** — Anthropic prompt-caching markers out of the box.

### Extensibility

- **MCP servers** — extra tools from local (stdio) or remote (HTTP)
  [Model Context Protocol](https://modelcontextprotocol.io) servers via
  `.mcp.json` (see [MCP servers](#mcp-servers)).
- **Editor integration** — `--acp` speaks the
  [Agent Client Protocol](https://agentclientprotocol.com) over stdio, so
  editors such as Zed can drive yolop as an external agent (see
  [Editor integration](#editor-integration-acp)).
- **Sessions** — every run writes a durable per-session event log; resume any
  conversation with `--session <id>` (see
  [Session persistence](#session-persistence)).

### Providers

| Provider   | Credential                            | Default model     |
| ---------- | ------------------------------------- | ----------------- |
| OpenAI     | `OPENAI_API_KEY`                      | `gpt-5.5`         |
| Anthropic  | `ANTHROPIC_API_KEY`                   | `claude-sonnet-4-5` |
| OpenRouter | `OPENROUTER_API_KEY`                  | `openai/gpt-5.2`  |
| Google     | `GEMINI_API_KEY` / `GOOGLE_API_KEY`   | `gemini-2.5-flash` |
| Ollama     | `OLLAMA_BASE_URL` / `OLLAMA_API_KEY`  | `llama3.2`        |
| llmsim     | none (offline simulator)              | —                 |

Pick explicitly with `--provider`, override the model with `-m/--model`.

### Git attribution

Enabled by default and configurable. When yolop creates commits, it keeps
your git author/committer identity and appends
`Co-Authored-By: yolop <yolop@everruns.com>` once. PR descriptions created or
edited through `gh` get a `Generated with yolop` footer. Disable with
`/setup attribution off`.

## Editor integration (ACP)

yolop implements the agent side of the [Agent Client
Protocol](https://agentclientprotocol.com). Launch it with `--acp` and it
speaks newline-delimited JSON-RPC 2.0 over stdin/stdout: the editor performs
the `initialize` handshake, opens a session with `session/new`, and sends
turns with `session/prompt`; yolop streams back assistant text, reasoning,
tool calls, and plans as `session/update` notifications.

To set up Zed:

```bash
yolop into zed
```

That adds a custom ACP agent server to `~/.config/zed/settings.json` using
the current yolop executable, preserving any existing `env` and extra
settings on re-run. Then pick **yolop** in Zed's agent panel. See
[`specs/acp.md`](./specs/acp.md) for the full protocol surface, mappings, and
current limitations.

## MCP servers

Yolop pulls in extra tools from MCP servers — remote (Streamable **HTTP**)
and local (**stdio**, a child process) — configured in the standard
`.mcp.json` shape every MCP client understands. Two scopes are read and
merged (workspace overrides global by name):

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

- `type` defaults to `http`; HTTP servers need a `url`, stdio servers need a
  `command`.
- String values support `${VAR}` expansion from the environment, so secrets
  stay out of the file (an unset `${VAR}` is left as-is so it's easy to spot).
- Discovered tools are exposed to the model as `mcp_<server>__<tool>`;
  `/mcp` lists the configured servers.

Trust model: HTTP requests keep yolop's DNS-pinned SSRF protection; stdio
servers run local processes you listed yourself, so authoring `.mcp.json` is
the act of consent. MCP tools run autonomously like the rest of yolop's
tools. See [`specs/mcp.md`](specs/mcp.md).

## Reference

### Flags

| Flag                       | Description                                                          |
| -------------------------- | -------------------------------------------------------------------- |
| `-C, --cwd <PATH>`         | Workspace root (default: current dir)                                |
| `--provider <P>`           | Force `anthropic`, `openai`, `google`, `openrouter`, `ollama`, or `llmsim` |
| `-m, --model <ID>`         | Override the model id for the chosen provider                        |
| `-p, --print <PROMPT>`     | Run one prompt non-interactively and print the result                |
| `--acp`                    | Speak the Agent Client Protocol over stdio (for editors like Zed)    |
| `--session <ID>`           | Resume a previous session by id                                      |
| `--session-dir <PATH>`     | Override the parent directory for session folders                    |
| `--reasoning-effort <E>`   | OpenAI reasoning effort (`low` / `medium` / `high`)                  |

### Commands

| Command            | Description                                     |
| ------------------ | ----------------------------------------------- |
| `yolop version`    | Print yolop, commit, and runtime versions       |
| `yolop into zed`   | Configure yolop as a custom ACP agent in Zed    |

`RUST_LOG` is honored for the underlying tracing layer (writes to stderr).

### Provider env vars

| Env var                         | Effect                                                       |
| ------------------------------- | ------------------------------------------------------------ |
| `OPENAI_API_KEY`                | Select OpenAI unless `--provider` overrides                  |
| `ANTHROPIC_API_KEY`             | Select Anthropic when OpenAI is not configured               |
| `OPENROUTER_API_KEY`            | Select OpenRouter when OpenAI/Anthropic are not configured   |
| `OPENROUTER_BASE_URL`           | Optional, defaults to `https://openrouter.ai/api/v1`         |
| `GEMINI_API_KEY` / `GOOGLE_API_KEY` | Select Google Gemini via its OpenAI-compatible endpoint  |
| `GOOGLE_BASE_URL`               | Optional, defaults to `https://generativelanguage.googleapis.com/v1beta/openai` |
| `OLLAMA_BASE_URL`               | Select Ollama, defaults to `http://localhost:11434/v1`       |
| `OLLAMA_API_KEY`                | Optional, defaults to `ollama` for local Ollama              |
| `EVERRUNS_CLI_MODEL`            | Override the auto-selected default model                     |
| `EVERRUNS_CLI_REASONING_EFFORT` | OpenAI-only reasoning effort override                        |

### Settings

A small TOML settings file persists the preferred provider and (optionally)
provider API tokens across runs: `<config_dir>/yolop/settings.toml` —
`~/.config/yolop/settings.toml` on Linux,
`~/Library/Application Support/yolop/settings.toml` on macOS,
`%APPDATA%\yolop\settings.toml` on Windows.

The TUI's `/setup`, `/model`, and `/effort` commands update the active
provider, saved API keys, current model, OpenAI reasoning effort, or offline
demo mode.

Provider resolution at startup:

1. `--provider` flag (always wins)
2. Saved `provider` setting
3. Auto-detect: the first provider in the order **OpenAI → Anthropic →
   OpenRouter → Google → Ollama** with either a matching env var or a saved
   token (the provider order decides the tiebreak, not the credential source)
4. Fall back to OpenAI's default model and open setup so a provider/API key
   can be configured

At runtime, the per-provider env var (`OPENAI_API_KEY`, etc.) always beats
the saved token, so a per-run env override is always possible.

`/setup` can store an API token under `[tokens]` in the settings file. The
file is written with `0o600` on Unix (owner-only) and stored token values are
never echoed — but it is plain text on disk, so treat it the same way you
would `~/.aws/credentials`.

### Session persistence

Every run writes a durable per-session folder under the platform-native user
data directory:

| OS      | Default location                                                 |
|---------|------------------------------------------------------------------|
| Linux   | `$XDG_DATA_HOME/yolop/sessions/<session_id>/` (typically `~/.local/share/…`) |
| macOS   | `~/Library/Application Support/yolop/sessions/<session_id>/` |
| Windows | `%APPDATA%\yolop\sessions\<session_id>\`                   |

The event log lives at `<session_folder>/events.jsonl`; large tool output is
spilled under `<session_folder>/outputs/`. On Unix the session folder is
`0o700` and the log `0o600` (owner-only). The log keeps everything needed to
restore the transcript and provider continuation state on resume — including
prompts, tool arguments and output, and reasoning artifacts.

To continue a previous conversation:

```bash
yolop --session session_019e3db018a17450aba5407af5777237
```

`--session-dir <PATH>` overrides the parent storage location (useful for
keeping per-workspace session histories in `<workspace>/.yolop/sessions/`).

**Treat session logs as you would shell history.** They contain every prompt
you typed, every string passed to `bash` / `write_file` / `web_fetch`, tool
output, and reasoning artifacts — deliberately unredacted, because providers
need them to resume encrypted reasoning across sessions. There is no
retention policy or rotation. If a session should not be persisted, point
`--session-dir` at a path you can wipe (e.g. a `tmpfs`) or delete the JSONL
after the run.

## Contributing

Development setup, validation commands, and local smoke tests live in
[`CONTRIBUTION.md`](./CONTRIBUTION.md).

Please report vulnerabilities through [`SECURITY.md`](./SECURITY.md), and follow
the project [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) when participating.

## Releases

Yolop ships to the `everruns/homebrew-tap` Homebrew tap and to crates.io as
the `yolop` crate. See [`specs/release.md`](./specs/release.md) for the
release procedure and [`CHANGELOG.md`](./CHANGELOG.md) for what shipped in
each version.

## License

MIT — see [`LICENSE`](./LICENSE).

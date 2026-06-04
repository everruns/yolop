# Yolop

A minimal terminal coding agent, built on
[`everruns-runtime`](https://crates.io/crates/everruns-runtime). Think codex /
claude-code in spirit, embedded as a single binary that talks to your codebase
through a curated set of in-process capabilities.

Yolop is a friendly promotion of the `examples/coding-cli` example from the
[`everruns/everruns`](https://github.com/everruns/everruns) workspace into a
standalone, releasable project. The original example still lives upstream and
serves as the reference embedder for the public runtime crate. Yolop is where
active development happens.

```text
$ yolop --provider openai -p "list the top-level files in this repo"
› list the top-level files in this repo
workspace  /home/me/code/some-repo
provider   openai/gpt-5.5
tools      read_file, write_file, edit_file, list_directory, grep_files,
           delete_file, stat_file, bash, web_fetch, duckduckgo_search,
           write_todos, query_history, list_skills, activate_skill, …
session    session_019e3db018a17450aba5407af5777237 (folder: …; log: …)
…
• Top-level files: AGENTS.md, Cargo.toml, LICENSE, README.md, …
```

## Features

- **TUI** chat (ratatui): scrolling transcript, multiline composer, status
  bar, optional modal approval bar.
- **Real-filesystem tools** through the built-in `session_file_system`
  capability layered over `RealDiskFileStore`: `read_file`, `write_file`,
  `edit_file`, `list_directory`, `grep_files`, `delete_file`, `stat_file`.
- **Host bash tool** — `bash -lc` from the workspace root, with a 120 s
  wall-clock timeout and per-stream 1 MiB output cap.
- **Curated capabilities** wired beyond the filesystem:
  - `code_environment_context` — workspace root, shell, local date/timezone,
    Git identity and branch.
  - `agent_instructions` — re-reads `AGENTS.md` every turn.
  - `your` — global personalization. Maintains a central `MEMORY.md`
    (`<config_dir>/yolop/MEMORY.md`) of durable, cross-session user
    preferences, injected every turn under a managed size budget. Edited in
    natural language ("remember that I prefer terse answers", "what is your
    config?") via `remember_your_memory` / `read_your_memory` /
    `write_your_memory`. See
    [`specs/your.md`](./specs/your.md).
  - `skills` — discovers `SKILL.md` files and exposes `list_skills` /
    `activate_skill`. Three scopes are merged (most-specific wins):
    **workspace** (`<workspace>/.agents/skills/<name>/`), **global**
    (`<config_dir>/yolop/skills/<name>/`, installed once per user), and
    **system** (pre-packed with the binary). See [`specs/skills.md`](specs/skills.md).
  - `infinity_context` — trims older history out of the live prompt while
    keeping it queryable via `query_history`.
  - `stateless_todo_list` — `write_todos` for multi-step tasks.
  - `loop_detection` — safety net against the model retrying the same failing
    tool call.
  - `prompt_caching` — Anthropic prompt-caching markers.
  - `duckduckgo` — `duckduckgo_search`, free, no API key.
  - `web_fetch` — HTTP GET/HEAD with optional markdown/text conversion.
  - `tool_output_persistence` — large bash output spilled to disk under
    `/outputs/` inside the current session folder.
- **Approval prompts (opt-in via `--ask`)**. Off by default: yolop acts
  autonomously. `--ask` prompts y/n before every write/edit/delete and every
  bash command, with a unified diff for writes. `--print` mode always
  auto-approves.
- **Write blocklist**: writes into `.git/`, `node_modules/`, `target/`,
  `dist/`, `build/`, `.next/`, `.venv/`, `venv/`, `.tox/`, `.gradle/` are
  rejected at any depth. Read access is unrestricted inside the workspace.
- **Multi-provider** via env vars and `--provider`:
  - `OPENAI_API_KEY` → OpenAI (`gpt-5.5`)
  - `ANTHROPIC_API_KEY` → Anthropic (`claude-sonnet-4-5`)
  - `OPENROUTER_API_KEY` → OpenRouter (`openai/gpt-5.2` by default)
  - `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) → Google Gemini (`gemini-2.5-flash`)
  - `OLLAMA_BASE_URL` / `OLLAMA_API_KEY` → Ollama (`llama3.2`)
- **Slash commands** (TUI): `/help`, `/tools`, `/cwd`, `/setup`, `/model`,
  `/effort`, `/clear`, `/quit`.
- **Guided setup**: launching yolop with no env vars and no saved
  settings opens a keyboard-driven setup overlay for provider → API key →
  model. Re-running `/setup` opens the same provider setup flow, `/model`
  opens the model picker directly, `/model <id>` opens it prefilled, and
  `/effort [level]` opens the OpenAI reasoning-effort picker.
- **`--print`** one-shot mode for CI smoke tests.
- **`--acp`** — speak the [Agent Client Protocol](https://agentclientprotocol.com)
  over stdio so editors such as Zed can drive yolop as an external agent:
  streaming message/thought chunks, tool calls, plans, and editor-mediated
  permission prompts. See [`specs/acp.md`](./specs/acp.md).
- **Session persistence** — durable per-session JSONL event log under the
  platform-native user data directory, with `--session <id>` to resume.
- **Git attribution** — enabled by default and configurable. When yolop creates
  commits, it keeps the user's git author/committer identity and appends
  `Co-Authored-By: yolop <yolop@everruns.com>` once. Pull request descriptions
  created or edited through `gh` get a `Generated with yolop` footer.

## Install

With Homebrew (macOS arm64/x86_64, Linux x86_64):

```bash
brew install everruns/tap/yolop
```

If Homebrew tap trust checks are enabled, trust the Yolop formula once before
rerunning the install command:

```bash
brew trust --formula everruns/tap/yolop
brew install everruns/tap/yolop
```

To trust every formula, cask, and command from the Everruns tap instead:

```bash
brew trust --tap everruns/tap
brew install everruns/tap/yolop
```

From crates.io:

```bash
cargo install yolop --locked
```

From git:

```bash
cargo install --git https://github.com/everruns/yolop --locked
```

Or, from a local clone:

```bash
cargo install --path . --locked
```

That drops the `yolop` binary into `~/.cargo/bin/` (or, for the Homebrew
install, the Homebrew prefix on your `$PATH`).

## Run

Interactive TUI in the current repo:

```bash
yolop
# or without installing:
cargo run
```

Against a different workspace:

```bash
yolop -C /path/to/repo
```

One-shot prompt (no TUI, prints to stdout):

```bash
yolop --provider anthropic -p "list the top-level crates and summarize each in one line."
```

With Doppler secrets:

```bash
doppler run -- yolop -p "Show me the README."
```

Offline (no API key required):

```bash
yolop --provider llmsim -p "hi"
```

OpenRouter, using its OpenAI-compatible Responses endpoint:

```bash
OPENROUTER_API_KEY=sk-or-... yolop --provider openrouter -m openai/gpt-5.2 -p "hi"
```

Local Ollama:

```bash
OLLAMA_BASE_URL=http://localhost:11434/v1 yolop --provider ollama -m llama3.2 -p "hi"
```

## Editor integration (ACP)

yolop implements the agent side of the [Agent Client
Protocol](https://agentclientprotocol.com), so any ACP-compatible editor can
drive it. Launch it with `--acp` and it speaks newline-delimited JSON-RPC 2.0
over stdin/stdout (logs still go to stderr). The editor performs the
`initialize` handshake, opens a session with `session/new`, and sends turns
with `session/prompt`; yolop streams the turn back as `session/update`
notifications (assistant text, reasoning, tool calls, plans) and asks the
editor to approve destructive operations via `session/request_permission`.

To set up Zed:

```bash
yolop into zed
```

That adds a custom ACP agent server to `~/.config/zed/settings.json` using the
current yolop executable. Re-running the command updates the existing `yolop`
entry while preserving its `env` and other extra settings.

Manual equivalent:

```json
{
  "agent_servers": {
    "yolop": {
      "type": "custom",
      "command": "yolop",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Then pick **yolop** in Zed's agent panel. The working directory and per-turn
prompts come from the editor. See [`specs/acp.md`](./specs/acp.md) for the
full protocol surface, mappings, and current limitations.

## Flags

| Flag                       | Description                                                          |
| -------------------------- | -------------------------------------------------------------------- |
| `-C, --cwd <PATH>`         | Workspace root (default: current dir)                                |
| `--provider <P>`           | Force `anthropic`, `openai`, `google`, `openrouter`, `ollama`, or `llmsim` |
| `-m, --model <ID>`         | Override the model id for the chosen provider                        |
| `-p, --print <PROMPT>`     | Run one prompt non-interactively and print the result                |
| `--acp`                    | Speak the Agent Client Protocol over stdio (for editors like Zed)    |
| `--ask`                    | Prompt before every destructive tool call (off by default)           |
| `--session <ID>`           | Resume a previous session by id                                      |
| `--session-dir <PATH>`     | Override the parent directory for session folders                    |
| `--reasoning-effort <E>`   | OpenAI reasoning effort (`low` / `medium` / `high`)                  |

## Commands

| Command            | Description                                     |
| ------------------ | ----------------------------------------------- |
| `yolop into zed`   | Configure yolop as a custom ACP agent in Zed    |

`RUST_LOG` is honored for the underlying tracing layer (writes to stderr).

## Provider env vars

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

## Settings

A small TOML settings file persists the preferred provider and (optionally)
provider API tokens across runs. It lives at `<config_dir>/yolop/settings.toml`
— `~/.config/yolop/settings.toml` on Linux,
`~/Library/Application Support/yolop/settings.toml` on macOS,
`%APPDATA%\yolop\settings.toml` on Windows.

The TUI's `/setup`, `/model`, and `/effort` commands can update the active
provider, saved API keys, current model, OpenAI reasoning effort, or offline
demo mode. Saved provider/API-key choices are written to this file.

Provider resolution at startup:

1. `--provider` flag (always wins)
2. Saved `provider` setting
3. Auto-detect: the first provider in the order **OpenAI → Anthropic →
   OpenRouter → Google → Ollama** for which *either* a matching env var
   *or* a saved token is present. Env vars and saved tokens are treated
   as equivalent credential signals here — the provider order decides
   the tiebreak, not the credential source.
4. Fall back to OpenAI's default model if nothing matches, then open
   setup so a provider/API key can be configured. `llmsim` remains
   available explicitly via `--provider llmsim` or from `/setup`.

At runtime, the per-provider env var (`OPENAI_API_KEY`, etc.) always
beats the saved token, so a per-run env override is always possible.
The setup wizard can also switch models for the current session. OpenAI
reasoning effort can be changed at runtime with the `/effort` modal or
`/setup effort <level>` (for example, `high` or `medium`).

Attribution is enabled by default. Run `/setup attribution off` to disable it
for future turns and future sessions, or `/setup attribution on` to re-enable
it. The saved TOML key is `attribution = false` when disabled.

### Storing tokens

`/setup` can store an API token under `[tokens]` in the settings file.
The file is written with `0o600` on Unix (owner-only). Stored token values
are not echoed.

Env vars still win at runtime: if both `OPENAI_API_KEY` is set and a token
is saved, the env var is used. Setup answers are not echoed into the
transcript or session log, so pasting a key into `/setup` is safer than
typing it at a chat prompt — but the resulting settings file is plain text
on disk, so treat it the same way you would `~/.aws/credentials`.

## Session persistence

Every run writes a durable per-session folder under the platform-native user
data directory:

| OS      | Default location                                                 |
|---------|------------------------------------------------------------------|
| Linux   | `$XDG_DATA_HOME/yolop/sessions/<session_id>/` (typically `~/.local/share/…`) |
| macOS   | `~/Library/Application Support/yolop/sessions/<session_id>/` |
| Windows | `%APPDATA%\yolop\sessions\<session_id>\`                   |

The event log lives at `<session_folder>/events.jsonl`. Tool output persisted
by `tool_output_persistence` lives under `<session_folder>/outputs/`. On Unix
`events.jsonl` is created with `0o600` and its parent session folder is set
to `0o700` (both owner-only) because session logs contain user prompts, tool
arguments, tool output, and the reasoning artifacts discussed below.

The event types kept on disk are those that round-trip into the
conversation (`input.message`, `output.message.completed`,
`tool.completed`) plus the agent reasoning artifacts yolop needs to
restore the live transcript view and provider continuation state on
resume (`reason.completed` carries the safe `text_preview` narration;
`reason.item` carries opaque/encrypted reasoning context curated by the
provider, such as OpenAI Responses reasoning items). Assistant
`thinking` / `thinking_signature` are persisted alongside
`output.message.completed` — providers that resume via encrypted
reasoning continuation (e.g. OpenAI Responses replays
`thinking_signature` as `encrypted_content`) cannot continue without
them. Streaming `*.delta` events and lifecycle markers
(`reason.started`, `reason.thinking.*`, `output.message.started`) are
dropped from the log — they are live status signals only and the delta
types would inflate the file O(n²) without adding resume value.

This persistence contract is **local-store**, not user-facing transcript
export. On Unix, the per-session folder is set to `0o700` and the
`events.jsonl` file inside it to `0o600` on every open, both under the
platform-native user data directory; treat the folder contents as
sensitive (see [Sensitivity](#sensitivity) below).

To continue a previous conversation:

```bash
yolop --session session_019e3db018a17450aba5407af5777237
```

`--session-dir <PATH>` overrides the parent storage location (useful for
keeping per-workspace session histories in `<workspace>/.yolop/sessions/`).

### Sensitivity

**Treat session logs as you would shell history.** Each line is a serialized
event from a turn, which may include:

- every prompt you typed
- tool call arguments — paths and any string passed to `bash`, `write_file`,
  `edit_file`, `web_fetch`, etc.
- tool output — `bash` stdout/stderr, file contents, HTTP response bodies
- agent reasoning artifacts — `reason.completed.text_preview` narration,
  `reason.item` opaque/encrypted reasoning context, and the `thinking` /
  `thinking_signature` fields on assistant messages. Persisting these is
  what lets `--session <id>` resume restore the transcript view and lets
  providers (e.g. OpenAI Responses) continue encrypted reasoning across
  resumes; they are deliberately not redacted from the local log.

There is no retention policy or rotation. If a session should not be
persisted, point `--session-dir` at a path you can wipe (e.g. a `tmpfs`) or
delete the JSONL after the run.

## Contributing

Development setup, validation commands, and local smoke tests live in
[`CONTRIBUTION.md`](./CONTRIBUTION.md).

Please report vulnerabilities through [`SECURITY.md`](./SECURITY.md), and follow
the project [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) when participating.

## Releases

Yolop is released to two registries:

- crates.io as the `yolop` crate (`cargo install yolop --locked`)
- the `everruns/homebrew-tap` Homebrew tap (`brew install everruns/tap/yolop`)

Releases are prepared by asking an agent ("cut release vX.Y.Z" / `/release`),
which opens a `chore(release): prepare vX.Y.Z` PR. After a human squash-
merges, [`.github/workflows/release.yml`](./.github/workflows/release.yml)
tags the commit, creates the GitHub Release, and dispatches the publish and
binary-build workflows. See [`specs/release.md`](./specs/release.md) for the
full procedure and [`CHANGELOG.md`](./CHANGELOG.md) for what shipped in
each version.

## License

MIT — see [`LICENSE`](./LICENSE).

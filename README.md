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

- **TUI** chat (ratatui): scrolling transcript, single-line composer, status
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
  - `skills` — discovers `SKILL.md` files under `.agents/skills/<name>/`;
    exposes `list_skills` / `activate_skill`.
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
  - `OLLAMA_BASE_URL` / `OLLAMA_API_KEY` → Ollama (`llama3.2`)
  - otherwise `llmsim` (offline simulator, no key required)
- **Slash commands** (TUI): `/help`, `/tools`, `/cwd`, `/model <provider>/<id>`,
  `/clear`, `/quit`.
- **`--print`** one-shot mode for CI smoke tests.
- **Session persistence** — durable per-session JSONL event log under the
  platform-native user data directory, with `--session <id>` to resume.

## Install

From source:

```bash
cargo install --git https://github.com/everruns/yolop --locked
```

Or, from a local clone:

```bash
cargo install --path . --locked
```

That drops the `yolop` binary into `~/.cargo/bin/`.

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

## Flags

| Flag                       | Description                                                          |
| -------------------------- | -------------------------------------------------------------------- |
| `-C, --cwd <PATH>`         | Workspace root (default: current dir)                                |
| `--provider <P>`           | Force `anthropic`, `openai`, `openrouter`, `ollama`, or `llmsim`     |
| `-m, --model <ID>`         | Override the model id for the chosen provider                        |
| `-p, --print <PROMPT>`     | Run one prompt non-interactively and print the result                |
| `--ask`                    | Prompt before every destructive tool call (off by default)           |
| `--session <ID>`           | Resume a previous session by id                                      |
| `--session-dir <PATH>`     | Override the parent directory for session folders                    |
| `--reasoning-effort <E>`   | OpenAI reasoning effort (`low` / `medium` / `high`)                  |

`RUST_LOG` is honored for the underlying tracing layer (writes to stderr).

## Provider env vars

| Env var                         | Effect                                                       |
| ------------------------------- | ------------------------------------------------------------ |
| `OPENAI_API_KEY`                | Select OpenAI unless `--provider` overrides                  |
| `ANTHROPIC_API_KEY`             | Select Anthropic when OpenAI is not configured               |
| `OPENROUTER_API_KEY`            | Select OpenRouter when OpenAI/Anthropic are not configured   |
| `OPENROUTER_BASE_URL`           | Optional, defaults to `https://openrouter.ai/api/v1`         |
| `OLLAMA_BASE_URL`               | Select Ollama, defaults to `http://localhost:11434/v1`       |
| `OLLAMA_API_KEY`                | Optional, defaults to `ollama` for local Ollama              |
| `EVERRUNS_CLI_MODEL`            | Override the auto-selected default model                     |
| `EVERRUNS_CLI_REASONING_EFFORT` | OpenAI-only reasoning effort override                        |

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

## How it's wired

- `src/runtime.rs` — registers a platform `SessionFileSystemFactory` that
  routes normal paths through a `RealDiskFileStore` rooted at the workspace
  and routes `/outputs/` through the current session folder, then wraps it
  with `WriteBlocklistFileStore` and `ApprovalGatingFileStore`. Registers all
  the built-in capabilities listed above plus a tiny custom
  `CodingBashCapability` for the shell tool. Picks a driver
  (Anthropic / OpenAI / llmsim).
- `src/tools.rs` — `BashTool` only. Built-in `virtual_bash` runs against the
  VFS, not the real workspace, so the shell tool stays custom for yolop.
- `src/approval.rs` — `ApprovalGate` and the request enum. Implements
  `everruns_runtime::FileApprovalGate` so it can be plugged into the approval
  decorator. The gate is shared between the bash tool and the session
  filesystem approval decorator.
- `src/app.rs` + `src/main.rs` — ratatui TUI and the one-shot `--print`
  driver.
- `src/session_log.rs` — JSONL event log, locking, and platform-aware
  session directory resolution.

## Development

### Building

```bash
cargo build
cargo build --release
```

### Tests

Unit tests (offline):

```bash
cargo test
```

Live integration test against OpenAI through Doppler (needs `OPENAI_API_KEY`
in your Doppler config):

```bash
doppler run -- cargo test --test integration -- --ignored
```

### Lint

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

### Smoke test

Offline:

```bash
cargo run -- --provider llmsim -p "hi"
```

Live (OpenAI):

```bash
doppler run -- cargo run -- --provider openai -p "list the files in this repo"
```

## Project structure

```
.
├── .agents/skills/
│   ├── ship/SKILL.md        # /ship workflow
│   └── maintenance/SKILL.md # /maintenance workflow
├── .github/workflows/
│   └── ci.yml               # lint + unit tests + offline smoke + live smoke
├── specs/
│   ├── shipping.md
│   └── maintenance.md
├── src/
│   ├── main.rs
│   ├── app.rs               # TUI
│   ├── approval.rs          # approval gate
│   ├── capabilities.rs      # yolop-owned capabilities
│   ├── diff.rs
│   ├── runtime.rs           # everruns-runtime wiring
│   ├── session_log.rs       # JSONL session log
│   └── tools.rs             # bash tool
├── tests/
│   └── integration.rs       # offline + live (`--ignored`) integration tests
├── AGENTS.md                # coding-agent guidance (read live every turn)
├── Cargo.toml
└── README.md
```

## Caveats

- Single-turn rendering: assistant messages appear after the turn completes
  rather than streaming token-by-token. The runtime emits delta events;
  wiring them to the UI is a follow-up.
- Persistence is event-log only. Messages are reconstructed from events on
  resume. There is no separate snapshot of agent state.
- Bash has a 120 s timeout and a 1 MiB-per-stream capture cap. Long-running
  jobs are not yet supported as background tools.
- The bash approval prompt shows the command string only — sub-commands
  spawned by it are not pre-listed.
- The write blocklist matches directory names case-sensitively at any depth;
  it is intentionally conservative, not exhaustive.

## License

MIT — see [`LICENSE`](./LICENSE).

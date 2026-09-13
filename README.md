# yolop

[![Release](https://img.shields.io/github/v/release/everruns/yolop)](https://github.com/everruns/yolop/releases)
[![CI](https://github.com/everruns/yolop/actions/workflows/ci.yml/badge.svg)](https://github.com/everruns/yolop/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/yolop.svg)](https://crates.io/crates/yolop)
[![Repo: Agent Friendly](https://img.shields.io/badge/agent-friendly-9cf.svg)](AGENTS.md)

Yolop is a terminal coding agent that plans the fix, runs the commands, and shows every step. Point it at your repo, describe the outcome, watch it work.

```bash
yolop
# > add retry with backoff to the sync client, then run the tests for it
```

![yolop upgrading a Rust CLI, adding JSON output, and running its tests](docs/demo.gif)

## Contents

- [Why yolop](#why-yolop)
- [Install](#install)
- [Quickstart](#quickstart)
- [How you work with it](#how-you-work-with-it)
- [Safety by default](#safety-by-default)
- [Providers and models](#providers-and-models)
- [Context superpowers](#context-superpowers)
- [Editor integration](#editor-integration)
- [MCP servers](#mcp-servers)
- [Extensions](#extensions)
- [Skills, memory, and hooks](#skills-memory-and-hooks)
- [Reference](#reference)
- [Contributing, evals, releases, license](#contributing-evals-releases-license)

## Why yolop

- **Autonomous, not chatty.** It reads code, makes the edit, runs tests, and iterates. You review the diff, not a wall of suggestions.
- **Safe to leave running.** OS sandboxing plus a soft approval model: routine file work proceeds, risky or outward facing actions pause for a yes.
- **Keeps your place.** Every run is a resumable session with checkpoints, rewind, undo, and fork. Long tasks survive interruptions.
- **Works where you work.** Full screen TUI, inline mode for small terminals, one shot print mode for pipes and CI, and ACP support for Zed and other editors.
- **Speaks your stack.** Nine provider backends, local inference, custom OpenAI compatible endpoints, MCP servers, extensions, skills, and hooks.

## Install

Prerequisites: a Rust toolchain for source builds, `git`, and optionally the `gh` CLI for GitHub workflows.

```bash
# macOS
brew install everruns/tap/yolop

# Any platform with Cargo
cargo install yolop

# From source
git clone https://github.com/everruns/yolop.git
cd yolop
cargo build --release
./target/release/yolop --help
```

Prefer a prebuilt binary, including accelerated local inference builds (`metal` on macOS, `cuda` on Linux)? Grab one from [releases](https://github.com/everruns/yolop/releases).

## Quickstart

```bash
yolop
# /setup    connect a provider (guided, stores key in the OS keychain or env)
/model      # pick a model any time
```

Then give it a real task:

```bash
yolop
# > explain how checkout retries work in this repo
# > add a failing test for expired-token retry, implement it, run cargo test
```

Three more ways to run it:

```bash
# One shot, print the answer and exit
yolop -p "summarize what this repo does"

# Non-interactive, deterministic output for pipes and scripts
yolop --print --no-stream -p "list every TODO in src/" | head -n 20

# Keep working in a named session, resume it later
yolop --session checkout-fix -p "reproduce the flaky checkout test"
yolop --resume checkout-fix
```

Expected result: yolop explores the repo, proposes a plan for anything risky, edits files, runs the relevant tests, and leaves a diff plus a transcript you can rewind.

## How you work with it

### Interactive TUI

The default is a full screen terminal UI. Type a task, watch it read, edit, and run commands.

| Key | Action |
| --- | ------ |
| `Enter` | Send message |
| `Ctrl+C` | Interrupt the running turn |
| `Esc` | Close popup, then cancel turn |
| `↑` / `↓` | History |
| `Ctrl+P` | Toggle plain text input (paste safe) |
| `Ctrl+T` | Toggle thinking display |
| `F12` | Save the screen to a PTF snapshot |

Prefer a compact layout? `yolop --inline` docks the conversation above a single prompt line, handy in short terminals and tmux panes. `yolop --no-stream` renders final text only.

### Print mode for scripts and CI

`--print` (alias `-p`) answers once and exits. It never resumes a session and ignores `--resume`, `--continue`, and `--fork`.

```bash
yolop -p "what does the sync client retry on?" --provider openai
echo "summarize this diff" | yolop -p "$(cat)"
yolop --print --no-stream --quiet -p "list public functions in src/sync.rs"
```

Piping a prompt through stdin works: the piped text becomes the prompt when no positional prompt is given.

### Sessions, checkpoints, and undo

Every run records a session: full transcript plus file and command checkpoints.

```bash
yolop --session stripe-work        # create or resume by name
yolop --continue                   # resume the most recent session
yolop --resume 2026-09-01_10-00-00 # resume by id prefix
yolop --fork 2026-09-01_10-00-00   # branch a copy, keep the original intact
yolop --session-log stripe-work    # read the transcript later
```

Inside the TUI:

- `/rewind` restores files and history to an earlier checkpoint. Your current work is kept on a forked branch, nothing is lost silently.
- `/undo` rolls back the last file change only.
- `yolop --record run.json` saves a session transcript for replay or bug reports.

Two agents can coordinate through one transcript: see [docs/session-coordination.md](docs/session-coordination.md).

### Isolated work with git worktrees

Start a task in a fresh worktree without touching your checkout:

```bash
yolop --worktree fix-login --session fix-login -p "reproduce the login bug and fix it"
```

The session stays attached to the worktree, so `--resume` and `--continue` return to the same isolated tree.

## Safety by default

Yolop is autonomous, so the guardrails are on from the start: sandboxing plus approvals, both configurable.

| Control | What it does |
| ------- | ------------ |
| Sandboxing | Restricts file and network access per command. Default `workspace-write` lets the agent edit inside the repo while keeping the rest of the system read only. |
| Approvals | Decides when to pause. Routine reads and edits proceed; destructive, irreversible, or outward facing actions ask first. |
| Soft approval | Remembers a yes for the session when the command shape stays the same, so you are not asked twice for the same safe pattern. |
| Hard stop | `--ask` forces approval for every tool call. `--yolo` removes prompts; only use it in a disposable sandbox. |

```bash
yolop --sandbox workspace-write --approval default   # the defaults
yolop --sandbox full --approval strict               # locked down
yolop --ask -p "migrate the database"                # approve every step
```

Yolop never force pushes, rewrites history, or skips hooks on your behalf. Details: [docs/features/sandboxing/sandboxing.md](docs/features/sandboxing/sandboxing.md) and [docs/features/approvals.md](docs/features/approvals.md).

## Providers and models

Nine backends, one switch. Set a key once with `/setup`, override per run with flags or env vars.

| Provider | Default model | Key |
| -------- | ------------- | --- |
| `openai` | `gpt-5` | `OPENAI_API_KEY` |
| `anthropic` | `claude-opus-4-6` | `ANTHROPIC_API_KEY` |
| `google` | `gemini-2.5-pro` | `GEMINI_API_KEY` |
| `codex` | `gpt-5.2-codex` | Codex OAuth or `OPENAI_API_KEY` |
| `meta` | `llama-4-maverick` | `META_API_KEY` |
| `openrouter` | `anthropic/claude-sonnet-4` | `OPENROUTER_API_KEY` |
| `ollama` | `qwen3:8b` | none for local defaults |
| `local` | `ministral-3-14b` | none, runs on your machine |
| `custom` | set via `/setup` | custom endpoint key |

```bash
yolop --provider anthropic --model claude-opus-4-6 -p "fix the failing test"
yolop --provider ollama --model qwen3:8b -p "explain this file"
yolop --provider local -p "refactor the parser without network access"
```

Resolution order, highest wins: `--model` flag, then `YOLOP_MODEL`, then the profile default, then the provider default above. `--list-models` shows what each backend offers.

<details>
<summary>Codex, OpenRouter, custom endpoints, and local inference</summary>

**Codex.** `--provider codex` uses your ChatGPT plan via OAuth (`/setup` walks through it, refresh is automatic). Any `OPENAI_API_KEY` in the environment is ignored while the OAuth token is valid, so a stale key cannot shadow your login. Codex supports reasoning effort (`/effort`, `--thinking high`) and request timeout (`YOLOP_CODEX_TIMEOUT_SECS`, default 600); reasoning text stays visible when `YOLOP_CODEX_REASONING_SUMMARY` is unset.

**OpenRouter.** `--provider openrouter` defaults to `anthropic/claude-sonnet-4`, sets sensible provider routing, and advertises noonotion support continuity. For ZDR-only traffic set `OPENROUTER_ZDR_ONLY=1`, which restricts routing to zero data retention providers.

**Custom.** Point any OpenAI compatible endpoint at yolop. Configure it once in `/setup` or per run:

```bash
export CUSTOM_API_URL="https://llm.example.com/v1"
export CUSTOM_API_KEY="sk-..."
export CUSTOM_MODEL="org/model"
yolop --provider custom -p "hi"
```

`CUSTOM_API_URL` also accepts a full chat completions URL, and `CUSTOM_MODEL_PREFIX` controls displayed names.

**Local.** `--provider local` runs inference on your machine through the `local-inference` feature. Mac users want the `metal` release binary, Linux users the `cuda` one.

</details>

## Context superpowers

Yolop earns its keep on large repos:

- **Repo map.** A compact structural index of the codebase, refreshed as files change, so the agent finds the right module before reading. Guide: [docs/features/repo-map/repo-map.md](docs/features/repo-map/repo-map.md).
- **LSP aware edits.** Rename a symbol and references follow across files instead of leaving stale call sites.
- **Dependency aware tasks.** It builds a task graph, runs independent work in parallel, and waits on what matters.
- **Subagents and background work.** Delegate a research branch or park a long test run in the background while you keep chatting. See [docs/features/subagents/subagents.md](docs/features/subagents/subagents.md).
- **Show me.** `/show <topic>` pulls a focused guide into context only when needed, keeping routine turns lean: [docs/features/show-me/show-me.md](docs/features/show-me/show-me.md).
- **OKF knowledge.** Durable project memory lives in an explicit bundle, validated by `python3 scripts/validate_okf.py knowledge --check-links`: [docs/features/okf/okf.md](docs/features/okf/okf.md).

## Editor integration

Yolop speaks the Agent Client Protocol, so Zed and other ACP editors can drive it directly.

```bash
yolop --acp  # speak ACP over stdio
```

In Zed, point a custom agent server at your yolop binary. Full steps live in the Zed docs for external agents.

## MCP servers

Add tools by listing MCP servers in `.mcp.json` at the repo root. Yolop connects over stdio or HTTP and advertises their tools to the agent.

```jsonc
// .mcp.json
{
  "mcpServers": {
    "searxng": {
      "type": "http",
      "url": "http://localhost:8888/mcp"
    },
    "context7": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@upstash/context7-mcp"]
    }
  }
}
```

```bash
yolop --mcp-config ./my-mcp.json -p "search the docs for retry semantics"
```

OAuth, Docker, and bearer auth variants are supported in the same file.

## Extensions

Extensions add custom tools over the YEP protocol. Scaffold one from inside yolop:

```bash
# In the TUI:
/extension create
```

That generates a working server (Python, Node, or Rust), registers it in `.yolop/extensions.yaml`, and hot loads it. You get per call progress, cancellation, and structured schema validation.

```yaml
# .yolop/extensions.yaml
servers:
 hello:
    command: ["python3", ".yolop/ext/hello_server.py"]
```

Full protocol and SDK guide: [docs/extensions.md](docs/extensions.md).

## Skills, memory, and hooks

- **Skills** are reusable workflows you invoke with `/skill-name`. Ship them in `.agents/skills/` and they load on demand.
- **Memory** persists what the agent should remember about you and the project: global and repo scoped notes plus a task checkpoint file per session.
- **Hooks** run your commands on agent lifecycle events (before a tool runs, after it finishes, on session stop). Configure them in `.yolop/hooks.yaml`, inspect a run with `yolop --debug-hooks`: [docs/features/hooks.md](docs/features/hooks.md).

## Reference

### CLI essentials

Run `yolop --help` for the full list. These are the flags you will reach for daily.

| Flag | What it does |
| ---- | ------------ |
| `-p, --prompt <text>` | One shot print mode. Aliases: `--query`, `--print`. Reads stdin when no prompt is given. |
| `--provider <name>` | Backend: `openai`, `anthropic`, `google`, `codex`, `meta`, `openrouter`, `ollama`, `local`, `custom`. |
| `--model <id>` | Model within the backend. |
| `--thinking <level>` | Reasoning effort: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`. |
| `--session <name>` | Create or resume a named session. |
| `--continue` | Resume the most recent session. |
| `--resume <id>` | Resume a session by id prefix. `--fork <id>` branches a copy. |
| `--session-log <name>` | Print a past transcript without resuming. |
| `--record <file>` | Save the transcript. `--replay <file>` reprint it. |
| `--sandbox <mode>` | `off`, `workspace-write` (default), `full`. Aliases: `--enable-yolo`, `--safe`, `--unsafe`. |
| `--approval <mode>` | `default`, `strict`. `--ask` approves every call, `--yolo` asks none. |
| `--write-mode <mode>` | `normal` or `diff-only` (propose patches without writing). |
| `-C, --working-dir <dir>` | Run as if started in another directory. |
| `--worktree <name>` | Run inside a fresh git worktree. |
| `--background` | Keep running after you detach; completion notifies you. |
| `--acp` | Speak Agent Client Protocol over stdio. |
| `--mcp-config <file>` | Load MCP servers from a custom file. |
| `--profile <name>` | Use a saved provider, model, and approval preset. |
| `--exec <cmd>` | Run one non-interactive command and exit (`--resume` keeps its session). |
| `--list-models` | List models for the selected backend. |
| `--extensions <spec>` | Load extra extension servers. `--no-session` skips persistence. |
| `-q, --quiet` | Plain output, no progress UI. `--no-stream` renders final text only. |
| `--inline` | Compact split footer layout instead of full screen. |

Environment overrides: `YOLOP_MODEL`, `YOLOP_PROFILE`, `YOLOP_SANDBOX`, `YOLOP_APPROVAL`, `YOLOP_NO_SESSION`, `YOLOP_MCP_CONFIG`, plus provider keys such as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `META_API_KEY`, `OPENROUTER_API_KEY`, `CUSTOM_API_URL`, `CUSTOM_API_KEY`, `CUSTOM_MODEL`. `RUST_LOG` controls log verbosity.

### Slash commands and keys

| Command | Purpose |
| ------- | ------- |
| `/setup` | Guided provider and key setup |
| `/model`, `/effort` | Switch model or reasoning effort |
| `/status` | Show session, provider, sandbox state |
| `/rewind`, `/undo` | Restore a checkpoint or revert the last edit |
| `/compact` | Summarize history to free context |
| `/show <topic>` | Load a focused guide on demand |
| `/checkpoint`, `/coordinator` | Manage multi agent handoffs |
| `/extension`, `/skills` | Create extensions, run skills |
| `/review`, `/commit` | Review the diff, commit it |
| `/background` | Manage background tasks |
| `/help`, `/quit` | List commands, exit |

### Recipes

```bash
# Explain unfamiliar code before touching it
yolop -p "how does auth refresh work? cite the files"

# Fix a bug end to end in an isolated tree
yolop --worktree fix-422 -p "reproduce issue #422 with a failing test, then fix it"

# Review before you commit
yolop
# > /review, then /commit

# Replay a run for a bug report
yolop --record /tmp/run.json -p "reproduce the flake"
yolop --replay /tmp/run.json
```

## Contributing, evals, releases, license

- Contributing guide: [CONTRIBUTING.md](CONTRIBUTING.md). Coding agent notes: [AGENTS.md](AGENTS.md). Security policy: [SECURITY.md](SECURITY.md). Code of conduct: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- Eval studies (SWE-bench Verified, harness A/Bs, LSP isolation): [evals/README.md](evals/README.md).
- Changelog: [CHANGELOG.md](CHANGELOG.md). Release process: ask for `/release`.
- License: MIT, see [LICENSE](LICENSE).

Fun fact: the name is a wink at the old joke about writing code on a dare. Yolop keeps the speed and removes the recklessness: sandbox first, ask when it matters, rewind when it does not.

*Produced by [yolop](https://everruns.com/yolop)*

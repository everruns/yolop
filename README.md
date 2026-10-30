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

![yolop upgrading a pizza-rocket CLI, adding cheesy JSON output, and running its tests](docs/demo.gif)

## Origin of the name

`yolop` comes from the Ukrainian `Йолоп`: a dummy, fool, or not-too-bright person. The name was meant to sound clever and funny in Ukrainian while also describing the agent's starting point: Yolop avoids per-tool approval pop-ups. Routine workspace work runs automatically; hard prompts are reserved for the configured shell boundary, with [soft approval](#trust-and-guardrails) for critical moments that need spoken consent and an audit trail.

## Contents

- [Origin of the name](#origin-of-the-name)
- [Why yolop](#why-yolop)
- [Install](#install)
- [Quickstart](#quickstart)
- [How you work with it](#how-you-work-with-it)
- [Trust and guardrails](#trust-and-guardrails)
- [Providers and models](#providers-and-models)
- [Context superpowers](#context-superpowers)
- [MCP servers](#mcp-servers)
- [Extensions](#extensions)
- [Skills, memory, and hooks](#skills-memory-and-hooks)
- [Reference](#reference)
- [Contributing, evals, releases, license](#contributing-evals-releases-license)

## Why yolop

- **Friday afternoon tasks, done.** Point it at the flaky test, the dependency bump, the "add JSON output" issue. Come back to a green suite and a diff you can actually read.
- **It already knows the repo.** Structural map, symbol search, workspace-wide renames. "Move auth refresh into its own module" just happens, references included.
- **Talk it into shape.** Providers, MCP servers, editor wiring, how careful to be. Ask in plain words or run one guided command; config files only when you want them.
- **It works where you work.** Full screen TUI when you want the show, `yolop -p` for pipes and CI, Paseo or Zed over ACP when you live in the editor.
- **It keeps its place.** Every run is a resumable session with checkpoints. Rewind the bad idea, redo the good one, pick the goal back up Monday.

## Install

Prerequisites: a Rust toolchain for source builds, `git`, and optionally the `gh` CLI for GitHub workflows.

```bash
# macOS
brew install everruns/tap/yolop

# Any platform with Cargo
cargo install yolop --locked

# From source
git clone https://github.com/everruns/yolop.git
cd yolop
cargo build --release
./target/release/yolop --help
```

Prefer a prebuilt binary, including accelerated local inference builds (`metal` on macOS, `cuda` on Linux)? Grab one from [releases](https://github.com/everruns/yolop/releases).

On macOS, sandboxed attached administration requires the Yolop executable to
be outside the shell's writable directories. A source build inside the active
workspace therefore cannot run shell commands in `workspace-write` mode.
Install or build it in a protected location, or use `read-only` when appropriate.
See [sandbox setup requirements](docs/features/sandboxing/sandboxing.md#macos-executable-location).

## Quickstart

```bash
yolop
# /setup    connect a provider (guided)
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
yolop -p "list every TODO in src/" | head -n 20

# Keep working in a named session, reopen it later by reusing the name
yolop --session checkout-fix -p "reproduce the flaky checkout test"
yolop --session checkout-fix
```

Expected result: yolop explores the repo, proposes a plan for anything risky, edits files, runs the relevant tests, and leaves a diff plus a transcript you can rewind.

## How you work with it

### In your editor, over ACP

The main way most people use yolop: right inside the editor, over the Agent Client Protocol. [Paseo](https://paseo.sh/) is the most used client.

![yolop running inside Paseo over ACP, working through a dark mode task](docs/paseo-acp.png)

Wiring is one command:

```bash
yolop into paseo   # wire up Paseo
yolop into zed     # wire up Zed
yolop into buzz    # wire up Buzz Desktop
yolop --acp        # speak ACP over stdio, for any other ACP client
```

The same agent, hosted where your code already is.

### Interactive TUI

The flagship: a full screen chat with a scrolling transcript, syntax highlighted code, a multiline composer, and a live status bar showing model, worktree, and what the agent is doing.

Type a task and watch it read, edit, and run commands. Ask side questions with `/btw` without touching history, recover unfinished work through bounded automatic continuation, and track long work in the `Ctrl+B` activity rail.

| Key | Action |
| --- | ------ |
| `Ctrl+C` | Interrupt the running turn |
| `Ctrl+D` | Quit |
| `Ctrl+R` | Search history (`↑` / `↓` recall, persisted across sessions) |
| `Ctrl+B` | Activity rail for agents, background commands, and monitors |
| `Ctrl+V` | Paste image |
| `Ctrl+O` | Expand or collapse retained work details |

The composer takes `!<command>` for direct shell runs, `@` for file path completion, and slash commands: `/setup`, `/model`, `/effort`, `/shell`, `/background`, `/tools`, `/mcp`, `/cwd`, `/status`, `/clear`, plus `/rewind`, `/undo`, `/checkpoint`, `/coordinator`, and `/extensions`. Plans run through `write_todos`, with loop detection stopping repeated failing calls.

Prefer a compact layout? `yolop --inline` runs the TUI inline instead of the alternate screen, handy in short terminals and tmux panes.

### Print mode for scripts and CI

`-p` (`--print`) answers once and exits. It is the mode for scripts and CI.

```bash
yolop -p "what does the sync client retry on?" --provider openai
echo "summarize this diff" | yolop -p "$(cat)"
yolop -p "list public functions in src/sync.rs"
```

Piping a prompt through stdin works: the piped text becomes the prompt when no positional prompt is given.

### Sessions, checkpoints, and undo

Every run records a session in the data dir: full event transcript plus file checkpoints before every turn.

```bash
yolop --session stripe-work   # create it, or reopen it later by reusing the name
```

Inside the TUI:

- `/undo` previews the restore and asks for a confirm token, then rolls files and history back one checkpoint.
- `/rewind` lists checkpoints to restore; the undone work stays available for `/redo` with a fresh token.
- Send another message to resume work after a continuation limit.

A coordinator session can hand work to threads that run as their own Yolop processes, and any session can message another with `yolop sessions send`: see [docs/session-coordination.md](docs/session-coordination.md).

Sessions run on the Everruns Engine with a local SQLite authority. Reopening an older session imports its full history, branches and rewind state automatically. Back up `engine.sqlite` and its private `engine.key` together with the session directories. `events.jsonl` remains a readable export.

`yolop sessions migration-check <session-id>` checks a closed session's saved history, branches, and compaction records. It reports a source fingerprint and counts without printing conversation content or transferring ownership.

## Trust and guardrails

The paradigm here is relaxed security, stated plainly: yolop trusts the agent with your machine so work flows without pop-ups. Shell commands run with full host access unless you opt into containment. What stands between that trust and regret is approvals: anything destructive, irreversible, or outward facing pauses for a yes.

| Control | What it does |
| ------- | ------------ |
| Sandbox modes | `read-only`, `workspace-write`, or `danger-full-access` (the default). Sandboxed commands run under Seatbelt on macOS or Landlock plus seccomp on Linux. |
| Approval policies | `untrusted`, `on-failure`, `on-request` (the default), or `never`, set independently of the sandbox mode. |
| Soft approval | A spoken-consent layer on top, not a hard gate: `protective` pauses before any state change, `normal` only before destructive or outward facing steps, `off` runs fully autonomous. You approve in plain language. |

Set the level with `/setup approval <protective|normal|off>`, or just tell yolop to be more or less careful. Details: [docs/features/sandboxing/sandboxing.md](docs/features/sandboxing/sandboxing.md) and [docs/features/approvals.md](docs/features/approvals.md).

Transparent authorship: commits yolop creates keep your git identity with a `Co-Authored-By: yolop` trailer, and PRs made through `gh` get a `Produced by yolop` footer. Disable with `/setup attribution off`.

## Providers and models

Fourteen backends, one switch. Set a key once with `/setup`, override per run with flags.

| Provider | Default model | Key |
| -------- | ------------- | --- |
| `openai` | `gpt-6.1-sol` | `OPENAI_API_KEY` |
| `codex` | `gpt-6.1-sol` | ChatGPT login (browser or device flow), or `CODEX_API_KEY` / `CODEX_ACCESS_TOKEN` |
| `anthropic` | `claude-opus-5-5` | `ANTHROPIC_API_KEY` |
| `meta` | `muse-spark-1.3` | `MODEL_API_KEY` |
| `openrouter` | `openai/gpt-6.1-sol` | browser login, or `OPENROUTER_API_KEY` |
| `google` | `gemini-3.8-flash` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` |
| `mistral` | `mistral-large-4` | `MISTRAL_API_KEY` |
| `fireworks` | `accounts/fireworks/models/llama-v3p1-70b` | `FIREWORKS_API_KEY` |
| `vercel` | `anthropic/claude-opus-5-5` | `AI_GATEWAY_API_KEY` |
| `cloudflare` | `@cf/meta/llama-3.3-70b-instruct-fp8-fast` | `CLOUDFLARE_API_TOKEN` plus `CLOUDFLARE_ACCOUNT_ID` |
| `mai` | `mai-1-preview` | `AZURE_AI_API_KEY` plus `AZURE_AI_ENDPOINT` |
| `ollama` | `llama3.2` | none; `OLLAMA_BASE_URL` / `OLLAMA_API_KEY` for remote servers |
| `local` | Qwen3-30B-A3B, Q4 (`~19 GB`) | none; weights via `yolop weights pull` |
| `custom` | provider default | `CUSTOM_BASE_URL` plus optional `CUSTOM_API_KEY` |

```bash
yolop --provider anthropic -p "fix the failing test"
yolop --provider ollama -p "explain this file"
yolop --provider local -p "refactor the parser without network access"
```

Pick per run with `--provider` and `-m`, persist choices with `/setup` or `yolop config model set`, and switch mid-session with `/model` and `/effort`. GPT-6.1 Sol is on the menu (`-m gpt-6.1-sol`); for OpenAI's faster or cheaper service tiers set `yolop config set speed fast` (or `flex`, `ultrafast`, `default`), or `YOLOP_SPEED` for one run. A tier the model does not offer is skipped.

<details>
<summary>Codex, OpenRouter, custom endpoints, and local inference</summary>

Search a provider's catalog and test a model before adding it to your menu:

```bash
yolop config models search nemotron --provider openrouter
yolop config models test openrouter/nvidia/nemotron-3-ultra-550b-a55b:free
yolop config models add openrouter nvidia/nemotron-3-ultra-550b-a55b:free
```

Search prints provider and model arguments for `config models add`, a name, and
context length when advertised. Broad results are bounded and report the total
so you can narrow the query. The test sends one small prompt without tools,
reports `ok` and latency, and exits nonzero on provider failure. It may incur a
provider charge. Neither command edits the model menu. `-m` accepts a menu label,
a unique listed model id, or `provider/model`. A qualified reference selects that
provider, including gateway ids such as `openrouter/openai/gpt-6.1-sol`.
Invalid explicit selections fail without substituting another model.

**Codex.** `--provider codex` uses your ChatGPT plan via a browser or device login (`/setup` walks through it). Pin a model with `yolop config model set codex <id>`, tune reasoning with `/effort`. The Codex route signs in with the Codex CLI's public OAuth client; to use a different client there, set `yolop config set chatgpt_client_id <id>` or `YOLOP_CHATGPT_CLIENT_ID` and log in again. An existing login keeps refreshing with the client that issued it. A browser sign-in uses OpenAI's open-source Sign in with ChatGPT by default, which needs no borrowed client: yolop registers a client for your account on the first sign-in and runs turns on the public Responses API, still billed to your ChatGPT plan. It is a preview route, so a few request options (output token cap, temperature, `speed`) do not apply there. To use the Codex route instead, run `yolop config set chatgpt_sign_in codex` (or set `YOLOP_CHATGPT_SIGN_IN=codex`) and sign in again. A device login uses the Codex route unless `open-source` is set explicitly.

**OpenRouter.** Sign in from the browser or set `OPENROUTER_API_KEY`. The default model is `openai/gpt-6.1-sol`; any `provider/model` id works with `-m`.

**Cloudflare Workers AI.** Needs the `CLOUDFLARE_API_TOKEN` plus the account ID it is scoped to. Setup collects the account ID alongside the key, or export `CLOUDFLARE_ACCOUNT_ID`.

**Microsoft MAI.** Needs the `AZURE_AI_API_KEY` plus the Azure AI Foundry endpoint of the deployment. Setup collects the endpoint alongside the key, or set `AZURE_AI_ENDPOINT` (or `base_urls.mai` in settings).

**Custom.** Point any OpenAI compatible endpoint at yolop via `CUSTOM_BASE_URL` (plus `CUSTOM_API_KEY` when needed), or store it in a profile. The model comes from your profile pick, falling back to the provider default.

**Local.** `--provider local` runs inference on your machine: no key, no network. `yolop weights pull` fetches the default Qwen3-30B-A3B (Q4, about 19 GB); `yolop weights pull qwen3-8b` grabs the 5 GB alternative. There is no CPU-only download. Prefer the accelerated release binaries: `metal` on macOS, `cuda` on Linux.

</details>

## Context superpowers

Yolop earns its keep on large repos (full inventory: [Tools and capabilities](docs/features/tools.md)):

- **Repo map.** A compact structural index of the codebase, refreshed as files change, so the agent finds the right module before reading. Guide: [docs/features/repo-map/repo-map.md](docs/features/repo-map/repo-map.md).
- **LSP aware edits.** Rename a symbol and references follow across files instead of leaving stale call sites.
- **Planned work.** `write_todos` keeps multi-step tasks on track, independent steps run in parallel, and loop detection stops repeated failing calls.
- **Subagents and background work.** Delegate a research branch or park a long test run in the background while you keep chatting. See [docs/features/subagents/subagents.md](docs/features/subagents/subagents.md).
- **Show me.** Focused guides load into context only when needed, keeping routine turns lean: [docs/features/show-me/show-me.md](docs/features/show-me/show-me.md).
- **Conversation control plane.** Broad configuration and administration go through `yolop <route>` commands instead of model tool schemas, so shell descendants connect back to the live session: [Conversation Control Plane](docs/features/conversation-control-plane/conversation-control-plane.md).
- **OKF knowledge.** Durable project memory lives in an explicit bundle, validated by `python3 scripts/validate_okf.py knowledge --check-links`: [docs/features/okf/okf.md](docs/features/okf/okf.md).

## MCP servers

The fastest way is to ask: "install the GitHub MCP server" and yolop wires it up and shows the new tools. `/mcp` manages live connections in-session; `yolop mcp` does it from the terminal (`list`, `show`, `test`, `login`, `add`, `remove`, `enable`, `disable`, each per scope). Configuration refreshes after tool execution, so servers installed mid-session do not require a restart. For OAuth servers, `yolop mcp login <name>` opens browser sign-in; `yolop mcp test <name>` verifies the connection and lists its tools.

The manual path for remote HTTP servers is `.mcp.json` at the repo root. Local
stdio servers must be added to user-owned global configuration with
`yolop mcp add <name> --scope global --type stdio ...`, because
repository-controlled commands are not executed:

```jsonc
// .mcp.json
{
  "mcpServers": {
    "searxng": {
      "type": "http",
      "url": "http://localhost:8888/mcp"
    }
  }
}
```



## Extensions

Extensions add custom tools over the YEP protocol. Install from crates.io, git, or a local path, enable them through settings capabilities or live with `/extensions`, and write your own against the `yolop-yep` SDK. The host gives per call progress, cancellation, and structured schema validation. Extensions run with your user privileges, so install only what you trust.

Full protocol and SDK guide: [docs/extensions.md](docs/extensions.md).

## Skills, memory, and hooks

- **Skills** are reusable workflows you invoke by name. Ship them in `.agents/skills/` (repo) or `~/.agents/skills` (global, shared across agents) and they load on demand. Manage packages with `yolop skills`; newly installed skills work immediately.
- **Memory** is a structured `MEMORY.md` of durable, cross-session memories, managed in natural language ("remember that I prefer terse answers") with `remember`, `recall`, and `forget`. Only titles load each turn; bodies recall on demand, and mutations require explicit approval.
- **Hooks** run your commands on agent lifecycle events (before a tool runs, after it finishes, on session stop). They live in `hooks.json` under the config dir: [docs/features/hooks.md](docs/features/hooks.md).

Source builds containing the shared inspector can opt into local memory reports
in `settings.toml`:

```toml
[[capabilities]]
ref = "memory"
report_only_inspection = true
```

Successful writes and selected recalls include reports tied to the note's
content, version and observed origin. Automatic disclosure checks only titles
already shown. Inspection never deletes notes, contacts external scanners or
replaces mutation approval. It is heuristic, not guaranteed injection protection.
This source change requires an Everruns dependency revision containing the
shared inspector; it is not available with the current committed lockfile.

## Reference

### CLI essentials

Run `yolop --help` for the full list. These are the flags you will reach for daily.

| Flag | What it does |
| ---- | ------------ |
| `-p, --print <text>` | One shot: answer once and exit. Reads stdin when no prompt is given. |
| `--provider <name>` | Backend for this run. |
| `-m, --model <id>` | Model for this run (or `EVERRUNS_CLI_MODEL`). |
| `--reasoning-effort <level>` | Reasoning effort for this run. |
| `--profile <name>` | Named settings overlay: provider, model, MCP, and more. |
| `--session <name>` | Create a session, or reopen it later by reusing the name. |
| `-C, --cwd <dir>` | Run as if started in another directory. |
| `--inline` | Single-screen inline TUI instead of the alternate screen. |
| `--no-compact-work` | Retain ordinary transcript rows instead of compact work summaries. |
| `--acp` | Speak Agent Client Protocol over stdio. |
| `--sandbox` | Attach the sandbox dashboard sidecar when one is provisioned for the run. |
| `--image <path>` | Attach an image (multimodal models). Repeatable. |
| `--trajectory-out <file>` | Write the full transcript JSONL, for bug reports and replays. |
| `--config-dir`, `--data-dir` | Override the settings home / session-plus-model home (or `YOLOP_CONFIG_DIR`, `YOLOP_DATA_DIR`). |
| Subcommands | `yolop into (paseo|zed|buzz)`, `yolop mcp ...`, `yolop weights ...`, `yolop version`. Full list: `yolop --help`. |

Environment: `EVERRUNS_CLI_MODEL` overrides the model; provider keys are `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `MODEL_API_KEY`, `OPENROUTER_API_KEY`, `GEMINI_API_KEY` or `GOOGLE_API_KEY`, and `CUSTOM_API_KEY`, with `CUSTOM_BASE_URL` and `OLLAMA_BASE_URL` pointing at endpoints. `RUST_LOG` controls log verbosity.

### Slash commands and keys

| Command | Purpose |
| ------- | ------- |
| `/setup` | Guided provider, key, approval, and attribution setup |
| `/model`, `/effort` | Switch model or reasoning effort |
| `/btw` | Side question without touching history |
| `/shell` | Run a shell command, with tab completion |
| `/background` | Manage background tasks |
| `/tools`, `/mcp` | List tools, manage MCP connections |
| `/status`, `/cwd` | Show session state, show or change directory |
| `/rewind`, `/undo` | Restore a checkpoint, revert the last file change |
| `/checkpoint`, `/coordinator` | Milestones and multi agent handoffs |
| `/extensions` | List and enable extensions |
| `/clear`, `/help`, `/quit` | Clear the transcript, list commands, exit |

### Recipes

```bash
# Explain unfamiliar code before touching it
yolop -p "how does auth refresh work? cite the files"

# Fix a bug end to end, keep the session for follow-ups
yolop --session fix-422 -p "reproduce issue #422 with a failing test, then fix it"

# Reopen it later and rewind if an approach fails
yolop --session fix-422
# > /rewind

# Share a run for a bug report
yolop --trajectory-out /tmp/run.jsonl -p "reproduce the flake"
```

## Contributing, evals, releases, license

- Contributing guide: [CONTRIBUTING.md](CONTRIBUTING.md). Coding agent notes: [AGENTS.md](AGENTS.md). Security policy: [SECURITY.md](SECURITY.md). Code of conduct: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- Eval studies (SWE-bench Verified, harness A/Bs, LSP isolation): [evals/README.md](evals/README.md).
- Changelog: [CHANGELOG.md](CHANGELOG.md). Release process: ask for `/release`.
- License: MIT, see [LICENSE](LICENSE).


*Produced by [yolop](https://everruns.com/yolop)*

### Provider driver packages

Yolop uses the aligned Everruns provider-driver packages for API providers,
ChatGPT plan sign-in, and the legacy Codex backend. Browser/device interaction
and settings storage stay in Yolop. Personal plan grants retain their issuing
client through refresh and reconnect; failed revocation keeps the saved grant
until disconnect can be retried.

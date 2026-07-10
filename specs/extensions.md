# Extensions — installable, capability-level packages

Status: **proposal** (design record, nothing implemented). Companion analysis
of prior art and alternatives is inline; the recommended design is
[Proposal A](#proposal-a-recommended--extension-packages-on-existing-seams).

## Why

Yolop's unit of functionality is the everruns **capability**: one `Capability`
implementation contributes tools, system-prompt text, hooks, MCP servers,
skills, commands, config schema, and facts through well-defined trait seams.
The harness is just an ordered list of capability refs + configs, already
user-editable via `[[capabilities]]` in `settings.toml`.

The contract is right; the *delivery* is wrong. Today every capability is
compiled into the binary. The recent `lsp` capability (#236) is the canonical
example: ~3.4k lines of Rust, a spec, and an eval study — all of which had to
land in this repo, ride the release train, and grow the binary of every user,
including the majority for whom it stays off by default. Anything
community-authored (a Kubernetes toolset, a Jira connector, a odd-language LSP
adapter, an alternative provider) has no path in at all.

An **extension** is an installable package that contributes at the capability
level — the same seams the trait exposes — without recompiling yolop.
Near-term contributions: tools + prompt + config (the LSP class). Later:
LLM providers, UI features, MCP server bundles, everruns plugins.

## What already exists (and must not be reinvented)

1. **The contract.** `everruns_core::capabilities::Capability` already
   enumerates every contribution kind an extension could want: `tools`,
   `system_prompt_contribution`, `config_schema`/`validate_config`,
   `mcp_servers`, `commands`, `mounts`, `dependencies`, `features`,
   pre/post tool hooks, tool-definition hooks, facts, message filters.
   The extension mechanism's job is to let that contract be *fulfilled by an
   installed artifact* instead of compiled code.
2. **Harness composition.** `[[capabilities]]` overrides (ordered
   remove/merge/append with JSON-schema validation, `set_config` tools,
   catalog listing) already control *which* capabilities are active and how
   they're configured. Extensions must plug into this, not add a parallel
   enable/disable system.
3. **Upstream plugins.** everruns-core ≥0.17 ships a plugin subsystem:
   a plugin directory (Claude Code / Codex / Cursor-compatible `plugin.json`
   dialect) compiles into a `DeclarativeCapabilityDefinition` — display
   metadata, system prompt, skills, user-invocable commands, bundled files,
   MCP servers, dependencies, risk level — registered as a `plugin:{name}`
   capability ref. Critically, the definition travels *inside the capability
   config*: the runtime hydrates `plugin:*` refs at collection time with no
   compile-time registration. Prompt/skills/MCP contributions therefore
   already flow through every existing path (prompt assembly, skill
   discovery, scoped MCP merge, dependency resolution).
4. **Scoped config surfaces.** Skills (workspace/global/system dirs), hooks
   (`hooks.json` global/workspace → upstream `user_hooks`), MCP (`.mcp.json`
   global/workspace), connectors (credential store) all follow one pattern:
   *workspace overrides global; broken optional files warn, never sink
   startup*. Extensions adopt the same pattern.

What is missing in yolop: an install/discovery/trust layer for packages, and a
way for an installed package to contribute **executable tool logic** — the
part that makes the LSP case work.

## Prior art

| Host | Unit & install | Code carrier | Contract breadth | Versioning |
|---|---|---|---|---|
| **pi** (Zechner) | TS files / npm / git packages, `pi install`, hot reload | in-process TypeScript via jiti, full trust | Maximal: ~35 loop events, tools, commands, **providers**, TUI renderers, flags | pre-1.0; pinning + compat shims |
| **Zed** | git repo → curated registry, prebuilt `extension.wasm` | in-process WASM (WIT contract), sandboxed; heavy work (LSP servers) spawned by host | Language servers, slash commands, MCP wrappers, DAP, themes | best-in-class: `zed:api-version` + per-version WIT snapshots |
| **goose** (Block) | config entry; any MCP server is an extension | stdio/HTTP MCP subprocess | Tools/resources/prompts only; no loop hooks, no prompt shaping | delegated to MCP negotiation |
| **Claude Code** | plugin dir via marketplaces (git), copied to cache | none in-process: markdown + JSON registering subprocesses (MCP, hooks, **`.lsp.json`**, bin/) | skills, commands, agents, hooks, MCP, LSP servers, monitors | `version` field or commit SHA; marketplace pinning |
| **OpenCode** | JS files / npm in config | in-process JS (Bun), full trust | tool intercept middleware, custom tools, events | npm semver of types package |
| **VS Code** | VSIX marketplace | separate Node "extension host" process | manifest contribution points + lazy activation events | `engines.vscode` + append-only API |

Takeaways that shape this design:

- **The in-process scripting model (pi, OpenCode) is a property of having a
  dynamic runtime.** It buys maximal contract breadth and a trivial authoring
  loop, at the price of zero isolation and single-language lock-in. A Rust
  binary gets this only by embedding an engine (Lua/JS/WASM) — a deliberate
  platform decision, not a default.
- **Everyone with a Rust/Go host converged on subprocess + protocol** for
  code-bearing extensions (goose, Crush, Claude Code, and Zed for anything
  heavy). It is the LSP pattern itself: language-neutral, crash-isolated,
  self-versioning via handshake.
- **Zed's split is the architectural insight**: sandboxed logic as *control
  plane*, host-spawned subprocesses as *data plane*. Its WIT snapshot
  versioning is the strongest compat story surveyed — and also the highest
  host-machinery cost.
- **Claude Code proves declarative-plus-subprocess covers most real demand**
  — including LSP servers via `.lsp.json` — with prompt-level contributions
  (skills/agents) as first-class citizens. Upstream everruns already bet on
  this dialect.
- **pi's anti-MCP stance is a warning about context cost, not transport**:
  many servers burn 5–10% of the context window on tool schemas before work
  starts. Yolop already has the antidote (`tool_search` deferral +
  never-defer allowlist); extensions must be able to *declare* their tool
  economics rather than dump schemas.

## Design space: how does logic arrive after compile time?

| Option | Verdict | Why |
|---|---|---|
| Rust `dylib` (`abi_stable`/`stabby`) | **Rejected** | No stable Rust ABI; per-platform artifacts; toolchain lockstep; no isolation. Unusable for third parties. |
| In-process scripting (Lua exists upstream; JS/`rquickjs`) | **Deferred** | Single language, engine surface to maintain, sandbox liability for FS/process/net access — which is exactly what LSP-class extensions need. Revisit for tiny in-loop logic (custom guardrail predicates) if demand shows. |
| WASM component (wasmtime + WIT, or Extism) | **Deferred** | The right *untrusted-marketplace* endgame, but heavy host machinery (WIT snapshots, bindgen per version), Rust-centric guest authoring, and WASI can't spawn/manage host processes — the LSP data plane still needs subprocesses. Adopt later as a control-plane tier if a public registry materializes. |
| Subprocess + protocol (the LSP/MCP pattern) | **Chosen** | Matches yolop precedent (LSP servers, stdio MCP, hook executors are all subprocesses). Any language, crash-isolated, protocol-versioned. Trust model identical to `.mcp.json` today. |
| Declarative-only bundle (Claude Code / everruns plugin) | **Chosen (base layer)** | Already implemented upstream; zero new runtime machinery; covers prompt/skills/commands/MCP/hooks. |

The two chosen rows compose: **an extension is a declarative bundle that may
carry subprocess servers as its code.** That is also exactly what upstream
`plugin.json` + `mcpServers` already expresses.

## Proposal A (recommended) — extension packages on existing seams

### The unit

An extension is a directory (installed from git, a local path, or later a
registry) in the upstream plugin dialect, with yolop-specific facets under a
single namespaced key so the package remains loadable by other hosts
(upstream parsing tolerates unknown fields):

```
my-ext/
  plugin.json            # upstream dialect + optional "yolop" facet block
  skills/<name>/SKILL.md # → declarative skills
  commands/*.md          # → user-invocable skills (slash commands)
  agents/*.md            # → system-prompt contribution
  .mcp.json              # → MCP servers (stdio/http), ${VAR} expansion
  hooks/hooks.json       # → user_hooks contributions (yolop facet)
```

Identity: capability ref `plugin:<name>` (upstream namespace, ≤43 bytes).
One extension ⇒ one capability ref, so enable/disable/configure rides the
existing `[[capabilities]]` machinery and `set_config` tools unchanged.

### Contribution mapping

| Facet | Carried by | Runtime seam (exists today) |
|---|---|---|
| System prompt | `agents/*.md` | `DeclarativeCapabilityDefinition.system_prompt` |
| Skills | `skills/` | declarative skills → scoped skills discovery |
| Slash commands | `commands/` | declarative user-invocable skills (`/name`) |
| Tools (code!) | `.mcp.json` stdio/http servers | scoped-MCP merge → runtime MCP client |
| Bundled files | `files` | capability mounts |
| Hooks | `hooks/hooks.json` (yolop facet) | `user_hooks` capability-contributed specs; mutable via `disabled_contributions` |
| Config schema | `yolop.config_schema` in manifest (yolop facet) | capability catalog validation + `set_config` |
| Tool policy | `yolop.tools` in manifest (yolop facet) | tool-search never-defer list, naming, approval hints |
| Dependencies | manifest `dependencies` | capability dependency resolution |

The first five rows are **pure reuse** of the upstream plugin compiler. The
yolop facets are the new work, and they are thin:

**Tool policy** (`yolop.tools`). MCP tools normally surface as
`mcp_<server>__<tool>` and may be deferred behind `tool_search`. The LSP eval
showed both matter enormously: stub schemas behind `tool_search` drove
adoption to ~zero, and prompt guidance names exact tools. So an extension can
declare, per tool: a **canonical name** (unprefixed alias; registered only if
it doesn't collide with a built-in or another extension — collision falls back
to the prefixed form with a startup warning), **`never_defer`** (schema always
loaded — budgeted, e.g. ≤8 tools per extension, so one package can't blow the
context), and approval/risk hints consumed by the approval capability.

**Config schema** (`yolop.config_schema`). JSON Schema for the extension's
`[[capabilities]]` config, exposed through the catalog like any built-in.
The config is delivered to the extension's servers as env
(`YOLOP_EXT_CONFIG` JSON) and/or `${config.*}` substitution in `.mcp.json` —
the same expansion mechanism `${VAR}` uses today. This gives an extension
exactly what `lsp` has: validated, discoverable, self-configurable settings.

**Hooks** (`hooks/hooks.json`). Same envelope as the existing hooks spec;
entries are namespaced `plugin:<name>:<hook_id>` and merged as
capability-contributed hooks, so users mute them with the existing
`disabled_contributions` list rather than editing the package.

### Discovery, install, trust

Scopes mirror skills/hooks/mcp exactly:

- **Workspace**: `<workspace>/.agents/extensions/<name>/` — ships with the
  repo.
- **Global**: `<config_dir>/yolop/extensions/<name>/` — per user.

Workspace overrides global by name. Malformed packages warn and are skipped.
Enablement: installing does **not** activate. An extension activates the same
way `lsp` does today — `[[capabilities]] ref = "plugin:<name>"` (written by
`/extensions enable`, `set_config`, or by hand). A manifest may declare
`yolop.default_enabled = true`, honored only for *global* installs (an act of
explicit user consent).

Install verbs (System commands + capability tools, so both the user and the
model can drive them):

```
/extensions install <git-url>[@rev] | <path>   # clone/copy into global scope
/extensions list | update [<name>] | remove <name>
/extensions enable|disable <name>
```

Git installs are pinned: `extensions.lock` (beside `settings.toml`) records
source URL, resolved commit, and a content hash; `update` is explicit.
Local-path installs are referenced, not copied (dev loop), and marked as such.

Trust model, in line with `.mcp.json` precedent but stricter for the new
attack path:

- **Global install = consent.** The user ran the install command; stdio
  servers and hook executors run with user privileges, same as `.mcp.json`
  today. `/extensions install` prints what the package will contribute
  (servers, hooks, prompt size, tool names) before confirming.
- **Workspace extensions arrive with someone else's repo.** They are
  discovered but **inert until approved once per content-hash**: first
  activation shows the contribution summary and records the approved hash in
  user-level state (not the repo). A changed package re-prompts. This is the
  VS Code workspace-trust lesson applied at package granularity.
- Hooks and prompt contributions are the sharpest edges (silent policy or
  instruction injection), which is why approval summarizes them explicitly.
- No sandbox is claimed. Sandboxing is what the WASM tier is *for*, later.

### Versioning and compatibility

- Manifest `version` (semver) + `engines.yolop = ">=0.6"`; incompatible
  packages load nothing and warn.
- The tool wire protocol is MCP — self-versioned by its own handshake, SDKs
  in every language, zero yolop-owned protocol surface.
- Yolop facets are versioned by the manifest schema; unknown facet keys warn
  and are ignored (tolerant, like upstream).
- No hot reload in v1: changes apply on next session (matches how
  `[[capabilities]]` config behaves today).

### Illustration: `lsp` as an extension

The existing capability decomposes cleanly:

- **Data plane** (already subprocesses): rust-analyzer, gopls, … unchanged.
- **Control plane** (today ~3.4k lines of in-tree Rust): becomes `yolop-lsp`,
  a standalone binary speaking MCP over stdio — the LSP client, server
  lifecycle manager, position-encoding conversion, workspace-edit safety
  checks move there verbatim. It exposes the seven `lsp_*` tools.
- **Everything else** becomes the package:

```json
{
  "name": "lsp",
  "description": "Semantic code intelligence from real language servers.",
  "version": "0.1.0",
  "engines": { "yolop": ">=0.6" },
  "mcpServers": { "lsp": { "type": "stdio", "command": "yolop-lsp",
                            "env": { "YOLOP_EXT_CONFIG": "${config}" } } },
  "yolop": {
    "config_schema": { "$ref": "./config.schema.json" },
    "tools": {
      "canonical_names": true,
      "never_defer": ["lsp_definition", "lsp_references", "lsp_hover",
                       "lsp_diagnostics", "lsp_rename", "lsp_symbols",
                       "lsp_code_actions"]
    }
  }
}
```

plus `agents/lsp.md` carrying the directive "call `lsp_*` FIRST for
symbol-level questions" prompt (verbatim from today's capability — the eval
showed the wording is load-bearing), and the same config schema
(`servers.<key>.command/args/extensions`, timeouts).

What this buys: a new language ecosystem integration (say, a JVM-heavy shop
wrapping jdtls with warmup quirks) is a package on a git URL, not a PR to
yolop. What it costs: MCP round-trip per tool call (noise against LSP server
latencies) and cross-process config delivery.

Migration policy: the built-in `lsp` capability **stays** until the extension
packaging reaches parity on `evals/lsp_integration` (pass rate, `lsp_*`
adoption, tokens). The eval is the gate; the extension is the dogfood that
proves the mechanism. If parity holds, the in-tree capability retires and the
binary shrinks — the mechanism pays for itself.

### Future facets (design now, build later)

- **LLM providers.** Two tiers. (1) *Declarative descriptors*: most
  OpenAI-compatible endpoints (Groq, Mistral, gateways) need only
  `base_url + auth env + model list + profiles` — a manifest facet feeding
  the existing `Custom`/Completions path. Prerequisite refactor: `Provider`
  moves from a closed enum to the catalog pattern the codebase already
  prefers. (2) *Provider proxies*: exotic protocols ship a subprocess
  exposing an OpenAI-compatible endpoint on a local socket (the pattern
  Ollama/llama.cpp normalized; `codex_driver`'s `DriverId::external` shows
  the driver-side seam). Native in-process drivers stay compiled-in.
- **UI features.** Extensions never run code in the TUI. Near-term surface:
  slash commands, config UI schema (`config_ui_schema` exists on the trait),
  and `user_ask`-style forms. If richer needs appear, upstream already has
  declarative UI precedent (`a2ui`/`openui`): extensions would emit
  component *trees*, yolop renders them with its own widgets. pi's
  TypeScript renderers are the counterexample that doesn't translate to a
  Rust host.
- **everruns plugin convergence.** The package format *is* the upstream
  plugin format plus a tolerated `yolop` facet. Anything installable in the
  hosted everruns product should load in yolop unchanged; yolop facets
  upstream one by one when the hosted product wants them (config schema and
  tool policy are obvious candidates). Keep in lockstep per the friendly-fork
  rule.
- **Registry.** Out of scope for v1 (git URLs suffice). If added: a curated
  index file in a git repo (Claude Code marketplace / Zed extensions-repo
  shape), commit-pinned entries.

## Proposal B (deferred) — a full capability wire protocol

Mirror the entire `Capability` trait over stdio JSON-RPC ("everything MCP
can't express"): dynamic prompt contributions, facts, message filters,
model-view providers, tool-definition hooks, narration. Rejected for now:

- The uncovered seams are the **hot loop**. Facts and message filters run per
  request and are documented as "cheap and side-effect free" — a cross-process
  round-trip in prompt assembly on every turn is hostile to latency and
  cache-friendliness. In-loop transformation is precisely where in-process
  execution (WASM tier, or upstream Rust) is the right tool.
- It creates a yolop-owned protocol with a versioning burden Zed needed WIT
  snapshots to manage — for demand that is, today, hypothetical.
- Static-but-per-session needs (prompt text chosen by config) are already
  covered by manifest + config substitution.

Revisit trigger: a concrete extension that cannot be expressed as
manifest + MCP (e.g. a community progress-guard variant needing message
filtering). Then prefer *narrow* extension methods negotiated on top of the
existing MCP connection (`experimental`/`yolop/*` namespaced) over a second
protocol.

## Proposal C (deferred) — WASM control-plane tier

wasmtime + WIT (or Extism) hosting sandboxed extension logic in-process, with
host functions for spawn-subprocess/read-workspace/HTTP mirroring Zed's
capability declarations. This is the only path to *untrusted* third-party
code and to in-loop hooks without RPC latency. Costs: WIT snapshot
versioning machinery, guest-language friction, and a second execution model
to support forever. Adopt only alongside a public registry where untrusted
authorship is the norm, and keep Proposal A's manifest as the outer package
format (a WASM module becomes one more facet, not a new unit).

## Rollout

1. **Loader + declarative facets.** Discover both scopes, compile via the
   upstream plugin compiler, inject `plugin:<name>` configs into the harness,
   workspace trust gate, `/extensions` list/enable/disable. Exit: a
   prompt+skills+MCP package (e.g. a docs-search extension) works end to end.
2. **Yolop facets.** `config_schema` into the catalog + config delivery to
   servers; tool policy (canonical names, `never_defer` budget, approval
   hints); hooks contributions; `extensions.lock` + install/update/remove
   from git. Exit: facets exercised by a reference extension in CI.
3. **`yolop-lsp` dogfood.** Extract the LSP control plane into an MCP binary
   packaged as an extension; run `evals/lsp_integration` against
   built-in vs extension. Exit: parity → retire the in-tree capability.
4. **Providers, then UI.** Declarative provider descriptors (after the
   Provider-catalog refactor); provider proxies and declarative UI on demand.

## Non-goals

- No in-process native code (dylibs) — ever, per the ABI analysis.
- No arbitrary TUI code from extensions.
- No hot reload, no central registry, no sandbox claims in v1.
- No second hook engine, skill format, MCP config shape, or enable/disable
  surface — every facet lands on an existing seam.

## Open questions

- Config delivery to servers: env blob vs `${config.*}` substitution vs both;
  restart-on-config-change semantics for long-lived stdio servers.
- `never_defer` budget size and whether the allowlist should be
  model-adaptive (mirrors `auto_tool_search`).
- Whether workspace-scope extensions may contribute hooks at all, or only
  with a separate per-hook approval step.
- Naming: `.agents/extensions/` (host-neutral, matches `.agents/skills/`)
  vs `.yolop/extensions/`.

# Extensions — capability servers over a subprocess protocol

Status: **proposal** (design record, nothing implemented).

## Why

Yolop's unit of functionality is the everruns **capability**: one `Capability`
implementation contributes tools, system-prompt text, config schema, hooks,
MCP servers, skills, and commands through well-defined trait seams, and the
harness is an ordered, user-editable list of capability refs
(`[[capabilities]]` in `settings.toml`). The contract is right; the delivery
is wrong: every capability is compiled in. The `lsp` capability (#236) is the
canonical example — ~3.4k lines of Rust that had to land in this repo, ride
the release train, and grow everyone's binary, including the majority who
keep it off.

An **extension** delivers a capability as an installed artifact, no
recompilation. Near-term: tools + prompt + config + hooks (the LSP class).
Later: LLM providers, UI features, everruns plugins.

**Scoping decision.** Declarative contributions (prompt text, skills,
commands, plain MCP server registrations) are arriving upstream as **everruns
plugins**: a plugin directory (Claude Code–compatible `plugin.json` dialect)
compiles to a `DeclarativeCapabilityDefinition` carried *inside* the
`plugin:{name}` capability config and hydrated at collection time. Yolop
inherits that layer by staying in lockstep; this spec does not redesign it.
What yolop must own is the tier upstream does not have: **executable,
stateful, capability-level logic in a subprocess** — the *capability server*
— and the protocol between yolop and it. That is the subject of this spec.

## Prior art (compressed)

Full survey history in the git log of this file. What bears on the protocol
tier:

- Hosts without a dynamic language runtime (goose, Crush, Claude Code, Zed
  for anything heavy) all converged on **subprocess + protocol**; in-process
  breadth (pi's ~35-event TypeScript API, OpenCode's JS middleware) is a
  property of having a scripting runtime, not a design yolop can copy.
- **goose** proves "extension = MCP server" works but also shows its ceiling:
  tools/resources/prompts only — no prompt shaping, no loop hooks, no config
  schema. That ceiling is exactly the gap between MCP and the `Capability`
  trait.
- **Zed** shows the strongest versioning story (per-version WIT snapshots)
  and the control-plane/data-plane split: extension logic decides, host- or
  extension-spawned subprocesses do the heavy work.
- **VS Code**'s durable lessons: static manifest so contributions are
  inspectable without executing code; keep extension code off the
  latency-critical path.
- **pi**'s anti-MCP argument is really about context economics (tool schemas
  burning 5–10% of the window). The antidote is declared tool policy, not a
  different transport.

## Design decisions

### D1 — The protocol is an MCP superset, not a second protocol

A capability server **is an MCP server** (JSON-RPC 2.0 over stdio) that
additionally negotiates a `yolop` extension block. MCP explicitly reserves
`experimental`/vendor capabilities and tolerates unknown methods, so this is
sanctioned, not a fork.

Why not bespoke LSP-style RPC: it would reimplement framing, handshake,
lifecycle, and tool plumbing MCP already standardizes; it would orphan the
SDK ecosystem extension authors get for free; and yolop would still need MCP
alongside it for plain tool servers — two protocols forever. Why not plain
MCP: the trait seams that make a capability a capability (config schema,
prompt contribution, tool policy, hooks) have no MCP expression — goose's
ceiling.

A capability server therefore degrades gracefully: in goose or Claude Code it
is just an MCP tool server (prefixed names, no facets); under yolop it is a
full capability. One binary, every host.

Base-protocol versioning is MCP's own. The `yolop` block carries its own
integer `protocolVersion`; yolop advertises the versions it supports in
`initialize` and refuses (with a clear startup warning, never a crash) blocks
it cannot speak.

### D2 — Persistent processes (new transport mode)

Today's stdio MCP transport spawns a process **per tool call** and tears it
down (`everruns-mcp` D2: "per-invocation spawn keeps lifecycle trivial").
That is fatal for the LSP class — warm state is the entire point (a
rust-analyzer index takes tens of seconds to build).

Capability servers get a **session-persistent connection**: spawned once per
session, kept alive, `kill_on_drop`, restarted with backoff on crash (pending
requests fail with a clear message; next call respawns — the exact policy
`lsp/manager.rs` already implements for language servers). Spawn is eager at
session start when the extension contributes prompt text or hooks (needed
before the first turn), lazy on first tool call otherwise.

This transport is useful beyond capability servers — stateful plain-MCP
servers suffer under spawn-per-call too — so it lands as an opt-in
(`"persistent": true`) for ordinary `.mcp.json` entries, with capability
servers always persistent. Candidate for upstreaming into `everruns-mcp`.

### D3 — Every trait seam gets one of three treatments

The core of the design. The `Capability` trait's seams split by how hot their
call path is:

| Treatment | Seams | Mechanism |
|---|---|---|
| **Static** — declared, no RPC after startup | id/name/description/category, `config_schema`, static system prompt, tool policy (naming, never-defer, approval/risk hints, narration templates), commands, hook *subscriptions*, dependencies, features | Package manifest + `initialize` handshake; a generic yolop-side adapter answers the trait from this cache |
| **RPC** — cold path, bounded, fallible | tool execution (`tools/call`), dynamic system prompt (`yolop/systemPrompt`, ≤1/turn, timeout + last-known-good fallback), hook firings (`yolop/hook`), user questions (MCP `elicitation` → `user_ask` bridge), config push (`yolop/configChanged`) | Requests over the persistent connection, each with a declared timeout and error policy |
| **Excluded** from the wire (v1) | `facts`, `message_filter_provider`, `model_view_provider`, `tool_definition_hooks` | Per-request hot loop, documented "cheap and side-effect free" — a cross-process round-trip there is hostile to latency and prompt-cache stability. If real demand appears, this is the future WASM tier's job, not a protocol extension |

Hooks over RPC deserve the explicit precedent: `user_hooks` today spawns a
**whole bash process per event**. A request to an already-warm process is
strictly cheaper than the mechanism yolop already ships, so hook RPC is not a
new class of cost — but it is bounded anyway: subscriptions are static
(event + tool-name matcher + `timeout_ms` + `on_error: warn|block`), a
match-all matcher must be spelled `"*"` and is called out at approval time,
and per-extension subscription count is capped.

### D4 — The manifest is the approval boundary; the handshake may only narrow it

Static facets live in **both** the package manifest and the handshake, with a
strict relationship:

- The **manifest** (`plugin.json` + `yolop` facet block) is what the user
  approves at install time — it must be inspectable *without executing the
  binary* (the VS Code lesson). It declares the server command and the upper
  bound of everything: tool names, never-defer list, hook subscriptions,
  prompt size, config schema.
- The **handshake** is the runtime truth, **clamped by the manifest**: a
  server may declare fewer tools or hooks than approved (feature-gated
  builds), but anything not in the approved manifest is refused and logged.
  A server that tries to widen its grant after installation is the exact
  attack the clamp exists for.

Content-hash approval (workspace packages inert until approved per hash,
recorded in user state, re-prompt on change) makes the boundary durable.

### D5 — Config rides the existing capability machinery

An enabled extension is one harness entry — `[[capabilities]]
ref = "ext:<name>"` — so enable/disable/configure/validate ride the existing
overrides, catalog, and `set_config` tools unchanged. The declared
`config_schema` plugs into `CapabilityCatalog` validation exactly like a
built-in's.

Delivery: validated config is passed in `initialize` params
(`yolop.config`), and pushed on change as `yolop/configChanged`; the server
answers `ok` or `restart-required` (yolop restarts it — the same
"rebuild manager on config change" semantics `LspCapability` has today).
Secrets stay in env (`${VAR}` expansion in the server spec, as `.mcp.json`
does now); config is for structure, not credentials.

### D6 — Tool policy is part of the contract, not a courtesy

The LSP eval produced two hard facts: tools deferred behind `tool_search`
stubs get ~zero adoption, and prompt guidance names exact tool names. So the
manifest declares, per tool:

- **canonical name** — surfaced unprefixed (`lsp_definition`, not
  `mcp_lsp__lsp_definition`). Registered only if it collides with nothing
  built-in or already-installed; on collision, fall back to the prefixed form
  with a startup warning.
- **`never_defer`** — schema always loaded, budgeted (≤8 per extension) so
  one package cannot blow the context window. Everything else defers behind
  `tool_search` as usual — this is the answer to pi's context-cost critique.
- **approval/risk hints** consumed by the approval capability, and optional
  narration templates (static templates beat narration RPC).

### The wire, end to end

```
yolop                                   capability server (ext:lsp)
  |-- spawn (session start; prompt facet present)
  |-- initialize { capabilities.experimental.yolop:
  |                { protocolVersions: [1], config: {...} } }
  |<- result { instructions: "<static prompt>",      # standard MCP field
  |            capabilities.experimental.yolop:
  |              { protocolVersion: 1,
  |                tools:  { canonical: true, neverDefer: [...] },
  |                hooks:  [ {event, matcher, timeoutMs, onError} ],
  |                dynamicSystemPrompt: false } }    # clamped vs manifest
  |-- notifications/initialized
  |-- tools/list                        -> seven lsp_* tools
  |   ... agent turn ...
  |-- tools/call lsp_definition         -> (server keeps rust-analyzer warm)
  |-- yolop/hook {event: post_tool_use, tool: edit_file, ...}   # if subscribed
  |   ... user edits settings ...
  |-- yolop/configChanged {config}      <- "restart-required" -> respawn
  |   ... session end ...
  |-- shutdown / SIGKILL (kill_on_drop)
```

The yolop side is one generic `ExtensionCapability` adapter implementing
`Capability`: static answers from the clamped handshake cache; `tools()`
proxied from `tools/list` with policy applied; `pre/post_tool_exec_hooks()`
manufactured from subscriptions; `system_prompt_contribution()` from
`instructions` (or the RPC when declared dynamic). Registered per
installed+enabled extension at startup — `CapabilityRegistry::register`
takes `Arc<dyn Capability>` at runtime, so no upstream change is needed to
register; only the facets that touch collection (config schema exposure)
need small hooks.

Note the deliberate reuse of MCP's standard `instructions` field for the
static prompt: even a plain MCP host that ignores every `yolop/*` method
still gets the guidance text.

## Packaging, install, trust

The package is the everruns plugin directory — the declarative layer arrives
upstream; yolop adds one facet:

```json
{
  "name": "lsp",
  "version": "0.1.0",
  "description": "Semantic code intelligence from real language servers.",
  "engines": { "yolop": ">=0.6" },
  "yolop": {
    "protocolVersion": 1,
    "capabilityServer": { "command": "yolop-lsp", "args": [] },
    "config_schema": { "$ref": "./config.schema.json" },
    "tools": {
      "canonical_names": true,
      "never_defer": ["lsp_definition", "lsp_references", "lsp_hover",
                       "lsp_diagnostics", "lsp_rename", "lsp_symbols",
                       "lsp_code_actions"]
    },
    "hooks": []
  }
}
```

- **Scopes** mirror skills/hooks/mcp: workspace
  `<workspace>/.agents/extensions/<name>/` and global
  `<config_dir>/yolop/extensions/<name>/`; workspace overrides global by
  name; malformed packages warn, never sink startup.
- **Install**: `/extensions install <git-url>[@rev] | <path>`, plus
  `list | update | remove | enable | disable` — System commands *and*
  capability tools, so both the user and the model can drive setup.
  Git installs are pinned in `extensions.lock` (source, commit, content
  hash); `update` is explicit. Path installs are referenced, not copied
  (dev loop). Binaries: the manifest names the command; resolution is PATH
  plus the package's own `bin/`; a missing binary is a call-time tool error
  with install guidance, exactly like a missing `rust-analyzer` today.
- **Trust**: a global install is consent by action (same stance as authoring
  `.mcp.json`), preceded by a printed contribution summary (servers, hooks,
  prompt size, tool names). Workspace packages arrive with someone else's
  repo: discovered but **inert until approved once per content-hash**,
  approval recorded in user state, changed package re-prompts. Hooks and
  prompt text are the sharpest injection edges and are named explicitly in
  the summary. No sandbox is claimed in v1.

## Illustration: `lsp` as a capability server

Decomposition of the existing capability:

- **Data plane** — rust-analyzer, gopls, pyright, … : already subprocesses;
  now spawned and kept warm by the extension process instead of by yolop.
- **Control plane** — `yolop-lsp`, a standalone binary: the transport-generic
  LSP client, server lifecycle manager, position-encoding conversion, and
  workspace-edit safety checks move there ~verbatim (`client.rs` is already
  transport-generic; only `manager.rs` knows processes). It exposes the seven
  `lsp_*` tools over MCP.
- **Package** — manifest above + the directive prompt text (verbatim: the
  eval showed the "call `lsp_*` FIRST" wording is load-bearing) + the same
  config schema (`servers.<key>.command/args/extensions`, timeouts). Config
  changes to the servers map answer `restart-required`, matching today's
  "rebuild manager, killing old servers" behavior.

Workspace-root delivery uses MCP `roots`; out-of-root edit rejection stays in
the extension (it owns the `WorkspaceEdit` application), while yolop's
approval hooks still see every write the tools perform.

Migration gate: the built-in stays until the extension reaches parity on
`evals/lsp_integration` (pass rate, `lsp_*` adoption, turns, tokens, plus
tool-call latency overhead measured). Parity → retire the in-tree capability;
the eval is the gate, the extension is the dogfood.

## Future facets on the same protocol

- **LLM providers.** Tier 1 is declarative (OpenAI-compatible descriptor:
  base_url, auth env, model list/profiles) once `Provider` moves from a
  closed enum to a catalog. Tier 2 rides *this* mechanism: the capability
  server exposes an OpenAI-compatible endpoint on a local socket and
  announces it in the handshake (`yolop.provider: { baseUrl, models }`) —
  the pattern Ollama normalized, and `codex_driver`'s `DriverId::external`
  already shows the driver-side seam. Native drivers stay compiled in.
- **UI features.** Extensions never run TUI code. The protocol path is
  declarative: `elicitation` already bridges to `user_ask` forms; if richer
  needs appear, handshake-declared component *trees* (upstream `a2ui`/
  `openui` precedent) rendered by yolop's own widgets.
- **everruns plugins.** A yolop extension without the `yolop` facet *is* an
  everruns plugin; hosts that grow capability-server support can adopt the
  `yolop` block as-is — it is a candidate for upstreaming under a neutral
  name once proven here.

## Alternatives considered

- **Rust dylibs** (`abi_stable`/`stabby`) — rejected: no stable ABI,
  toolchain lockstep, per-platform artifacts, no isolation.
- **In-process scripting** (pi/OpenCode model; upstream `lua` exists) —
  deferred: single language, engine surface, and a sandbox liability for
  exactly the FS/process access the LSP class needs.
- **WASM components** (wasmtime + WIT / Extism) — deferred, and *scoped*: it
  is the untrusted-marketplace endgame and the only honest home for the
  excluded hot-loop seams (D3), but WASI cannot spawn the data plane, and the
  host machinery (per-version WIT snapshots) is Zed-scale. When it comes, a
  WASM module is one more facet in this same package format, not a new unit.
- **Bespoke JSON-RPC protocol** — rejected in D1.
- **Pure MCP with no extension block** — rejected as the ceiling goose
  already demonstrates; it cannot express config schema, prompt policy,
  tool economics, or hooks.

## Rollout

1. **Persistent MCP transport.** Session-lifetime stdio connections with
   crash/respawn policy behind `"persistent": true` in `.mcp.json`; measure
   against spawn-per-call. Independently valuable; candidate upstream PR.
2. **Handshake + adapter.** `yolop` extension block (protocolVersion, tool
   policy, static prompt via `instructions`, config in `initialize`),
   `ExtensionCapability` adapter, config-schema wiring into the catalog.
   Exit: a hand-built capability server exercises every static facet in CI.
3. **Packaging + trust.** `.agents/extensions/` + global scope discovery,
   `extensions.lock`, `/extensions` verbs, manifest-clamps-handshake
   enforcement, workspace content-hash approval. Exit: install from a git
   URL to working tools in one command.
4. **Hooks + dynamic prompt RPC.** `yolop/hook`, `yolop/systemPrompt`,
   `yolop/configChanged`, elicitation bridge. Exit: a guardrail-style
   reference extension (block writes to generated files) works.
5. **`yolop-lsp` dogfood.** Extract the control plane; A/B on
   `evals/lsp_integration`; parity retires the built-in.
6. **Providers.** Descriptor tier after the Provider-catalog refactor;
   provider-proxy handshake facet after.

## Non-goals

- No in-process native code, ever (ABI analysis above).
- No TUI code from extensions.
- No hot-loop seams over the wire (facts, message filters, model views,
  tool-definition transforms) — see D3.
- No hot reload, central registry, or sandbox claims in v1.
- No second hook engine, skills format, or enable/disable surface — every
  facet lands on an existing seam.

## Open questions

- Namespace: `ext:<name>` vs reusing upstream `plugin:<name>` for
  capability-server extensions. `ext:` keeps "hydrates from serialized
  config" (upstream plugin semantics) distinct from "proxied live process",
  at the cost of a second prefix; decide when the loader lands.
- Should the persistent transport multiplex several sessions over one
  process (LSP servers are expensive to duplicate) or stay
  process-per-session (simpler isolation)? Yolop is effectively
  single-session per TUI today; background sessions may force the choice.
- `never_defer` budget size, and whether it should be model-adaptive
  (mirroring `auto_tool_search`).
- Hook RPC while the extension process is crashed: fail-open with warning
  (availability) vs fail-closed per `on_error` (integrity). Leaning: honor
  the subscription's declared `on_error`, same as hook timeouts.
- Whether workspace-scope packages may subscribe hooks at all, or only with
  per-hook approval.

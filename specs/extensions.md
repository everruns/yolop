# Extensions — capability servers over the yolop extension protocol

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
commands, static MCP server registrations) are arriving upstream as
**everruns plugins**: a plugin directory (Claude Code–compatible
`plugin.json` dialect) compiles to a `DeclarativeCapabilityDefinition`
carried *inside* the `plugin:{name}` capability config and hydrated at
collection time. Yolop inherits that layer by staying in lockstep; this spec
does not redesign it. What yolop must own is the tier upstream does not
have: **executable, stateful, capability-level logic in a subprocess** — the
*capability server* — and the protocol between yolop and it: the **yolop
extension protocol (YEP)**.

## Prior art (compressed)

Full survey history in the git log of this file. What bears on the protocol
tier:

- Hosts without a dynamic language runtime (goose, Crush, Claude Code, Zed
  for anything heavy) all converged on **subprocess + protocol**; in-process
  breadth (pi's ~35-event TypeScript API, OpenCode's JS middleware) is a
  property of having a scripting runtime, not a design yolop can copy.
- **goose** proves "extension = MCP server" works but also shows its ceiling:
  tools/resources/prompts only — no prompt shaping, no loop hooks, no config
  schema, no streaming tool output. That ceiling is exactly the gap between
  MCP and the `Capability` trait.
- **Zed** shows the strongest versioning story (per-version WIT snapshots)
  and the control-plane/data-plane split: extension logic decides, host- or
  extension-spawned subprocesses do the heavy work.
- **VS Code**'s durable lessons: static manifest so contributions are
  inspectable without executing code; keep extension code off the
  latency-critical path.
- **pi**'s anti-MCP argument is really about context economics (tool schemas
  burning 5–10% of the window). The antidote is declared tool policy, not a
  particular transport.
- **mira** (sibling project; its eval protocol is ACP-inspired) is the
  in-house existence proof for building a protocol of exactly this shape:
  stdio ndjson JSON-RPC dialect, field-classified messages, capability
  tokens + `capability_params`, `MAJOR.MINOR` with hard forward-compat
  rules, schema generated from Rust types with CI drift guards, native
  zero-dependency SDKs generated from the schema, and staged unstable
  additions. YEP adopts these conventions wholesale (see "Protocol
  construction") rather than rediscovering them.

## Design decisions

### D1 — YEP is its own protocol; MCP is a *contribution*, not the base

A capability server speaks **YEP**: yolop-owned methods and lifecycle,
JSON-RPC 2.0 as the message envelope (the same envelope LSP, DAP, and MCP
use — it is framing, not semantics) over stdio, newline-delimited. The
protocol mirrors the `Capability` trait directly instead of bending the
contract to fit another protocol's shape.

Why not an MCP superset (the previous revision of this spec recommended one;
recorded here as rejected):

- **The contract should evolve with the trait, not with MCP.** Tool policy,
  hooks, prompt contributions, config push, streaming tool output — none of
  these exist in MCP; a superset means forever expressing yolop semantics
  through someone else's extension escape hatch and tracking their spec's
  churn underneath.
- **Tool calls need a richer lifecycle than MCP offers**: streamed
  progress/partial output while a tool runs (yolop's TUI already renders
  streaming `bash` output; extension tools must not regress to
  spinner-until-done), cancellation, and structured result payloads aligned
  with `ToolExecutionResult` — native YEP messages rather than bolted-on
  progress notifications.
- **The cross-host argument inverts.** The superset's main selling point was
  "the same binary also works in goose/Claude Code." Composition achieves
  that better: an extension that wants cross-host reach *provides an MCP
  server* (next paragraph) and keeps the yolop-specific surface in YEP.

**MCP as a contribution.** An extension can announce, in its handshake:
*"I provide these MCP servers"* — each entry a name plus transport (a stdio
command, or an HTTP URL, typically `localhost` served by the extension
process itself). Yolop merges them into scoped MCP config and consumes them
through its **existing MCP client path**, exactly as if they came from
`.mcp.json`. This is not a new seam: it is the wire form of
`Capability::mcp_servers_with_config()`, which compiled-in capabilities
already use. Division of labor:

- **Native YEP tools** — streaming, stateful, policy-rich, session-scoped;
  the LSP class.
- **Contributed MCP servers** — wrapping/bundling the existing MCP
  ecosystem: an extension can manage credentials, config, or lifecycle for a
  third-party MCP server and hand yolop the endpoint. Stateful contributed
  servers should prefer an extension-hosted HTTP endpoint (the extension
  owns the process, so state survives) since yolop's stdio MCP transport is
  spawn-per-call today.

The cost of a bespoke protocol is the missing SDK ecosystem. The mitigation
is not hand-waving — it is the pattern [mira](https://github.com/everruns/mira)
already ships for its eval protocol (itself an ACP-inspired stdio dialect;
YEP joins the same family and follows `mira/docs/protocol.md` as its style
template): a schema-first contract with generated, native SDKs and a
conformance harness. See "Protocol construction" below.

### D2 — Persistent, session-scoped processes

A capability server is spawned once per session and lives for it —
persistence is intrinsic to YEP, not an option. Spawn is eager at session
start when the extension contributes prompt text or hooks (needed before the
first turn), lazy on first tool call otherwise. `kill_on_drop`; crash fails
pending requests with a clear message and the next call respawns with
backoff — the policy `lsp/manager.rs` already implements for language
servers. Graceful path: `shutdown` request, then `exit` notification,
SIGKILL after a grace window.

The worktree caveat is first-class: yolop can repoint the active workspace
mid-session (`WorkspaceHost`), so the workspace root arrives in `initialize`
**and** changes arrive as `workspace/changed` notifications; servers that
cache roots (the LSP class) must handle it.

### D3 — Every trait seam gets one of three treatments

The core of the design. The `Capability` trait's seams split by how hot
their call path is:

| Treatment | Seams | Mechanism |
|---|---|---|
| **Static** — declared, no RPC after startup | id/name/description/category, `config_schema`, static system prompt, tool definitions + policy (naming, never-defer, approval/risk hints, narration templates), **provided MCP servers**, commands, hook *subscriptions*, dependencies, features | Package manifest + `initialize` handshake; a generic yolop-side adapter answers the trait from this cache |
| **RPC** — cold path, bounded, fallible | tool execution (streamed), dynamic system prompt (≤1/turn, timeout + last-known-good fallback), hook firings, user questions, config push, workspace change | YEP requests/notifications over the persistent connection, each with a declared timeout and error policy |
| **Excluded** from the wire (v1) | `facts`, `message_filter_provider`, `model_view_provider`, `tool_definition_hooks` | Per-request hot loop, documented "cheap and side-effect free" — a cross-process round-trip there is hostile to latency and prompt-cache stability. If real demand appears, this is the future WASM tier's job, not a protocol extension |

Hooks over RPC deserve the explicit precedent: `user_hooks` today spawns a
**whole bash process per event**. A request to an already-warm process is
strictly cheaper than the mechanism yolop already ships — but bounded
anyway: subscriptions are static (event + tool-name matcher + `timeout_ms` +
`on_error: warn|block`), a match-all matcher must be spelled `"*"` and is
called out at approval time, and per-extension subscription count is capped.

### D4 — The manifest is the approval boundary; the handshake may only narrow it

Static facets live in **both** the package manifest and the handshake, with
a strict relationship:

- The **manifest** (`plugin.json` + `yolop` facet block) is what the user
  approves at install time — it must be inspectable *without executing the
  binary* (the VS Code lesson). It declares the server command and the upper
  bound of everything: tool names, never-defer list, hook subscriptions,
  provided MCP servers, prompt size, config schema.
- The **handshake** is the runtime truth, **clamped by the manifest**: a
  server may declare fewer tools, hooks, or MCP servers than approved
  (feature-gated builds), but anything not in the approved manifest is
  refused and logged. A server that tries to widen its grant after
  installation is the exact attack the clamp exists for. Provided MCP
  servers are clamped by *name and transport shape* — an approved stdio
  command cannot silently become a remote URL.

The lockfile's content hash makes the boundary durable: what was approved
is pinned, and an `update` whose manifest widens the grant re-asks with a
contribution diff before anything runs.

### D5 — Config rides the existing capability machinery

An enabled extension is one harness entry — `[[capabilities]]
ref = "ext:<name>"` — so enable/disable/configure/validate ride the existing
overrides, catalog, and `set_config` tools unchanged. The declared
`config_schema` plugs into `CapabilityCatalog` validation exactly like a
built-in's.

Delivery: validated config is passed in `initialize` params, and pushed on
change as `config/changed`; the server answers `ok` or `restart-required`
(yolop restarts it — the same "rebuild manager on config change" semantics
`LspCapability` has today). Secrets stay in env (`${VAR}` expansion in the
server spec, as `.mcp.json` does now); config is for structure, not
credentials.

### D6 — Tool policy is part of the tool definition

The LSP eval produced two hard facts: tools deferred behind `tool_search`
stubs get ~zero adoption, and prompt guidance names exact tool names. In a
yolop-owned protocol this is native — each declared tool carries:

- its **real name** (`lsp_definition` — no forced prefix; registered only if
  it collides with nothing built-in or already installed; on collision, the
  extension-qualified form with a startup warning),
- **`never_defer`** (schema always loaded, budgeted: ≤8 per extension so one
  package cannot blow the context window; everything else defers behind
  `tool_search` — the answer to pi's context-cost critique),
- **approval/risk hints** consumed by the approval capability, and an
  optional **narration template** (static templates beat narration RPC),
- **`streaming`** — whether the tool emits `tool/update` progress.

## The protocol surface

### Wire model (mira conventions, adopted)

YEP's wire model is lifted from the mira eval protocol — the ACP-inspired
stdio dialect this codebase's sibling already specifies, generates, and
tests — rather than invented fresh:

- **Transport & framing.** Child-process stdio; newline-delimited JSON, one
  UTF-8 object per line, blank lines ignored. `stdout` carries **only**
  protocol JSON; `stderr` is free for the server's logs and is never parsed.
  EOF on stdin signals clean exit (then `kill_on_drop` after grace).
- **Field-based message classification.** A line bearing `method` is a
  request (with `id`) or notification (without); only a `method`-less line
  is a response. Classification is by fields, not by direction or pipe.
- **Bidirectional from day one, with mira's reserved-seam invariants made
  live**: independent `id` spaces per direction (a response matches pending
  requests *on the same side* only), and each direction's optional methods
  are capability-negotiated — a peer never emits a method the other side
  did not advertise. YEP needs the reverse direction immediately
  (`ui/ask`, `status/changed`, `log`), so these invariants are v1 behavior,
  not a reservation.
- **Correlated notifications.** A notification cannot carry the envelope
  `id` (that would classify it as a response), so streamed `tool/update`
  events correlate to their in-flight call via `request_id` in the payload —
  the same demultiplexing key mira uses, which is what lets many tool calls
  (and their update streams) multiplex over the single pipe.
- **Errors are JSON-RPC-shaped and defaulted**: `code` (JSON-RPC
  conventions, `0` = unclassified), required `message`, optional
  `retryable` hint and structured `data`. A bare `{"message": …}` parses.
- **Cancellation by request id.** `cancel {id}` is a generic method
  addressing any in-flight request (mira semantics): best-effort, `false`
  is benign, and the cancelled call itself resolves promptly with error
  `cancelled` instead of hanging. This is the lever for user interrupts and
  turn aborts.
- **Multiplexing.** Yolop may keep many requests in flight; responses
  correlate by `id` and arrive in any order. Parallel tool calls in one
  turn ride this directly.

### Versioning (mira rules, verbatim)

`MAJOR.MINOR`, negotiated at `initialize`; v1 ships as `1.0`. Majors must
match — yolop refuses a mismatched server with a startup warning, never a
crash. Minors are additive. Forward compatibility is a hard requirement on
both sides: **ignore unknown fields** (no deny-unknown-fields on the wire),
**default missing fields**, and **feature-detect via capability tokens, not
version sniffing**. The handshake's contribution facets are capability
tokens with structured `capability_params` (open vocabulary, carried
verbatim when unrecognized): `tools`, `streaming`, `hooks`, `prompt`,
`dynamic_prompt`, `mcp_servers`, `commands`, `ui_ask`, `cancel`,
`provider` (future). A server advertising only `tools` is fully conforming.

### Methods

| Direction | Method | Kind | Purpose |
|---|---|---|---|
| →server | `initialize` | req | protocol version, session id, workspace root, locale, validated config, host feature set |
| ←server | (result) | — | identity, contributions (prompt, tools, hooks, MCP servers, commands), clamped by manifest |
| →server | `initialized` | ntf | handshake complete |
| →server | `tool/call` | req | `{tool_call_id, name, args}`; response is the final `ToolExecutionResult`-shaped payload |
| ←server | `tool/update` | ntf | streamed progress/partial output; correlates via `request_id` in the payload |
| →server | `cancel` | req | `{id}` — abort any in-flight request (mira semantics: best-effort, aborted call resolves with error `cancelled`) |
| →server | `prompt/contribution` | req | dynamic system prompt (only if declared `dynamic`); timeout + last-known-good |
| →server | `hook/fire` | req | `{event, toolName, payload}` → decision per subscription (`allow/block/mutate`) |
| →server | `config/changed` | req | new validated config → `ok` \| `restart-required` |
| →server | `workspace/changed` | ntf | active worktree/root repointed |
| ←server | `ui/ask` | req | user question/form — bridged to the `user_ask` capability |
| ←server | `status/changed` | ntf | capability status (e.g. `degraded: rust-analyzer not found`) surfaced in `/extensions list` |
| ←server | `log` | ntf | structured logs → yolop's tracing layer (`RUST_LOG` honored) |
| →server | `shutdown` / `exit` | req/ntf | graceful stop before `kill_on_drop` |

```
yolop                                   capability server (ext:lsp)
  |-- spawn (session start; prompt facet present)
  |-- initialize {protocol_version:"1.0", session_id, workspace_root,
  |               config:{...},
  |               capabilities:["streaming","hooks","ui_ask","cancel"]}
  |<- result {protocol_version:"1.0", name:"lsp",
  |           capabilities:["tools","streaming","prompt"],
  |           capability_params:{
  |             prompt:{static:"<directive text>"},
  |             tools:[{name:"lsp_definition", schema:{...},
  |                     never_defer:true, streaming:false}, ...x7]}}
  |                                     # ^ clamped vs manifest (D4)
  |-- initialized
  |   ... agent turn ...
  |-- {id:1} tool/call {tool_call_id:"t1", name:"lsp_definition", args:{...}}
  |<- {id:1} (result)                   # server keeps rust-analyzer warm
  |-- {id:2} tool/call {tool_call_id:"t2", name:"lsp_rename", args:{...}}
  |<- tool/update {request_id:2, output:"3/14 files patched"}
  |<- {id:2} (result)
  |-- {id:3} cancel {id:2}              # had it still been running:
  |<- {id:3} {cancelled:true}           #   ...and id:2 resolves error "cancelled"
  |   ... user edits settings ...
  |-- config/changed {config}          <- "restart-required" -> respawn
  |   ... session end ...
  |-- (close stdin) / shutdown          # EOF => exit; SIGKILL after grace
```

### Protocol construction (the mira pattern)

How the contract is defined, published, and kept honest — adopted wholesale
from mira, which has already proven each piece for a protocol of this exact
shape:

- **Rust types are the source of truth; the schema is generated.** Wire
  types live in one `yolop::yep` module; a schema-gen binary emits
  `schema/yep/v1/schema.json` (JSON Schema 2020-12, root `anyOf` over the
  three envelopes, every payload under `$defs`) and `meta.json` (protocol
  version, method list, capability tokens, event vocabularies). The
  directory is versioned by protocol **major**. CI runs the generator with
  `--check` so a wire change cannot merge without a matching schema update;
  a test suite validates real serialized messages against the committed
  schema, and a conformance corpus lives beside it (`schema/yep/v1/
  conformance/`).
- **SDKs are native, zero-dependency libraries, never FFI bindings.** The
  protocol is the seam by design; bindings would re-couple what the wire
  decouples. Each SDK ships a small codegen with its own `--check` drift
  mode that generates wire types from `schema.json` and protocol metadata
  from `meta.json` — the version string and method/capability vocabulary
  are generated, not hardcoded — plus a hand-written ergonomic layer: a
  `serve()` stdio loop and tool/hook registration helpers. Rust
  (`yolop-extension`, dogfooded by `yolop-lsp`) first; TypeScript and
  Python when demand shows, following `mira/specs/sdks.md` including its
  drift-guard table (handled-methods ⊇ meta methods, advertised
  capabilities ⊆ meta tokens, emitted messages validate against schema).
- **Unstable staging.** New wire *structure* develops behind a
  `yep-unstable` cargo feature; the schema generator builds without it, so
  the committed schema describes only the stable protocol and an addition
  reaches the artifact (and a minor bump) only when promoted. Open
  vocabularies (capability tokens, `capability_params`, metadata maps)
  extend without any bump at all.
- **Conformance harness in the product**: `/extensions doctor <cmd>` drives
  a server through handshake, tool call, streaming, cancellation, and
  config push, validating every message against the committed schema — the
  runtime dual of the CI guards, pointed at third-party servers.

### The adapter

The yolop side is one generic `ExtensionCapability` adapter implementing
`Capability`: static answers from the clamped handshake cache; `tools()`
manufactured from declared tools (calls proxied as `tool/call`, updates
streamed to the TUI); `mcp_servers_with_config()` returns the contributed
endpoints for the runtime's scoped-MCP merge; `pre/post_tool_exec_hooks()`
manufactured from subscriptions; `system_prompt_contribution()` from the
static text or the RPC. Registered per installed+enabled extension at
startup — `CapabilityRegistry::register` takes `Arc<dyn Capability>` at
runtime, so registration needs no upstream change.

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
    "protocol_version": "1.0",
    "capabilityServer": { "command": "yolop-lsp", "args": [] },
    "config_schema": { "$ref": "./config.schema.json" },
    "tools": [
      { "name": "lsp_definition", "never_defer": true },
      { "name": "lsp_references", "never_defer": true },
      { "name": "lsp_hover",      "never_defer": true },
      { "name": "lsp_diagnostics","never_defer": true },
      { "name": "lsp_rename",     "never_defer": true, "streaming": true },
      { "name": "lsp_symbols",    "never_defer": true },
      { "name": "lsp_code_actions","never_defer": true }
    ],
    "mcpServers": [],
    "hooks": []
  }
}
```

- **One scope: global.** Packages live in
  `<config_dir>/yolop/extensions/<name>/`, installed per user; malformed
  packages warn, never sink startup. There is deliberately **no workspace
  scope** (no `.agents/extensions/` discovered from repos): a repository
  must not carry agent-specific machinery — committing yolop extensions to
  a repo would quietly couple that project to one agent. Projects keep
  using the agent-neutral surfaces they already have (`.mcp.json`,
  `.agents/skills/`, `.agents/hooks.json`); a project README may *recommend*
  extensions, and the user installs them once, globally, by choice.
- **Install**: `/extensions install <source>`, plus
  `list | update | remove | enable | disable | doctor` — System commands
  *and* capability tools, so both the user and the model can drive setup.
  Sources, all pinned in `extensions.lock` and updated only explicitly:
  - `<git-url>[@rev]` — cloned into the global dir; lock records source,
    resolved commit, content hash. Carries the package; the server binary
    must already be resolvable (PATH or the package's `bin/`) — a missing
    binary is a call-time tool error with install guidance, exactly like a
    missing `rust-analyzer` today.
  - `crates.io:<crate>[@version]` — provisions package *and binary* in one
    step, **with no cargo or Rust toolchain required**. The pieces that
    don't need a toolchain are compiled into yolop: the sparse index
    (`index.crates.io`) is plain HTTPS + JSON and the `.crate` file is a
    checksummed tar.gz, so yolop resolves the version, downloads the
    tarball, verifies the registry checksum, and reads the extension
    manifest from it — `plugin.json` at the crate root or
    `[package.metadata.yolop]` in `Cargo.toml` — **without executing
    anything**, preserving the D4 invariant. Binary provisioning after
    consent, in order:
    1. **Prebuilt artifact (primary).** The crate's metadata names
       per-platform release artifacts (URL template + sha256 per target,
       `cargo-binstall`-compatible metadata accepted); yolop downloads the
       artifact for the host target, verifies the digest, and unpacks it
       into the package dir. Chain of custody: the digests live inside the
       registry-checksummed crate, so the lock's crate checksum transitively
       pins the binary.
    2. **`cargo install --locked --root <package dir>` (secondary).** Only
       if no artifact matches the host target *and* a toolchain happens to
       be present.
    3. Otherwise: install completes package-only and the missing binary is
       a call-time tool error with guidance — same policy as git installs.
    Lock records crate, version, registry checksum, and the artifact
    digest actually installed.
  - `<path>` — referenced in place, not copied (dev loop).
- **Trust**: install is consent by action (same stance as authoring
  `.mcp.json`), preceded by a printed contribution summary (server command,
  tools, hooks, MCP servers, prompt size) — readable straight from the
  manifest, without executing the binary. `update` shows a **contribution
  diff** against the approved manifest and re-asks when the grant widened
  (new tools, new hooks, a changed server command); a hash-identical update
  is silent. Hooks and prompt text are the sharpest injection edges and are
  named explicitly in both summaries. No sandbox is claimed in v1.

## Illustration: `lsp` as a capability server

Decomposition of the existing capability:

- **Data plane** — rust-analyzer, gopls, pyright, … : already subprocesses;
  now spawned and kept warm by the extension process instead of by yolop.
- **Control plane** — `yolop-lsp`, a standalone binary on the reference SDK:
  the transport-generic LSP client, server lifecycle manager,
  position-encoding conversion, and workspace-edit safety checks move there
  ~verbatim (`client.rs` is already transport-generic; only `manager.rs`
  knows processes). It exposes the seven `lsp_*` tools natively over YEP —
  streaming suits `lsp_rename` (per-file progress on large workspaces).
- **Package** — manifest above + the directive prompt text (verbatim: the
  eval showed the "call `lsp_*` FIRST" wording is load-bearing) + the same
  config schema (`servers.<key>.command/args/extensions`, timeouts). Config
  changes to the servers map answer `restart-required`, matching today's
  "rebuild manager, killing old servers" behavior. `workspace/changed`
  re-roots the client, matching today's `WorkspaceHost` repointing.

Migration gate: the built-in stays until the extension reaches parity on
`evals/lsp_integration` (pass rate, `lsp_*` adoption, turns, tokens, plus
tool-call latency overhead measured). Parity → retire the in-tree
capability; the eval is the gate, the extension is the dogfood — for both
the protocol and the SDK.

## Future facets on the same protocol

- **LLM providers.** Tier 1 is declarative (OpenAI-compatible descriptor:
  base_url, auth env, model list/profiles) once `Provider` moves from a
  closed enum to a catalog. Tier 2 rides the handshake: the server announces
  `provider: { baseUrl: "http://127.0.0.1:<port>/v1", models: [...] }` — an
  OpenAI-compatible endpoint it hosts (the pattern Ollama normalized;
  `codex_driver`'s `DriverId::external` shows the driver-side seam). Native
  drivers stay compiled in.
- **UI features.** Extensions never run TUI code. `ui/ask` bridges to
  `user_ask` forms now; if richer needs appear, handshake-declared component
  *trees* (upstream `a2ui`/`openui` precedent) rendered by yolop's own
  widgets.
- **everruns plugins.** A package without the `yolop` facet *is* an everruns
  plugin and loads through the upstream declarative path; the facet is
  additive. If upstream grows capability-server support, YEP is the candidate
  to upstream under a neutral name once proven here.

## Alternatives considered

- **MCP superset** (previous revision's recommendation) — rejected: the
  contract would be shaped by, and forever versioned under, a protocol that
  lacks tool streaming, hooks, prompt policy, and config semantics; the
  cross-host benefit is achieved better by composition — extensions
  *provide* MCP servers through the handshake instead of *being* one.
- **Pure MCP with no extension surface** — rejected as the ceiling goose
  already demonstrates: tools only, no capability-level contract. Still
  fully supported for plain tool servers via `.mcp.json` and via
  extension-provided MCP servers.
- **Rust dylibs** (`abi_stable`/`stabby`) — rejected: no stable ABI,
  toolchain lockstep, per-platform artifacts, no isolation.
- **In-process scripting** (pi/OpenCode model; upstream `lua` exists) —
  deferred: single language, engine surface, and a sandbox liability for
  exactly the FS/process access the LSP class needs.
- **WASM components** (wasmtime + WIT / Extism) — deferred, and *scoped*: it
  is the untrusted-marketplace endgame and the only honest home for the
  excluded hot-loop seams (D3), but WASI cannot spawn the data plane, and
  the host machinery (per-version WIT snapshots) is Zed-scale. When it
  comes, a WASM module is one more facet in this same package format, not a
  new unit.

## Rollout

1. **Protocol core + schema + SDK.** `initialize`/`initialized`,
   `tool/call` + `tool/update` + `cancel`, `shutdown`; wire types +
   schema-gen + `schema/yep/v1/` with CI `--check` and a conformance
   corpus; the `ExtensionCapability` adapter; the `yolop-extension`
   reference crate; `/extensions doctor`. Exit: a hand-built server passes
   conformance and its tools stream in the TUI.
2. **Packaging + trust.** Global-scope discovery, `extensions.lock`,
   `/extensions` verbs, manifest-clamps-handshake enforcement,
   update-time contribution diffs, config schema into the catalog +
   `config/changed`. Exit: install from a git URL to working tools in one
   command.
3. **Contributed MCP servers.** Handshake `mcpServers` merged into scoped
   MCP config through the existing client; name/transport clamping. Exit: an
   extension wrapping a third-party MCP server (credentials + lifecycle)
   works end to end.
4. **Hooks + dynamic prompt + ui/ask.** `hook/fire`, `prompt/contribution`,
   `ui/ask`, `workspace/changed`. Exit: a guardrail-style reference
   extension (block writes to generated files) works.
5. **`yolop-lsp` dogfood.** Extract the control plane onto the SDK; A/B on
   `evals/lsp_integration`; parity retires the built-in.
6. **Providers.** Descriptor tier after the Provider-catalog refactor;
   provider handshake facet after.

## Non-goals

- No in-process native code, ever (ABI analysis above).
- No TUI code from extensions.
- No hot-loop seams over the wire (facts, message filters, model views,
  tool-definition transforms) — see D3.
- No workspace scope — repos never carry (or auto-discover) yolop
  extensions; projects stay agent-neutral.
- No hot reload, central registry, or sandbox claims in v1.
- No second hook engine, skills format, or enable/disable surface — every
  facet lands on an existing seam.

## Open questions

- Namespace: `ext:<name>` vs reusing upstream `plugin:<name>`. `ext:` keeps
  "hydrates from serialized config" (upstream plugin semantics) distinct
  from "proxied live process", at the cost of a second prefix; decide when
  the loader lands.
- Multiplex several sessions over one server process (LSP servers are
  expensive to duplicate) vs process-per-session (simpler isolation)? Yolop
  is effectively single-session per TUI today; background sessions may force
  the choice. The protocol reserves `session_id` on every request either
  way.
- `never_defer` budget size, and whether it should be model-adaptive
  (mirroring `auto_tool_search`).
- Hook RPC while the server is crashed: fail-open with warning
  (availability) vs fail-closed per `on_error` (integrity). Leaning: honor
  the subscription's declared `on_error`, same as hook timeouts.
- Whether contributed *stdio* MCP servers should get a persistent connection
  mode (today's transport is spawn-per-call), or whether stateful cases
  should always be extension-hosted HTTP endpoints.

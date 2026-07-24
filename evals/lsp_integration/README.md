# lsp_integration — isolated A/B study for yolop's LSP capability

A [Mira](https://github.com/everruns/mira) eval **study** dedicated to the
optional `lsp` capability ([`knowledge/specs/lsp.md`](../../knowledge/specs/lsp.md)): the same
semantic-navigation tasks run with the capability **off** (yolop out of the
box) and **on** (`[[capabilities]] ref = "lsp"` in a per-case `settings.toml`),
so the capability can be judged on pass rate, adoption, turns, tool calls,
tokens, and cost.

It is deliberately separate from [`harness_basic/`](../harness_basic/): that
study keeps a small generic sample set for broad feature A/Bs, while every
sample here is a **trap for lexical navigation** — cases where grep/ast-grep
is wrong or wasteful and a language server is precise. Structurally it is the
same pure-Rust `mira-eval` SDK pattern, driving headless `yolop -p` per case
and mining the session `events.jsonl`.

## The matrix

Samples × two axes:

| Axis | Values | Where |
|------|--------|-------|
| **target** (model) | `anthropic/claude-sonnet-4-5` · `anthropic/claude-opus-4-8` · `openai/gpt-5.5` · `openrouter/z-ai/glm-5.2` | `targets()` in `src/main.rs` |
| **harness** | `default` (LSP off — baseline) · `lsp` (capability enabled) | `HARNESS_VARIANTS` |

Targets gate on provider key env vars and are skipped when the key is missing.
Reasoning effort is intentionally not an axis — yolop's per-model default is
used everywhere; this study isolates one variable.

## Samples — every one is a lexical-search trap

| Sample | Language | Task | The trap | Passes when |
|--------|----------|------|----------|-------------|
| `rename-with-decoys` | Python | Rename `fetch_records` → `load_records` project-wide | Same-named *method* on an unrelated class (+ its call site), an aliased import (`as fr`), and a wire-format string constant — all must survive; a lexical rename corrupts them | Definition/imports/call sites renamed, all three decoys intact |
| `count-call-sites` [smoke] | Python | "How many call sites does `normalize` have?" | Decoy method `Vector.normalize` + its call + a TODO comment mentioning `normalize(` — grep over-counts | Response contains the true count (`3`) |
| `find-definition-through-reexport` [smoke] | Python | "Which file defines the `format_amount` main.py uses?" | Two-hop re-export chain (`sdk/__init__` → `sdk/api` → `sdk/internal/money`), plus a deprecated same-named `def` that grep surfaces first | Response names `sdk/internal/money.py` |
| `diagnose-broken-call` | Python | Find the one call that doesn't match its callee's signature, no shell commands | Signature knowledge; `lsp_diagnostics` pinpoints it, the baseline reads everything | Response names `billing/invoice.py` |
| `diagnose-type-mismatch-rust` [rust] | Rust | Find the one compile error, no cargo | Exercises rust-analyzer through yolop's pull-diagnostics path | Response names `src/report.rs` |

Prompts state *intent* naturally ("do not change unrelated code") without
naming any tool, so the A/B measures the capability plus organic adoption —
not prompt steering.

## What a run reports

Per case, beyond pass/fail `checks`:

- **`lsp_used` scorer** — on the `lsp` variant, did the model call any
  `lsp_*` tool at all? A failed `lsp_used` next to a passed `checks` means the
  task was solved *around* the capability — an adoption finding, not a
  capability one. N/A on the baseline.
- **Metrics** — `lsp_tool_calls`, `lsp_tool_calls_failed`, `turns`,
  `llm_calls`, `tool_calls_failed`, `exploration_tools_before_first_mutation`,
  `agent_reported_ms`, tokens and cost; `tool_calls` carries the full ordered
  tool-name list for trajectory reading.
- **Guardrails** — `succeeded`, `turns_within(32)`, `tool_calls_within(64)`,
  `cost_within($2)`.

## Language servers on the eval host

The `lsp` variant needs the sample's server binary on PATH; a missing server
reports the case as **infra N/A** (never a model failure), so a partial host
still yields a clean run. `./bootstrap.sh` installs both:

- `pyright-langserver` (Python samples) — `npm install -g pyright` or
  `uv tool install pyright`
- `rust-analyzer` (the `rust`-tagged sample) — `rustup component add rust-analyzer`

Server processes spawn lazily inside yolop per case and die with it
(`kill_on_drop`); cases never share server state because every case gets a
fresh workdir and config tree.

## Setup and usage

```bash
cd evals/lsp_integration
./bootstrap.sh            # yolop release build + mira + language servers

mira list                 # the eval, samples, targets, axes

# Cheap sanity check: the 2 read-only smoke samples, LSP off vs on (4 cases).
doppler run -- mira run --preset smoke --group-by harness

# The core A/B: every sample, one model, LSP off vs on.
doppler run -- mira run --preset compare --group-by harness

# Just the rust-analyzer pull-diagnostics case.
doppler run -- mira run --preset rust --group-by harness

# Cross-model adoption check, LSP on everywhere.
doppler run -- mira run --preset models --group-by target
```

`LSP_INTEGRATION_YOLOP_BIN` overrides the yolop binary under test (falls back
to `target/release/yolop`, then `target/debug/yolop` at the repo root);
`LSP_INTEGRATION_TIMEOUT_S` caps each case (default 600).

Runs archive to `results/<run_id>/` (`report.json`, `report.html`, `meta.json`,
per-case `cases/`); resume with `mira run --resume <run_id>`. Raw per-case
`events.jsonl` logs land under the gitignored `.cache/sessions/` and each
case's `metadata.session_log` records the path.

## Reading the A/B

The headline questions, in order:

1. **Correctness** — does `checks` pass rate move on `rename-with-decoys`?
   That sample is the moat: a lexical rename corrupts a decoy and fails a
   check; `lsp_rename` cannot.
2. **Adoption** — is `lsp_used` passing? If not, the capability's system
   prompt guidance (not its tools) is what needs work.
3. **Efficiency** — on the read-only samples, compare `turns`, `tool_calls`,
   input tokens, and `agent_reported_ms`. Server startup makes the *first*
   LSP call per case slower; judge end-to-end cost, not per-call latency.

## Verified results (2026-07-08, runs `f175` → `f80c`)

The first full run surfaced two real capability bugs — pyright stalling
without a `didChangeConfiguration` push, and pyright answering cross-file
queries with empty-but-successful results during warmup (which sent one model
into a 173-`read_file` spiral) — both fixed and covered by tests. After those
fixes plus the adoption levers (never-defer schemas, directive prompt guidance),
the clean run `20260708T064759Z-f80c` (opus-4.8 + gpt-5.5, 20 cases) showed:
**20/20 checks pass on both variants** (samples don't yet separate correctness
for frontier models), organic adoption in 5/10 LSP cases (9 `lsp_*` calls,
0 failures), and shorter trajectories where adopted — opus averaged 5.4 turns /
$0.61 with LSP vs 8.6 turns / $1.02 without; gpt-5.5 used fewer turns/tools but
paid a small input-token overhead from the always-loaded schemas. Single-trial,
n=5 per cell: treat as directional until samples are hardened and trials added.

## Tests

`cargo test` in this directory: dataset/trap consistency, settings variants,
adoption mining and scorer gating, the availability gate, and an offline
end-to-end run of the real yolop binary (llmsim provider, both variants — no
API key needed).

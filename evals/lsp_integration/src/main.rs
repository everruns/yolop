//! lsp_integration — an isolated A/B study for yolop's optional **LSP
//! capability** (`[[capabilities]] ref = "lsp"`, see `specs/lsp.md`).
//!
//! A [Mira](https://github.com/everruns/mira) eval study on the `mira-eval`
//! Rust SDK, structured like `harness_basic/` (the subject spawns headless
//! `yolop -p` per case and mines the session `events.jsonl`) but deliberately
//! separate: every sample here is a **semantic-navigation trap** where lexical
//! search (grep/ast-grep) is wrong or wasteful and a language server is
//! precise — decoy same-name symbols that must survive a rename, re-export
//! chains hiding a definition, signature bugs findable via diagnostics
//! without running a build.
//!
//! The matrix is samples × two axes values on one axis:
//!   * **target** — provider models (Anthropic, OpenAI, OpenRouter)
//!   * **harness** — `default` (LSP off, yolop out of the box) vs `lsp`
//!     (the capability enabled via a per-case `settings.toml`)
//!
//! Beyond pass rate, the study reports **adoption**: `lsp_tool_calls` /
//! `lsp_tool_calls_failed` metrics and an `lsp_used` scorer, because an A/B
//! where the model never touched the LSP tools measures prompt phrasing, not
//! the capability. Run with the `mira` host CLI from this directory
//! (`mira list`, `mira run --preset smoke`); see README.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mira::scorer::{Scorer, cost_within, scorer, succeeded, tool_calls_within, turns_within};
use mira::subject::subject_fn;
use mira::{Dataset, Eval, RunCx, Sample, Score, Target, Transcript, eval};
use serde_json::{Value, json};

// ============================================================================
// Matrix
// ============================================================================

/// One yolop configuration under test. `settings` is TOML appended to the
/// study's base `settings.toml`; `default` is yolop out of the box, where the
/// LSP capability is registered but not enabled.
struct HarnessVariant {
    name: &'static str,
    settings: &'static str,
}

const HARNESS_VARIANTS: &[HarnessVariant] = &[
    HarnessVariant {
        name: "default",
        settings: "",
    },
    HarnessVariant {
        name: "lsp",
        settings: "[[capabilities]]\nref = \"lsp\"\n",
    },
];

/// Study plumbing, not a variant under test: every case runs in a plain temp
/// dir (not a git repo) and file scorers read edits back from it, so yolop's
/// linked-worktree mode must stay off for all variants.
const BASE_SETTINGS: &str = "worktrees = \"off\"\n";

fn settings_for_variant(name: &str) -> Option<String> {
    HARNESS_VARIANTS
        .iter()
        .find(|v| v.name == name)
        .map(|v| format!("{BASE_SETTINGS}{}", v.settings))
}

/// The model matrix. Every target gates on its provider key env var and is
/// skipped (not failed) when it is missing, so a keyless `mira run` is a
/// no-op rather than a wall of failures.
fn targets() -> Vec<Target> {
    vec![
        Target::anthropic("claude-sonnet-4-5"),
        Target::anthropic("claude-opus-4-8"),
        Target::openai("gpt-5.5"),
        Target::cloud("openrouter", "z-ai/glm-5.2", "OPENROUTER_API_KEY"),
    ]
}

// ============================================================================
// Dataset — semantic-navigation traps with declarative checks
// ============================================================================
//
// Each sample declares:
//   * `checks` — file/response assertions graded by the generic scorer, and
//   * `lsp_server` — the language-server binary the `lsp` variant needs on
//     PATH (the case reports infra-N/A when it is missing, never a failure).
//
// Samples lean on pyright (fast startup, one host dependency) plus one
// rust-analyzer case that exercises yolop's pull-diagnostics path.

fn dataset() -> Dataset {
    Dataset::new(vec![
        rename_with_decoys(),
        count_call_sites(),
        find_definition_through_reexport(),
        diagnose_broken_call(),
        diagnose_type_mismatch_rust(),
    ])
}

/// The flagship rename trap: same-named method on an unrelated class, an
/// aliased import, and a wire-format string constant — all of which a lexical
/// rename corrupts and `lsp_rename` leaves alone.
fn rename_with_decoys() -> Sample {
    Sample::new(
        "rename-with-decoys",
        "Rename the top-level function `fetch_records` defined in fetcher.py to \
         `load_records` across the project: its definition, all imports, and all \
         call sites. Do not change unrelated code: methods on classes that happen \
         to share the name, or string constants. Behavior must not change. Make \
         the edits directly; do not ask questions.",
    )
    .file(
        "fetcher.py",
        "def fetch_records(path):\n    with open(path) as f:\n        return f.read().splitlines()\n",
    )
    .file(
        "app.py",
        "from fetcher import fetch_records\n\n\ndef main():\n    for row in fetch_records(\"data.txt\"):\n        print(row)\n",
    )
    .file(
        "report.py",
        "import fetcher\n\n\ndef count(path):\n    return len(fetcher.fetch_records(path))\n",
    )
    .file(
        "cli.py",
        "from fetcher import fetch_records as fr\n\n\ndef run(path):\n    return fr(path)\n",
    )
    .file(
        "vendor_client.py",
        "class VendorClient:\n    \"\"\"Third-party API client; unrelated to fetcher.py.\"\"\"\n\n    def fetch_records(self):\n        return [\"remote\"]\n",
    )
    .file(
        "snapshots.py",
        "from vendor_client import VendorClient\n\n\ndef snapshot():\n    return VendorClient().fetch_records()\n",
    )
    .file(
        "telemetry.py",
        "# Historical wire-format event name; consumers depend on this exact string.\nLEGACY_FETCH_EVENT = \"fetch_records\"\n",
    )
    .meta("kind", "refactor")
    .meta("lsp_server", "pyright-langserver")
    .meta(
        "checks",
        json!([
            {"file": "fetcher.py", "contains": ["def load_records"], "lacks": ["fetch_records"]},
            {"file": "app.py", "contains": ["load_records"], "lacks": ["fetch_records"]},
            {"file": "report.py", "contains": ["load_records"], "lacks": ["fetch_records"]},
            {"file": "cli.py", "contains": ["load_records as fr"], "lacks": ["fetch_records"]},
            {"file": "vendor_client.py", "contains": ["def fetch_records"]},
            {"file": "snapshots.py", "contains": [".fetch_records()"]},
            {"file": "telemetry.py", "contains": ["\"fetch_records\""]}
        ]),
    )
}

/// Read-only reference counting: grep over-counts (decoy method, decoy call,
/// TODO comment); `lsp_references` returns the precise set.
fn count_call_sites() -> Sample {
    Sample::new(
        "count-call-sites",
        "How many call sites does the function `normalize` defined in \
         text/clean.py have in this project? Count function calls only — not \
         the definition, not import statements, and not methods on classes \
         that happen to share the name. Reply with just the number.",
    )
    .file("text/__init__.py", "")
    .file(
        "text/clean.py",
        "def normalize(s):\n    return s.strip().lower()\n",
    )
    .file(
        "ingest.py",
        "from text.clean import normalize\n\n# TODO: add a separate normalize(path) helper for file paths someday.\n\n\ndef ingest(lines):\n    seen = []\n    for line in lines:\n        seen.append(normalize(line))\n    return seen\n\n\ndef ingest_one(line):\n    return normalize(line)\n",
    )
    .file(
        "stats.py",
        "import text.clean as tc\n\n\ndef vocab(words):\n    return {tc.normalize(w) for w in words}\n",
    )
    .file(
        "vector.py",
        "class Vector:\n    def normalize(self):\n        return self\n\n\ndef unit(v):\n    return v.normalize()\n",
    )
    .tag("smoke")
    .meta("kind", "navigation")
    .meta("lsp_server", "pyright-langserver")
    .meta("checks", json!([{"response_contains": ["3"]}]))
}

/// Definition buried behind a two-hop re-export chain, with a deprecated
/// same-named decoy definition that grep surfaces first.
fn find_definition_through_reexport() -> Sample {
    Sample::new(
        "find-definition-through-reexport",
        "main.py calls `format_amount`. Which file contains the actual `def` of \
         the `format_amount` that main.py uses? Reply with only the relative \
         file path.",
    )
    .file(
        "main.py",
        "from sdk import format_amount\n\nprint(format_amount(1234.5, \"USD\"))\n",
    )
    .file(
        "sdk/__init__.py",
        "from .api import format_amount\n\n__all__ = [\"format_amount\"]\n",
    )
    .file(
        "sdk/api.py",
        "# Public API surface; implementations live in internal modules.\nfrom .internal.money import format_amount\n",
    )
    .file("sdk/internal/__init__.py", "")
    .file(
        "sdk/internal/money.py",
        "def format_amount(value, currency):\n    return f\"{value:,.2f} {currency}\"\n",
    )
    .file("legacy/__init__.py", "")
    .file(
        "legacy/formatting.py",
        "# Deprecated: superseded by the sdk package; kept for reference only.\ndef format_amount(value, currency):\n    return str(value) + \" \" + currency\n",
    )
    .tag("smoke")
    .meta("kind", "navigation")
    .meta("lsp_server", "pyright-langserver")
    .meta(
        "checks",
        json!([{"response_contains": ["sdk/internal/money.py"]}]),
    )
}

/// One arity bug across a small project; `lsp_diagnostics` pinpoints it
/// without a build, the baseline has to read everything.
fn diagnose_broken_call() -> Sample {
    Sample::new(
        "diagnose-broken-call",
        "There is exactly one bug in this project: a function call that does not \
         match the callee's signature. Without running any shell commands, find \
         it and reply with only the relative path of the file containing the \
         incorrect call.",
    )
    .file("billing/__init__.py", "")
    .file(
        "billing/pricing.py",
        "def line_total(quantity, unit_price):\n    return quantity * unit_price\n",
    )
    .file(
        "billing/invoice.py",
        "from .pricing import line_total\n\n\ndef invoice_total(items):\n    total = 0\n    for item in items:\n        total += line_total(item.quantity, item.unit_price, item.discount)\n    return total\n",
    )
    .file(
        "billing/shipping.py",
        "from .pricing import line_total\n\n\ndef shipping_total(parcels):\n    return sum(line_total(p.quantity, p.rate) for p in parcels)\n",
    )
    .file(
        "models.py",
        "class Item:\n    def __init__(self, quantity, unit_price, discount):\n        self.quantity = quantity\n        self.unit_price = unit_price\n        self.discount = discount\n",
    )
    .file(
        "main.py",
        "from billing.invoice import invoice_total\nfrom models import Item\n\nprint(invoice_total([Item(2, 10, 0)]))\n",
    )
    .meta("kind", "diagnostics")
    .meta("lsp_server", "pyright-langserver")
    .meta("checks", json!([{"response_contains": ["billing/invoice.py"]}]))
}

/// Rust flavor of the diagnostics case; exercises yolop's pull-diagnostics
/// path through rust-analyzer's native type inference (no cargo build).
fn diagnose_type_mismatch_rust() -> Sample {
    Sample::new(
        "diagnose-type-mismatch-rust",
        "There is exactly one compile error in this crate. Without running cargo \
         or any shell command, identify it and reply with only the relative path \
         of the file containing the error.",
    )
    .file(
        "Cargo.toml",
        "[package]\nname = \"seed\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .file(
        "src/lib.rs",
        "pub mod parse;\npub mod report;\npub mod stats;\n",
    )
    .file(
        "src/parse.rs",
        "pub fn parse_score(raw: &str) -> Option<u32> {\n    raw.trim().parse().ok()\n}\n",
    )
    .file(
        "src/stats.rs",
        "pub fn mean(xs: &[u32]) -> f64 {\n    if xs.is_empty() {\n        return 0.0;\n    }\n    xs.iter().copied().sum::<u32>() as f64 / xs.len() as f64\n}\n",
    )
    .file(
        "src/report.rs",
        "use crate::parse::parse_score;\n\npub fn summary(raw: &str) -> String {\n    let score: u32 = parse_score(raw);\n    format!(\"score: {score}\")\n}\n",
    )
    .tag("rust")
    .meta("kind", "diagnostics")
    .meta("lsp_server", "rust-analyzer")
    .meta("checks", json!([{"response_contains": ["src/report.rs"]}]))
}

// ============================================================================
// Scoring
// ============================================================================

/// Grades each sample against its own `checks` metadata (file contents captured
/// from the workdir after the run, and/or the final response).
fn checks_scorer() -> Box<dyn Scorer> {
    scorer("checks", |sample, t| {
        let Some(specs) = sample.metadata.get("checks").and_then(Value::as_array) else {
            return Score::na("checks", "sample declares no checks");
        };
        let mut passed = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for spec in specs {
            run_check(spec, t, &mut passed, &mut failures);
        }
        if failures.is_empty() {
            Score::pass("checks", format!("{passed} check(s) passed"))
        } else {
            Score::fail("checks", failures.join("; "))
        }
    })
}

fn run_check(spec: &Value, t: &Transcript, passed: &mut usize, failures: &mut Vec<String>) {
    let strings = |key: &str| -> Vec<&str> {
        spec.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    };
    if let Some(path) = spec.get("file").and_then(Value::as_str) {
        let Some(contents) = t.files.get(path) else {
            failures.push(format!("no such file: {path}"));
            return;
        };
        for needle in strings("contains") {
            if contents.contains(needle) {
                *passed += 1;
            } else {
                failures.push(format!("{path} missing {needle:?}"));
            }
        }
        for needle in strings("lacks") {
            if contents.contains(needle) {
                failures.push(format!("{path} still contains {needle:?}"));
            } else {
                *passed += 1;
            }
        }
    }
    for needle in strings("response_contains") {
        if t.final_response.contains(needle) {
            *passed += 1;
        } else {
            failures.push(format!("response missing {needle:?}"));
        }
    }
}

/// Adoption signal: on the `lsp` variant, did the model actually call any
/// `lsp_*` tool? A failed `lsp_used` next to a passed `checks` means the task
/// was solved *around* the capability — an adoption finding, not a capability
/// one. N/A on the baseline variant.
fn lsp_used_scorer() -> Box<dyn Scorer> {
    scorer("lsp_used", |_sample, t| {
        if t.metadata.get("harness") != Some(&json!("lsp")) {
            return Score::na("lsp_used", "baseline variant; LSP tools not offered");
        }
        let calls = t.metrics.get("lsp_tool_calls").copied().unwrap_or(0.0);
        if calls > 0.0 {
            Score::pass("lsp_used", format!("{calls} lsp_* tool call(s)"))
        } else {
            Score::fail("lsp_used", "LSP enabled but no lsp_* tool was called")
        }
    })
}

// ============================================================================
// Subject — spawn `yolop -p` and mine its events.jsonl
// ============================================================================

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The yolop binary under test: `LSP_INTEGRATION_YOLOP_BIN` override, else the
/// repo's release build, else the debug build.
fn yolop_bin() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("LSP_INTEGRATION_YOLOP_BIN")
        && !explicit.trim().is_empty()
    {
        let p = PathBuf::from(explicit);
        return if p.exists() {
            Ok(p)
        } else {
            Err(format!(
                "LSP_INTEGRATION_YOLOP_BIN not found: {}",
                p.display()
            ))
        };
    }
    for profile in ["release", "debug"] {
        let p = repo_root().join("target").join(profile).join("yolop");
        if p.exists() {
            return Ok(p);
        }
    }
    Err(
        "yolop binary not found — run `cargo build --release` at the repo root, \
         or set LSP_INTEGRATION_YOLOP_BIN"
            .to_string(),
    )
}

fn timeout_s() -> u64 {
    std::env::var("LSP_INTEGRATION_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
}

/// PATH lookup without spawning: several language servers (pyright) exit
/// non-zero on `--version`, so "runs cleanly" is the wrong availability probe.
fn binary_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

static CASE_SEQ: AtomicU64 = AtomicU64::new(0);

/// yolop validates `--session` ids as `session_` + 32 hex chars.
fn fresh_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed) ^ ((std::process::id() as u64) << 32);
    format!("session_{nanos:016x}{seq:016x}")
}

/// What one events.jsonl yields. Mirrors `harness_basic`: usage/cost only from
/// `output.message.completed` (yolop repeats usage on `reason.completed`),
/// tool calls from `tool.completed`, plus this study's LSP adoption counters.
#[derive(Default)]
struct Mined {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
    llm_calls: u64,
    iterations: u64,
    turns: u64,
    reason_ms: u64,
    turn_ms: u64,
    tool_calls: Vec<String>,
    tool_calls_failed: u64,
    lsp_tool_calls: u64,
    lsp_tool_calls_failed: u64,
    final_response: String,
    exploration_tools_before_first_mutation: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum ToolKind {
    Exploration,
    Mutation,
    Other,
}

/// Coarse classification for the exploration-before-first-mutation counter.
/// LSP navigation tools are exploration; `lsp_rename`/`lsp_code_actions`
/// write to the workspace, so they count as mutations.
fn classify_tool(name: &str) -> ToolKind {
    match name {
        "read_file" | "grep_files" | "repo_map" | "repo_symbols" | "ast_grep"
        | "list_directory" | "stat_file" | "lsp_definition" | "lsp_references" | "lsp_hover"
        | "lsp_diagnostics" | "lsp_symbols" => ToolKind::Exploration,
        "write_file" | "edit_file" | "delete_file" | "lsp_rename" | "lsp_code_actions" => {
            ToolKind::Mutation
        }
        _ => ToolKind::Other,
    }
}

fn parse_events(jsonl: &str) -> Mined {
    let mut m = Mined::default();
    let mut saw_mutation = false;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let etype = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let data = ev.get("data").cloned().unwrap_or(Value::Null);
        let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
        match etype {
            "output.message.completed" => {
                m.llm_calls += 1;
                if let Some(usage) = data.get("usage") {
                    m.input_tokens += num(usage, "input_tokens");
                    m.output_tokens += num(usage, "output_tokens");
                    m.cache_read_tokens += num(usage, "cache_read_tokens");
                    m.cache_creation_tokens += num(usage, "cache_creation_tokens");
                    m.cost_usd += usage
                        .get("actual_cost_usd")
                        .or_else(|| usage.get("estimated_cost_usd"))
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                }
                if let Some(message) = data.get("message") {
                    let text: String = message
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        m.final_response = text;
                    }
                }
            }
            "tool.completed" => {
                let name = data
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let failed = data.get("success") == Some(&Value::Bool(false));
                m.tool_calls.push(name.to_string());
                if failed {
                    m.tool_calls_failed += 1;
                }
                if name.starts_with("lsp_") {
                    m.lsp_tool_calls += 1;
                    if failed {
                        m.lsp_tool_calls_failed += 1;
                    }
                }
                match classify_tool(name) {
                    ToolKind::Mutation => saw_mutation = true,
                    ToolKind::Exploration => {
                        if !saw_mutation {
                            m.exploration_tools_before_first_mutation += 1;
                        }
                    }
                    ToolKind::Other => {}
                }
            }
            "reason.completed" => {
                m.iterations += 1;
                m.reason_ms += num(&data, "duration_ms");
            }
            "turn.completed" => {
                m.turns += 1;
                m.turn_ms += num(&data, "duration_ms");
            }
            _ => {}
        }
    }
    if m.turns == 0 {
        m.turns = m.iterations;
    }
    m
}

/// Read the workdir back for file scorers: UTF-8 files only, `.git` skipped,
/// large files skipped (nothing in these samples comes close).
fn read_files_back(root: &Path) -> BTreeMap<String, String> {
    const MAX_LEN: u64 = 512 * 1024;
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                stack.push(path);
            } else if entry
                .metadata()
                .map(|md| md.len() <= MAX_LEN)
                .unwrap_or(false)
                && let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.insert(rel.to_string_lossy().to_string(), text);
            }
        }
    }
    out
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Keep the raw session log for debugging: events.jsonl files land under the
/// study's gitignored `.cache/sessions/`, and the transcript metadata records
/// the path. Best-effort — a copy failure never fails the case.
fn preserve_session_log(events_path: &Path, slug: &str) -> Option<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/sessions");
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!("{slug}.events.jsonl"));
    std::fs::copy(events_path, &dest).ok()?;
    Some(format!(".cache/sessions/{slug}.events.jsonl"))
}

async fn run_yolop(sample: Sample, cx: RunCx) -> Transcript {
    let bin = match yolop_bin() {
        Ok(bin) => bin,
        // Not the model's fault: report as infra so the host shows N/A, not a failure.
        Err(e) => return Transcript::infra_error(e),
    };
    let harness = cx.param("harness").unwrap_or("default").to_string();
    let Some(settings) = settings_for_variant(&harness) else {
        return Transcript::infra_error(format!("unknown harness variant `{harness}`"));
    };
    // The lsp variant is only meaningful when the sample's language server is
    // actually installed — a missing binary would silently grade the baseline
    // twice. Infra, not a failure: bootstrap.sh installs the servers.
    if harness == "lsp"
        && let Some(server) = sample.metadata.get("lsp_server").and_then(Value::as_str)
        && !binary_on_path(server)
    {
        return Transcript::infra_error(format!(
            "language server `{server}` not on PATH — run this study's bootstrap.sh"
        ));
    }

    // Isolated per-case dirs: `work` is the workspace yolop edits (seeded from
    // the sample); `scratch` holds the XDG dirs + session log, *outside* the
    // workspace so file scorers never see study plumbing.
    let (Ok(work), Ok(scratch)) = (tempfile::tempdir(), tempfile::tempdir()) else {
        return Transcript::infra_error("failed to create case temp dirs");
    };
    for (rel, contents) in &sample.files {
        let path = work.path().join(rel);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, contents) {
            return Transcript::infra_error(format!("seed {rel}: {e}"));
        }
    }
    let home = scratch.path().join("home");
    let xdg_config = scratch.path().join("xdg-config");
    let xdg_settings_dir = xdg_config.join("yolop");
    let macos_settings_dir = home.join("Library/Application Support/yolop");
    if std::fs::create_dir_all(&xdg_settings_dir).is_err()
        || std::fs::write(xdg_settings_dir.join("settings.toml"), &settings).is_err()
        || std::fs::create_dir_all(&macos_settings_dir).is_err()
        || std::fs::write(macos_settings_dir.join("settings.toml"), &settings).is_err()
    {
        return Transcript::infra_error("failed to write case settings.toml");
    }
    let sessions = scratch.path().join("sessions");
    let session_id = fresh_session_id();

    let prompt = sample.input.join("\n");
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.current_dir(work.path())
        .arg("-C")
        .arg(work.path())
        .arg("--provider")
        .arg(&cx.target.provider);
    if !cx.target.model.is_empty() {
        cmd.arg("--model").arg(&cx.target.model);
    }
    cmd.arg("--session")
        .arg(&session_id)
        .arg("--session-dir")
        .arg(&sessions)
        .arg("-p")
        .arg(&prompt);
    // Full config isolation: Linux honors XDG_CONFIG_HOME; macOS `dirs` uses
    // HOME/Library/Application Support, so set HOME to the scratch tree too.
    cmd.env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", scratch.path().join("xdg-data"))
        .env("XDG_STATE_HOME", scratch.path().join("xdg-state"))
        .env("XDG_CACHE_HOME", scratch.path().join("xdg-cache"))
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true); // dropping the wait future on timeout kills yolop

    let started = std::time::Instant::now();
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Transcript::infra_error(format!("spawn {}: {e}", bin.display())),
    };
    let waited =
        tokio::time::timeout(Duration::from_secs(timeout_s()), child.wait_with_output()).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    let mut t = Transcript::default();
    let mut stop_reason = "completed";
    match &waited {
        Ok(Ok(output)) if output.status.success() => {}
        Ok(Ok(output)) => {
            stop_reason = "error";
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = stderr
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ");
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            t.error = Some(format!("yolop exit {code}: {tail}"));
        }
        Ok(Err(e)) => {
            stop_reason = "error";
            t.error = Some(format!("wait for yolop: {e}"));
        }
        Err(_) => {
            // A run that never finishes is a finding about the agent under
            // test, not the environment — score it as a failure, not infra.
            stop_reason = "timeout";
            t.error = Some(format!("timeout after {}s", timeout_s()));
        }
    }

    let events_path = sessions.join(&session_id).join("events.jsonl");
    let mined = std::fs::read_to_string(&events_path)
        .map(|text| parse_events(&text))
        .unwrap_or_default();
    if mined.llm_calls == 0 && t.error.is_none() {
        t.error = Some(format!("no events at {}", events_path.display()));
        stop_reason = "error";
    }

    t.final_response = mined.final_response;
    t.iterations = mined.iterations as usize;
    t.tool_calls_count = mined.tool_calls.len();
    t.tool_calls = mined.tool_calls;
    t.usage.input_tokens = mined.input_tokens;
    t.usage.output_tokens = mined.output_tokens;
    t.usage.cache_read_tokens = mined.cache_read_tokens;
    t.usage.cost_usd = mined.cost_usd;
    t.timing.duration_ms = duration_ms;
    t.files = read_files_back(work.path());

    t.metrics.insert("llm_calls".into(), mined.llm_calls as f64);
    t.metrics.insert("turns".into(), mined.turns as f64);
    t.metrics
        .insert("tool_calls_failed".into(), mined.tool_calls_failed as f64);
    t.metrics
        .insert("lsp_tool_calls".into(), mined.lsp_tool_calls as f64);
    t.metrics.insert(
        "lsp_tool_calls_failed".into(),
        mined.lsp_tool_calls_failed as f64,
    );
    t.metrics.insert(
        "cache_creation_tokens".into(),
        mined.cache_creation_tokens as f64,
    );
    t.metrics.insert(
        "exploration_tools_before_first_mutation".into(),
        mined.exploration_tools_before_first_mutation as f64,
    );
    let agent_ms = if mined.turn_ms > 0 {
        mined.turn_ms
    } else {
        mined.reason_ms
    };
    t.metrics
        .insert("agent_reported_ms".into(), agent_ms as f64);

    t.metadata
        .insert("provider".into(), json!(cx.target.provider));
    t.metadata.insert("model".into(), json!(cx.target.model));
    t.metadata.insert("harness".into(), json!(harness));
    t.metadata.insert("stop_reason".into(), json!(stop_reason));
    let slug = sanitize(&format!(
        "{}-{}-{}-{}",
        sample.id,
        cx.target.label,
        harness,
        &session_id["session_".len()..]
    ));
    if let Some(kept) = preserve_session_log(&events_path, &slug) {
        t.metadata.insert("session_log".into(), json!(kept));
    }
    t
}

// ============================================================================
// The eval
// ============================================================================

#[eval]
fn lsp_navigation() -> Eval {
    Eval::new("lsp_navigation")
        .describe(
            "Semantic-navigation traps through the yolop harness (`yolop -p`): \
             LSP capability off vs on, with adoption metrics",
        )
        .dataset(dataset())
        .subject(subject_fn(run_yolop))
        .targets(targets())
        .axis("harness", HARNESS_VARIANTS.iter().map(|v| v.name))
        .scorer(succeeded())
        .scorer(checks_scorer())
        .scorer(lsp_used_scorer())
        // Guardrails, not the comparison itself: the per-case numbers (turns,
        // tool calls, tokens, cost) surface in the report for A/B reading.
        .scorer(turns_within(32))
        .scorer(tool_calls_within(64))
        .scorer(cost_within(2.0))
        .max_turns(32)
        .meta("suite", "lsp-integration-v1")
        .build()
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    mira::Study::registered().serve().await
}

// ============================================================================
// Tests — `cargo test` in this directory
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_samples_are_well_formed() {
        let ds = dataset();
        let mut ids: Vec<&str> = ds.samples.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ds.samples.len(), "sample ids must be unique");
        for s in &ds.samples {
            let checks = s.metadata.get("checks").and_then(Value::as_array);
            assert!(
                checks.is_some_and(|c| !c.is_empty()),
                "sample {} must declare checks",
                s.id
            );
            assert!(!s.input.is_empty(), "sample {} must have a prompt", s.id);
            assert!(
                s.metadata
                    .get("lsp_server")
                    .and_then(Value::as_str)
                    .is_some_and(|b| !b.is_empty()),
                "sample {} must declare the language server the lsp variant needs",
                s.id
            );
        }
    }

    /// The rename trap's checks must be internally consistent: decoys the
    /// checks require to *survive* must exist in the seed exactly as asserted.
    #[test]
    fn rename_trap_decoys_are_seeded() {
        let sample = rename_with_decoys();
        let files: BTreeMap<&str, &str> = sample
            .files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert!(files["vendor_client.py"].contains("def fetch_records"));
        assert!(files["snapshots.py"].contains(".fetch_records()"));
        assert!(files["telemetry.py"].contains("\"fetch_records\""));
        assert!(files["cli.py"].contains("fetch_records as fr"));
    }

    #[test]
    fn harness_variants_render_settings() {
        let default = settings_for_variant("default").unwrap();
        assert!(default.contains("worktrees = \"off\""));
        assert!(!default.contains("capabilities"));

        let lsp = settings_for_variant("lsp").unwrap();
        assert!(lsp.contains("worktrees = \"off\""));
        assert!(lsp.contains("ref = \"lsp\""));
        assert!(!lsp.contains("enabled = false"));

        assert!(settings_for_variant("nope").is_none());
    }

    #[test]
    fn session_ids_satisfy_yolop_format() {
        let id = fresh_session_id();
        let hex = id.strip_prefix("session_").expect("session_ prefix");
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(fresh_session_id(), id);
    }

    #[test]
    fn binary_on_path_finds_a_shell() {
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn parse_events_counts_lsp_adoption() {
        let jsonl = r#"
{"type":"tool.completed","data":{"tool_name":"lsp_references","success":true}}
{"type":"tool.completed","data":{"tool_name":"grep_files","success":true}}
{"type":"tool.completed","data":{"tool_name":"lsp_rename","success":true}}
{"type":"tool.completed","data":{"tool_name":"lsp_diagnostics","success":false}}
{"type":"output.message.completed","data":{"message":{"role":"agent","content":[{"type":"text","text":"done"}]},"usage":{"input_tokens":10,"output_tokens":2,"estimated_cost_usd":0.01}}}
{"type":"reason.completed","data":{"success":true,"duration_ms":100}}
"#;
        let m = parse_events(jsonl);
        assert_eq!(m.lsp_tool_calls, 3);
        assert_eq!(m.lsp_tool_calls_failed, 1);
        assert_eq!(m.tool_calls_failed, 1);
        // lsp_references + grep_files explore before the lsp_rename mutation;
        // the post-mutation lsp_diagnostics doesn't count.
        assert_eq!(m.exploration_tools_before_first_mutation, 2);
        assert_eq!(m.final_response, "done");
    }

    #[tokio::test]
    async fn lsp_used_scorer_gates_on_variant() {
        let sample = Sample::new("s", "x");
        let mut t = Transcript {
            metadata: BTreeMap::from([("harness".to_string(), json!("lsp"))]),
            ..Default::default()
        };
        t.metrics.insert("lsp_tool_calls".into(), 0.0);
        let score = lsp_used_scorer().score(&sample, &t).await;
        assert!(!score.pass && !score.na, "{}", score.reason);

        t.metrics.insert("lsp_tool_calls".into(), 2.0);
        let score = lsp_used_scorer().score(&sample, &t).await;
        assert!(score.pass, "{}", score.reason);

        t.metadata.insert("harness".into(), json!("default"));
        let score = lsp_used_scorer().score(&sample, &t).await;
        assert!(score.na, "{}", score.reason);
    }

    #[tokio::test]
    async fn checks_scorer_grades_files_and_response() {
        let t = Transcript {
            final_response: "sdk/internal/money.py".into(),
            files: BTreeMap::from([(
                "fetcher.py".into(),
                "def load_records(path):\n    ...\n".into(),
            )]),
            ..Default::default()
        };
        let s = Sample::new("a", "x").meta(
            "checks",
            json!([
                {"file": "fetcher.py", "contains": ["def load_records"], "lacks": ["fetch_records"]},
                {"response_contains": ["sdk/internal/money.py"]}
            ]),
        );
        let score = checks_scorer().score(&s, &t).await;
        assert!(score.pass, "{}", score.reason);

        let failing = Sample::new("b", "x").meta(
            "checks",
            json!([{"file": "fetcher.py", "lacks": ["load_records"]}]),
        );
        let score = checks_scorer().score(&failing, &t).await;
        assert!(!score.pass && !score.na);
        assert!(score.reason.contains("still contains"));
    }

    #[test]
    fn matrix_shape() {
        let eval = lsp_navigation();
        assert_eq!(eval.targets.len(), 4);
        assert_eq!(eval.axis_combinations().len(), HARNESS_VARIANTS.len());
        // Every target is a key-gated cloud model; none is unconditionally on.
        assert!(eval.targets.iter().all(|t| !t.is_sim()));
    }

    /// Drives the subject end-to-end: spawns the real yolop binary against its
    /// bundled offline llmsim provider (no key, no cost — llmsim is not a
    /// matrix target, but it still proves the spawn → settings → events.jsonl →
    /// transcript pipeline, including booting with the lsp capability enabled).
    /// Skips when no yolop binary is built.
    #[tokio::test]
    async fn run_yolop_end_to_end_offline() {
        if yolop_bin().is_err() {
            eprintln!("skipping: no yolop binary (cargo build at the repo root first)");
            return;
        }
        for harness in ["default", "lsp"] {
            let mut cx = RunCx::new(Target::new("llmsim", "llmsim", "llmsim-yolop"));
            cx.params.insert("harness".into(), harness.into());
            let sample = Sample::new("e2e", "say hi")
                .file("note.txt", "seed\n")
                .meta("lsp_server", "sh"); // always present: gate must not trip
            let t = run_yolop(sample, cx).await;
            assert!(t.error.is_none(), "harness={harness}: {:?}", t.error);
            assert!(!t.final_response.is_empty(), "harness={harness}");
            assert!(t.usage.input_tokens > 0, "harness={harness}");
            assert_eq!(t.files.get("note.txt").map(String::as_str), Some("seed\n"));
            assert_eq!(t.metadata["stop_reason"], json!("completed"));
            assert_eq!(t.metadata["harness"], json!(harness));
            assert_eq!(t.metrics.get("lsp_tool_calls"), Some(&0.0));
        }
    }

    /// The availability gate: an lsp-variant case whose language server is
    /// missing must report infra (N/A), not a model failure.
    #[tokio::test]
    async fn missing_language_server_is_infra_not_failure() {
        if yolop_bin().is_err() {
            eprintln!("skipping: no yolop binary (cargo build at the repo root first)");
            return;
        }
        let mut cx = RunCx::new(Target::new("llmsim", "llmsim", "llmsim-yolop"));
        cx.params.insert("harness".into(), "lsp".into());
        let sample =
            Sample::new("gated", "hi").meta("lsp_server", "definitely-not-a-real-binary-xyz");
        let t = run_yolop(sample, cx).await;
        assert!(t.errored_infra(), "expected infra error, got {:?}", t.error);
        assert!(
            t.error
                .as_deref()
                .is_some_and(|e| e.contains("bootstrap.sh")),
            "error should point at bootstrap: {:?}",
            t.error
        );
    }
}

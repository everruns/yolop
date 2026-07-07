//! harness_basic — basic coding tasks driven through the **yolop harness**.
//!
//! A [Mira](https://github.com/everruns/mira) eval study, written against the
//! `mira-eval` Rust SDK (no Python plumbing). The subject spawns yolop's
//! headless one-shot mode (`yolop -p`, the runtime path — no TUI) in a fresh
//! seeded workdir per case, and mines the session `events.jsonl` for metrics.
//!
//! The matrix is samples × three axes:
//!   * **target** — provider models (Anthropic, OpenAI, OpenRouter)
//!   * **effort** — `--reasoning-effort` (`default` = yolop's per-model default)
//!   * **harness** — yolop configuration variants (out-of-the-box, ast-grep off, …),
//!     applied as a per-case `settings.toml` in an isolated `XDG_CONFIG_HOME`
//!
//! This study exists to A/B yolop *features*: add a harness variant to
//! `HARNESS_VARIANTS` and compare pass rate / turns / tool calls / cost across
//! otherwise-identical cases. Run it with the `mira` host CLI from this
//! directory (`mira list`, `mira run --preset smoke`); see README.md.

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

/// `--reasoning-effort` values. `default` omits the flag so yolop applies the
/// model profile's own default.
const EFFORTS: &[&str] = &["default", "low", "high"];

/// One yolop configuration under test. `settings` is TOML appended to the
/// study's base `settings.toml` (see [`settings_for_variant`]); `default` is
/// the out-of-the-box harness. Add a variant here (and nothing else) to put a
/// feature configuration into the matrix.
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
        name: "no-progress-guard",
        settings: "[[capabilities]]\nref = \"progress_guard\"\nenabled = false\n",
    },
    HarnessVariant {
        name: "no-ast-grep",
        settings: "[[capabilities]]\nref = \"ast_grep\"\nenabled = false\n",
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
// Dataset — small, self-contained coding tasks with declarative checks
// ============================================================================

/// Every sample carries its own assertions as `checks` metadata (see
/// [`checks_scorer`]), so adding a case is data-only:
///
/// ```json
/// {"file": "src/lib.rs", "contains": ["fn greet"], "lacks": ["TODO"]}
/// {"response_contains": ["7321"]}
/// ```
fn progress_guard_probe_sample() -> Sample {
    let mut sample = Sample::new(
        "progress-guard-sequential-read",
        "This is a stepwise file-reading evaluation. Read notes/01.txt through \
         notes/24.txt in numeric order, one file at a time, using the file-read \
         tool for each file. Do not use grep, bash, directory listing, or search \
         shortcuts. Do not stop before notes/24.txt. Reply with only the \
         FINAL_CODE value from notes/24.txt.",
    )
    .tag("progress-guard")
    .meta("kind", "guardrail")
    .meta("checks", json!([{"response_contains": ["KITE-7429"]}]));

    for i in 1..24 {
        sample = sample.file(
            format!("notes/{i:02}.txt"),
            format!("note {i:02}: keep reading; FINAL_CODE is not in this file.\n"),
        );
    }
    sample.file("notes/24.txt", "note 24: FINAL_CODE=KITE-7429\n")
}

fn dataset() -> Dataset {
    let cargo_toml = "[package]\nname = \"seed\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    Dataset::new(vec![
        Sample::new(
            "add-fn",
            "In src/lib.rs, add a public function `greet` that returns the string \
             \"hello, yolop\" as `&'static str`. Keep the existing contents. \
             Make the edit directly; do not ask questions.",
        )
        .file("Cargo.toml", cargo_toml)
        .file("src/lib.rs", "// Library entry point.\n")
        .tag("smoke")
        .meta("kind", "edit")
        .meta(
            "checks",
            json!([{"file": "src/lib.rs", "contains": ["fn greet", "hello, yolop"]}]),
        ),
        Sample::new(
            "fix-off-by-one",
            "`sum` in src/lib.rs skips the last element. Fix it so it sums every \
             element. Make the edit directly; do not ask questions.",
        )
        .file("Cargo.toml", cargo_toml)
        .file(
            "src/lib.rs",
            "/// Sums all elements of `xs`.\n\
             pub fn sum(xs: &[i32]) -> i32 {\n    \
                 xs.iter().take(xs.len().saturating_sub(1)).sum()\n\
             }\n",
        )
        .meta("kind", "edit")
        .meta(
            "checks",
            json!([{"file": "src/lib.rs", "contains": ["fn sum"], "lacks": ["take("]}]),
        ),
        Sample::new(
            "rename-across-files",
            "Rename the function `fetch_records` to `load_records` across the whole \
             project — its definition, all imports, and all call sites. Behavior must \
             not change. Make the edits directly; do not ask questions.",
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
        .meta("kind", "refactor")
        .meta(
            "checks",
            json!([
                {"file": "fetcher.py", "contains": ["def load_records"], "lacks": ["fetch_records"]},
                {"file": "app.py", "contains": ["load_records"], "lacks": ["fetch_records"]},
                {"file": "report.py", "contains": ["load_records"], "lacks": ["fetch_records"]}
            ]),
        ),
        Sample::new(
            "find-constant",
            "What is the numeric value of MAGIC_TIMEOUT_MS in this project? \
             Reply with just the number.",
        )
        .file("main.py", "from settings.defaults import MAGIC_TIMEOUT_MS\n\nprint(MAGIC_TIMEOUT_MS)\n")
        .file(
            "settings/defaults.py",
            "RETRY_LIMIT = 4\nMAGIC_TIMEOUT_MS = 7321\nBATCH_SIZE = 128\n",
        )
        .file("settings/net.py", "KEEPALIVE_S = 45\nPOOL_SIZE = 8\n")
        .tag("smoke")
        .meta("kind", "search")
        .meta("checks", json!([{"response_contains": ["7321"]}])),
        Sample::new(
            "implement-todo",
            "Implement `clamp` in utils.js as described by the TODO comment, and \
             remove the TODO comment once done. Make the edit directly; do not ask \
             questions.",
        )
        .file(
            "utils.js",
            "// TODO: implement clamp(value, min, max): return value bounded to [min, max].\n\
             function clamp(value, min, max) {\n  throw new Error(\"not implemented\");\n}\n\n\
             module.exports = { clamp };\n",
        )
        .meta("kind", "edit")
        .meta(
            "checks",
            json!([{
                "file": "utils.js",
                "contains": ["function clamp", "module.exports"],
                "lacks": ["TODO", "not implemented"]
            }]),
        ),
        Sample::new(
            "add-module",
            "Create a new module `util` (src/util.rs) containing \
             `pub fn double(x: i32) -> i32` that returns `2 * x`, and declare \
             `pub mod util;` in src/lib.rs. Make the edits directly; do not ask \
             questions.",
        )
        .file("Cargo.toml", cargo_toml)
        .file("src/lib.rs", "pub fn existing() -> i32 {\n    1\n}\n")
        .meta("kind", "edit")
        .meta(
            "checks",
            json!([
                {"file": "src/util.rs", "contains": ["pub fn double"]},
                {"file": "src/lib.rs", "contains": ["pub mod util"]}
            ]),
        ),
        progress_guard_probe_sample(),
    ])
}

// ============================================================================
// Scoring
// ============================================================================

/// Grades each sample against its own `checks` metadata (file contents captured
/// from the workdir after the run, and/or the final response). N/A — not a
/// failure — on samples that declare no checks.
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

// ============================================================================
// Subject — spawn `yolop -p` and mine its events.jsonl
// ============================================================================

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The yolop binary under test: `HARNESS_BASIC_YOLOP_BIN` override, else the
/// repo's release build, else the debug build.
fn yolop_bin() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("HARNESS_BASIC_YOLOP_BIN")
        && !explicit.trim().is_empty()
    {
        let p = PathBuf::from(explicit);
        return if p.exists() {
            Ok(p)
        } else {
            Err(format!(
                "HARNESS_BASIC_YOLOP_BIN not found: {}",
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
         or set HARNESS_BASIC_YOLOP_BIN"
            .to_string(),
    )
}

fn timeout_s() -> u64 {
    std::env::var("HARNESS_BASIC_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
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

/// What one events.jsonl yields. Mirrors the swebench_verified study's
/// `extract_yolop`: usage/cost only from `output.message.completed` (yolop
/// repeats the same usage block on `reason.completed`, so a naive sum
/// double-counts), tool calls from `tool.completed`, iterations from
/// `reason.completed`, turns from `turn.completed`.
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
    final_response: String,
    effort_applied: Option<String>,
    exploration_tools_before_first_mutation: u64,
    max_exploration_tools_without_progress: u64,
    progress_guard_warnings: u64,
}

fn normalized_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tool_result_value(data: &Value) -> Option<Value> {
    let result = data.get("result")?;
    if let Some(items) = result.as_array() {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = item.get("text").and_then(Value::as_str) else {
                continue;
            };
            return Some(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())));
        }
    }
    Some(result.clone())
}

fn tool_command(data: &Value) -> String {
    tool_result_value(data)
        .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn has_progress_guard_warning(data: &Value) -> bool {
    match tool_result_value(data) {
        Some(Value::Object(obj)) => obj
            .get("progress_guard_warning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        Some(Value::String(text)) => {
            text.contains("progress_guard_warning") || text.starts_with("progress_guard:")
        }
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ToolKind {
    Exploration,
    Mutation,
    Validation,
    Other,
}

fn is_status_command(command: &str) -> bool {
    matches!(
        command,
        "git status" | "git status --short" | "git status --short --branch" | "git diff"
    ) || command.starts_with("git status ")
        || command.starts_with("git diff ")
}

fn classify_tool(data: &Value) -> ToolKind {
    let name = data
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match name {
        "read_file" | "grep_files" | "repo_map" | "ast_grep" | "list_directory" | "stat_file" => {
            return ToolKind::Exploration;
        }
        "write_file" | "edit_file" | "delete_file" | "edit" => return ToolKind::Mutation,
        "bash" => {}
        _ => return ToolKind::Other,
    }

    let command = normalized_command(&tool_command(data));
    if command.is_empty() {
        return ToolKind::Other;
    }
    if is_status_command(&command)
        || [
            "rg",
            "grep",
            "find",
            "sed",
            "cat",
            "ls",
            "git show",
            "git log",
            "git blame",
            "git grep",
            "git ls-files",
        ]
        .iter()
        .any(|prefix| command.starts_with(prefix))
    {
        return ToolKind::Exploration;
    }
    if [
        "cargo test",
        "cargo clippy",
        "cargo fmt --check",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "pytest",
        "uv run",
        "go test",
        "python -m unittest",
    ]
    .iter()
    .any(|prefix| command.starts_with(prefix))
    {
        return ToolKind::Validation;
    }
    if [
        "apply_patch",
        "cargo fmt",
        "npm run format",
        "pnpm run format",
        "git apply",
        "git commit",
        "git add",
        "mv ",
        "cp ",
        "rm ",
        "mkdir ",
    ]
    .iter()
    .any(|part| command.contains(part))
    {
        return ToolKind::Mutation;
    }
    ToolKind::Other
}

fn parse_events(jsonl: &str) -> Mined {
    let mut m = Mined::default();
    let mut current_exploration_without_progress = 0_u64;
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
                    // Prefer the provider-reported cost; fall back to yolop's
                    // price-table estimate.
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
                    if let Some(effort) = message
                        .pointer("/metadata/reasoning_effort")
                        .and_then(Value::as_str)
                    {
                        m.effort_applied = Some(effort.to_string());
                    }
                }
            }
            "tool.completed" => {
                let name = data
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                m.tool_calls.push(name.to_string());
                if data.get("success") == Some(&Value::Bool(false)) {
                    m.tool_calls_failed += 1;
                }
                if has_progress_guard_warning(&data) {
                    m.progress_guard_warnings += 1;
                }
                match classify_tool(&data) {
                    ToolKind::Mutation => {
                        saw_mutation = true;
                        current_exploration_without_progress = 0;
                    }
                    ToolKind::Validation => {
                        current_exploration_without_progress = 0;
                    }
                    ToolKind::Exploration => {
                        current_exploration_without_progress += 1;
                        m.max_exploration_tools_without_progress = m
                            .max_exploration_tools_without_progress
                            .max(current_exploration_without_progress);
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
    let effort = cx.param("effort").unwrap_or("default").to_string();
    let Some(settings) = settings_for_variant(&harness) else {
        return Transcript::infra_error(format!("unknown harness variant `{harness}`"));
    };

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
    // `default` leaves yolop's per-model default effort in place.
    if effort != "default" {
        cmd.arg("--reasoning-effort").arg(&effort);
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
    t.metrics.insert(
        "cache_creation_tokens".into(),
        mined.cache_creation_tokens as f64,
    );
    t.metrics.insert(
        "exploration_tools_before_first_mutation".into(),
        mined.exploration_tools_before_first_mutation as f64,
    );
    t.metrics.insert(
        "max_exploration_tools_without_progress".into(),
        mined.max_exploration_tools_without_progress as f64,
    );
    t.metrics.insert(
        "progress_guard_warnings".into(),
        mined.progress_guard_warnings as f64,
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
    t.metadata.insert("effort".into(), json!(effort));
    if let Some(applied) = mined.effort_applied {
        t.metadata
            .insert("reasoning_effort_applied".into(), json!(applied));
    }
    t.metadata.insert("harness".into(), json!(harness));
    t.metadata.insert("stop_reason".into(), json!(stop_reason));
    let slug = sanitize(&format!(
        "{}-{}-{}-{}-{}",
        sample.id,
        cx.target.label,
        harness,
        effort,
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
fn basic_coding() -> Eval {
    Eval::new("basic_coding")
        .describe(
            "Basic coding tasks through the yolop harness (`yolop -p`): \
             models × reasoning effort × harness configuration",
        )
        .dataset(dataset())
        .subject(subject_fn(run_yolop))
        .targets(targets())
        .axis("harness", HARNESS_VARIANTS.iter().map(|v| v.name))
        .axis("effort", EFFORTS.iter().copied())
        .scorer(succeeded())
        .scorer(checks_scorer())
        // Guardrails, not the comparison itself: the per-case numbers (turns,
        // tool calls, tokens, cost) surface in the report for A/B reading.
        .scorer(turns_within(32))
        .scorer(tool_calls_within(64))
        .scorer(cost_within(2.0))
        .max_turns(32)
        .meta("suite", "harness-basic-v1")
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
        }
    }

    #[test]
    fn harness_variants_render_settings() {
        let default = settings_for_variant("default").unwrap();
        assert!(default.contains("worktrees = \"off\""));
        assert!(!default.contains("capabilities"));

        let no_ast = settings_for_variant("no-ast-grep").unwrap();
        assert!(no_ast.contains("worktrees = \"off\""));
        assert!(no_ast.contains("ref = \"ast_grep\""));
        assert!(no_ast.contains("enabled = false"));

        let no_progress = settings_for_variant("no-progress-guard").unwrap();
        assert!(no_progress.contains("worktrees = \"off\""));
        assert!(no_progress.contains("ref = \"progress_guard\""));
        assert!(no_progress.contains("enabled = false"));

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
    fn parse_events_mines_yolop_stream() {
        // Shape taken from a real `yolop -p` events.jsonl: usage appears on both
        // output.message.completed and reason.completed — it must be counted once.
        let jsonl = r#"
{"type":"input.message","data":{"message":{"role":"user","content":[{"type":"text","text":"do it"}]}}}
{"type":"tool.completed","data":{"tool_name":"grep_files","success":true}}
{"type":"tool.completed","data":{"tool_name":"bash","success":true,"result":[{"type":"text","text":"{\"command\":\"git status --short\",\"progress_guard_warning\":\"progress_guard: repeated status\"}"}]}}
{"type":"tool.completed","data":{"tool_name":"bash","success":true,"result":[{"type":"text","text":"{\"command\":\"cargo test --all-features\"}"}]}}
{"type":"tool.completed","data":{"tool_name":"edit_file","success":false}}
{"type":"output.message.completed","data":{"message":{"role":"agent","content":[{"type":"text","text":"Done."}],"metadata":{"reasoning_effort":"high"}},"usage":{"input_tokens":100,"output_tokens":10,"cache_read_tokens":40,"cache_creation_tokens":5,"estimated_cost_usd":0.02}}}
{"type":"reason.completed","data":{"success":true,"duration_ms":900,"usage":{"input_tokens":100,"output_tokens":10,"estimated_cost_usd":0.02}}}
{"type":"output.message.completed","data":{"message":{"role":"agent","content":[{"type":"text","text":"All finished."}]},"usage":{"input_tokens":200,"output_tokens":20,"actual_cost_usd":0.05,"estimated_cost_usd":0.99}}}
{"type":"reason.completed","data":{"success":true,"duration_ms":600}}
{"type":"turn.completed","data":{"duration_ms":1500}}
"#;
        let m = parse_events(jsonl);
        assert_eq!(m.llm_calls, 2);
        assert_eq!(m.input_tokens, 300);
        assert_eq!(m.output_tokens, 30);
        assert_eq!(m.cache_read_tokens, 40);
        assert_eq!(m.cache_creation_tokens, 5);
        assert!(
            (m.cost_usd - 0.07).abs() < 1e-9,
            "actual preferred: {}",
            m.cost_usd
        );
        assert_eq!(m.iterations, 2);
        assert_eq!(m.turns, 1);
        assert_eq!(m.turn_ms, 1500);
        assert_eq!(
            m.tool_calls,
            vec!["grep_files", "bash", "bash", "edit_file"]
        );
        assert_eq!(m.tool_calls_failed, 1);
        assert_eq!(m.final_response, "All finished.");
        assert_eq!(m.effort_applied.as_deref(), Some("high"));
        assert_eq!(m.exploration_tools_before_first_mutation, 2);
        assert_eq!(m.max_exploration_tools_without_progress, 2);
        assert_eq!(m.progress_guard_warnings, 1);
    }

    #[test]
    fn parse_events_turns_fall_back_to_iterations() {
        let jsonl = r#"{"type":"reason.completed","data":{"duration_ms":10}}"#;
        let m = parse_events(jsonl);
        assert_eq!(m.iterations, 1);
        assert_eq!(m.turns, 1);
    }

    fn graded_transcript() -> Transcript {
        Transcript {
            final_response: "the value is 7321".into(),
            files: BTreeMap::from([(
                "src/lib.rs".into(),
                "pub fn greet() -> &'static str { \"hello, yolop\" }\n".into(),
            )]),
            metadata: BTreeMap::from([("provider".to_string(), json!("anthropic"))]),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn checks_scorer_grades_files_and_response() {
        let s = Sample::new("a", "x").meta(
            "checks",
            json!([
                {"file": "src/lib.rs", "contains": ["fn greet"], "lacks": ["TODO"]},
                {"response_contains": ["7321"]}
            ]),
        );
        let score = checks_scorer().score(&s, &graded_transcript()).await;
        assert!(score.pass, "{}", score.reason);

        let failing = Sample::new("b", "x").meta(
            "checks",
            json!([{"file": "src/lib.rs", "contains": ["fn missing"]}]),
        );
        let score = checks_scorer().score(&failing, &graded_transcript()).await;
        assert!(!score.pass && !score.na);
        assert!(score.reason.contains("fn missing"));

        let missing_file =
            Sample::new("c", "x").meta("checks", json!([{"file": "nope.rs", "contains": ["x"]}]));
        let score = checks_scorer()
            .score(&missing_file, &graded_transcript())
            .await;
        assert!(!score.pass);
        assert!(score.reason.contains("no such file"));
    }

    #[tokio::test]
    async fn checks_scorer_is_na_without_checks() {
        let unchecked = Sample::new("b", "x");
        assert!(
            checks_scorer()
                .score(&unchecked, &graded_transcript())
                .await
                .na
        );
    }

    #[test]
    fn matrix_shape() {
        let eval = basic_coding();
        assert_eq!(eval.targets.len(), 4);
        // harness × effort axis cross-product
        assert_eq!(
            eval.axis_combinations().len(),
            HARNESS_VARIANTS.len() * EFFORTS.len()
        );
        // Every target is a key-gated cloud model; none is unconditionally on.
        assert!(eval.targets.iter().all(|t| !t.is_sim()));
    }

    /// Drives the subject end-to-end: spawns the real yolop binary against its
    /// bundled offline llmsim provider (no key, no cost — llmsim is not a
    /// matrix target, but it still proves the spawn → settings → events.jsonl →
    /// transcript pipeline). Skips when no yolop binary is built.
    #[tokio::test]
    async fn run_yolop_end_to_end_offline() {
        if yolop_bin().is_err() {
            eprintln!("skipping: no yolop binary (cargo build at the repo root first)");
            return;
        }
        for harness in ["default", "no-progress-guard", "no-ast-grep"] {
            let mut cx = RunCx::new(Target::new("llmsim", "llmsim", "llmsim-yolop"));
            cx.params.insert("harness".into(), harness.into());
            let sample = Sample::new("e2e", "say hi").file("note.txt", "seed\n");
            let t = run_yolop(sample, cx).await;
            assert!(t.error.is_none(), "harness={harness}: {:?}", t.error);
            assert!(!t.final_response.is_empty(), "harness={harness}");
            assert!(t.usage.input_tokens > 0, "harness={harness}");
            assert!(t.iterations >= 1, "harness={harness}");
            assert_eq!(t.files.get("note.txt").map(String::as_str), Some("seed\n"));
            assert_eq!(t.metadata["stop_reason"], json!("completed"));
            assert_eq!(t.metadata["harness"], json!(harness));
        }
    }
}

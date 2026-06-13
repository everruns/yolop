// The `memory` capability — yolop's global, durable memory.
//
// Extracted from the `your` capability so personalization (hooks, framing) and
// durable cross-session memory evolve independently. Memory is now *structured*:
// a `MEMORY.md` where each `## ` section is one memory with a title (its
// description), an id, created/updated timestamps, and a body (the memory text).
//
// Progressive disclosure: only memory TITLES are injected every turn (newest
// first, capped). The model reads a memory's full body on demand with `recall`,
// adds/updates with `remember`, and deletes with `forget`. This keeps the prompt
// small no matter how much the user remembers — the opposite of the old model,
// which injected the whole file every turn.
//
// See specs/memory.md for the design and configuration knobs.

use crate::settings::SettingsStore;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const MEMORY_CAPABILITY_ID: &str = "memory";

/// Structured memory file name, co-located with `settings.toml` in the yolop
/// config dir.
pub(crate) const MEMORY_FILE_NAME: &str = "MEMORY.md";

const MEMORY_HEADER: &str = "# yolop memory\n\n\
<!-- Global, durable user memory. One `## ` section per memory: the heading is its\n\
title, the HTML comment carries its id and timestamps, and the body is the memory\n\
text. Managed by the `remember` / `recall` / `forget` tools — edit through those,\n\
not by hand. -->\n";

// ---------- configuration ----------

/// Tunables for the memory capability, supplied through the generic
/// capability-config system (`AgentCapabilityConfig.config`) and described by
/// [`MemoryConfig::schema`]. All keys are optional; defaults apply otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryConfig {
    /// How many memory titles to inject into the system prompt each turn.
    pub disclosed_titles: usize,
    /// Default number of memories `recall` returns for a search/recent query.
    pub recall_limit: usize,
    /// Once memory holds more than this, `remember`/`recall` nudge the model to
    /// prune. A soft limit only — yolop never silently drops a user's memory.
    pub soft_cap: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            disclosed_titles: 15,
            recall_limit: 5,
            soft_cap: 200,
        }
    }
}

impl MemoryConfig {
    /// Parse config from a capability config `Value` (from
    /// `AgentCapabilityConfig.config`, the generic capability-config system).
    /// `Null` / missing keys fall back to defaults. Strict on types so the same
    /// parse backs `validate_config`: a present key must be a non-negative
    /// integer or it is rejected.
    pub(crate) fn from_value(config: &Value) -> Result<Self, String> {
        let mut cfg = Self::default();
        if config.is_null() {
            return Ok(cfg);
        }
        let obj = config
            .as_object()
            .ok_or_else(|| "memory config must be an object".to_string())?;
        let read = |key: &str, slot: &mut usize| -> Result<(), String> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(()),
                Some(v) => {
                    let n = v
                        .as_u64()
                        .ok_or_else(|| format!("`{key}` must be a non-negative integer"))?;
                    *slot = n as usize;
                    Ok(())
                }
            }
        };
        read("disclosed_titles", &mut cfg.disclosed_titles)?;
        read("recall_limit", &mut cfg.recall_limit)?;
        read("soft_cap", &mut cfg.soft_cap)?;
        Ok(cfg)
    }

    /// JSON Schema for the generic capability-config editor.
    fn schema() -> Value {
        let default = Self::default();
        json!({
            "type": "object",
            "properties": {
                "disclosed_titles": {
                    "type": "integer",
                    "minimum": 0,
                    "title": "Disclosed titles",
                    "description": "How many memory titles to inject into the system prompt each turn (newest first).",
                    "default": default.disclosed_titles,
                },
                "recall_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "title": "Recall limit",
                    "description": "Default number of memories `recall` returns for a search or recent query.",
                    "default": default.recall_limit,
                },
                "soft_cap": {
                    "type": "integer",
                    "minimum": 0,
                    "title": "Soft cap",
                    "description": "Warn (never delete) once more than this many memories exist. 0 disables the warning.",
                    "default": default.soft_cap,
                }
            },
            "additionalProperties": false
        })
    }
}

// ---------- data model ----------

/// One durable memory: a titled, timestamped note. `title` doubles as the
/// memory's human description and is what search boosts and disclosure shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Memory {
    pub id: String,
    pub title: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub body: String,
}

impl Memory {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "created": self.created.to_rfc3339(),
            "updated": self.updated.to_rfc3339(),
            "memory": self.body,
        })
    }

    /// Render one `## ` section for the on-disk file.
    fn render(&self) -> String {
        format!(
            "## {title}\n<!-- id: {id} · created: {created} · updated: {updated} -->\n\n{body}\n",
            title = self.title.trim(),
            id = self.id,
            created = self.created.to_rfc3339(),
            updated = self.updated.to_rfc3339(),
            body = self.body.trim(),
        )
    }
}

/// Outcome of a `remember` call: the resulting memory, whether it was newly
/// created (vs. an update), and the new total count.
pub(crate) struct RememberOutcome {
    pub memory: Memory,
    pub created: bool,
    pub total: usize,
}

/// Result of a `recall` search: the capped slice plus the total number of
/// matches, so callers can tell the model "showing N of T — narrow your query".
pub(crate) struct SearchResult {
    pub matches: Vec<Memory>,
    pub total_matched: usize,
}

// ---------- store ----------

/// Thread-safe handle to the structured `MEMORY.md`. The in-memory `Vec` is the
/// source of truth (kept newest-first by `updated`); mutations flush to disk via
/// an atomic temp-file + rename so readers and crashes never see a half-written
/// file. Mirrors `SettingsStore`.
pub(crate) struct MemoryStore {
    path: PathBuf,
    inner: Mutex<Vec<Memory>>,
}

impl MemoryStore {
    pub(crate) fn open(path: PathBuf) -> Self {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let mut memories = parse(&content);
        sort_newest_first(&mut memories);
        Self {
            path,
            inner: Mutex::new(memories),
        }
    }

    /// Derive the memory path from the settings file's directory so memory
    /// lives next to `settings.toml` and tests pointing settings at a tempdir
    /// get an isolated memory file for free.
    pub(crate) fn beside_settings(settings: &SettingsStore) -> Self {
        let path = settings
            .path()
            .parent()
            .map(|p| p.join(MEMORY_FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(MEMORY_FILE_NAME));
        Self::open(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.lock().expect("memory lock poisoned").len()
    }

    /// Add a new memory, or update an existing one. A memory is updated when an
    /// `id` is given and matches, or (when no id is given) when an existing
    /// title matches case-insensitively — so re-remembering the same thing
    /// edits in place instead of duplicating. Always stamps `updated = now`.
    pub(crate) fn remember(
        &self,
        title: &str,
        body: &str,
        id: Option<&str>,
    ) -> Result<RememberOutcome> {
        let now = Utc::now();
        let title = title.trim();
        let mut guard = self.inner.lock().expect("memory lock poisoned");

        let existing = match id {
            Some(id) => guard.iter().position(|m| m.id == id),
            None => guard
                .iter()
                .position(|m| m.title.eq_ignore_ascii_case(title)),
        };

        let (memory, created) = match existing {
            Some(idx) => {
                let m = &mut guard[idx];
                m.title = title.to_string();
                m.body = body.trim().to_string();
                m.updated = now;
                (m.clone(), false)
            }
            None => {
                let new_id = id
                    .map(str::to_string)
                    .unwrap_or_else(|| generate_id(&guard, title, now));
                let m = Memory {
                    id: new_id,
                    title: title.to_string(),
                    created: now,
                    updated: now,
                    body: body.trim().to_string(),
                };
                guard.push(m.clone());
                (m, true)
            }
        };

        sort_newest_first(&mut guard);
        let total = guard.len();
        save_to(&self.path, &guard)?;
        Ok(RememberOutcome {
            memory,
            created,
            total,
        })
    }

    /// Remove a memory by id, or by exact (case-insensitive) title. Returns the
    /// removed memory, or `None` if nothing matched.
    pub(crate) fn forget(&self, key: &str) -> Result<Option<Memory>> {
        let key = key.trim();
        let mut guard = self.inner.lock().expect("memory lock poisoned");
        let idx = guard
            .iter()
            .position(|m| m.id == key)
            .or_else(|| guard.iter().position(|m| m.title.eq_ignore_ascii_case(key)));
        let removed = idx.map(|i| guard.remove(i));
        if removed.is_some() {
            save_to(&self.path, &guard)?;
        }
        Ok(removed)
    }

    /// Fetch a single memory by id.
    pub(crate) fn get(&self, id: &str) -> Option<Memory> {
        let guard = self.inner.lock().expect("memory lock poisoned");
        guard.iter().find(|m| m.id == id).cloned()
    }

    /// The `limit` most recently updated memories, plus the total count.
    pub(crate) fn recent(&self, limit: usize) -> SearchResult {
        let guard = self.inner.lock().expect("memory lock poisoned");
        SearchResult {
            matches: guard.iter().take(limit).cloned().collect(),
            total_matched: guard.len(),
        }
    }

    /// Rank memories against `query`, returning the top `limit` plus the total
    /// number that matched. Title hits are weighted more than body hits, and
    /// ties break toward the most recently updated memory ("prefer latest").
    pub(crate) fn search(&self, query: &str, limit: usize) -> SearchResult {
        let tokens = tokenize(query);
        if tokens.is_empty() {
            return self.recent(limit);
        }
        let guard = self.inner.lock().expect("memory lock poisoned");
        let mut scored: Vec<(i64, &Memory)> = guard
            .iter()
            .filter_map(|m| {
                let score = relevance(m, &tokens);
                (score > 0).then_some((score, m))
            })
            .collect();
        // Primary: score desc. Secondary: updated desc (guard is already sorted
        // newest-first, so a stable sort by score keeps recency as the tiebreak).
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let total_matched = scored.len();
        let matches = scored
            .into_iter()
            .take(limit)
            .map(|(_, m)| m.clone())
            .collect();
        SearchResult {
            matches,
            total_matched,
        }
    }

    /// Titles of the `limit` most recent memories, plus the total count, for
    /// progressive disclosure in the system prompt.
    pub(crate) fn titles(&self, limit: usize) -> (Vec<(String, String, DateTime<Utc>)>, usize) {
        let guard = self.inner.lock().expect("memory lock poisoned");
        let titles = guard
            .iter()
            .take(limit)
            .map(|m| (m.id.clone(), m.title.clone(), m.updated))
            .collect();
        (titles, guard.len())
    }
}

// ---------- parsing / serialization ----------

fn sort_newest_first(memories: &mut [Memory]) {
    memories.sort_by_key(|m| std::cmp::Reverse(m.updated));
}

/// Split case-insensitive query into deduped non-empty word tokens.
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        let t = raw.trim().to_lowercase();
        if !t.is_empty() && !tokens.contains(&t) {
            tokens.push(t);
        }
    }
    tokens
}

/// Relevance of one memory to the query tokens. Each token present in the title
/// scores 3, in the body scores 1; summed across tokens.
fn relevance(memory: &Memory, tokens: &[String]) -> i64 {
    let title = memory.title.to_lowercase();
    let body = memory.body.to_lowercase();
    tokens
        .iter()
        .map(|t| {
            let mut s = 0;
            if title.contains(t.as_str()) {
                s += 3;
            }
            if body.contains(t.as_str()) {
                s += 1;
            }
            s
        })
        .sum()
}

/// Generate a short, unique-within-store id like `m-1a2b3c`.
fn generate_id(existing: &[Memory], seed: &str, now: DateTime<Utc>) -> String {
    use std::hash::{Hash, Hasher};
    let mut counter: u64 = 0;
    loop {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        seed.hash(&mut hasher);
        now.timestamp_nanos_opt().unwrap_or(0).hash(&mut hasher);
        counter.hash(&mut hasher);
        let id = format!("m-{:06x}", hasher.finish() & 0xFFFFFF);
        if !existing.iter().any(|m| m.id == id) {
            return id;
        }
        counter += 1;
    }
}

/// Parse the structured `MEMORY.md` into memories. Robust by design:
/// - Section boundaries are `## ` headings *anchored* by the `<!-- id: … -->`
///   metadata line we always write directly beneath them. This keeps round-trips
///   lossless even when a memory body itself contains a `## ` markdown heading —
///   such a body line is not metadata-anchored, so it stays in the body.
/// - If no heading is anchored (a hand-authored file), every `## ` is treated as
///   a boundary, and a missing metadata comment gets a generated id / `now`.
/// - A legacy flat-bullet file (no `## ` sections) is imported wholesale as a
///   single "Imported notes" memory so an upgrade never loses a user's notes.
fn parse(content: &str) -> Vec<Memory> {
    let lines: Vec<&str> = content.lines().collect();
    let is_heading = |l: &str| l.starts_with("## ");
    let is_meta = |l: &str| {
        let t = l.trim();
        t.starts_with("<!--") && t.contains("id:")
    };
    // Prefer metadata-anchored headings so a `## ` line inside a body never
    // splits a memory. Fall back to every heading only when nothing is anchored.
    let anchored: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| is_heading(l) && lines.get(i + 1).map(|n| is_meta(n)).unwrap_or(false))
        .map(|(i, _)| i)
        .collect();
    let heads: Vec<usize> = if anchored.is_empty() {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| is_heading(l))
            .map(|(i, _)| i)
            .collect()
    } else {
        anchored
    };

    if heads.is_empty() {
        return import_legacy(&lines);
    }

    let mut memories = Vec::new();
    for (n, &start) in heads.iter().enumerate() {
        let end = heads.get(n + 1).copied().unwrap_or(lines.len());
        let title = lines[start].trim_start_matches("## ").trim().to_string();
        let mut id = None;
        let mut created = None;
        let mut updated = None;
        let mut body_lines: Vec<&str> = Vec::new();
        for &line in &lines[start + 1..end] {
            let trimmed = line.trim();
            if id.is_none() && trimmed.starts_with("<!--") && trimmed.ends_with("-->") {
                let (i, c, u) = parse_meta(trimmed);
                id = i;
                created = c;
                updated = u;
                continue;
            }
            body_lines.push(line);
        }
        let body = body_lines.join("\n").trim().to_string();
        let now = Utc::now();
        let created = created.unwrap_or_else(|| updated.unwrap_or(now));
        let updated = updated.unwrap_or(created);
        let id = id.unwrap_or_else(|| generate_id(&memories, &title, created));
        memories.push(Memory {
            id,
            title,
            created,
            updated,
            body,
        });
    }
    memories
}

/// Parse a `<!-- id: … · created: … · updated: … -->` metadata line. Each field
/// is optional and order-independent.
fn parse_meta(comment: &str) -> (Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let inner = comment
        .trim_start_matches("<!--")
        .trim_end_matches("-->")
        .trim();
    let mut id = None;
    let mut created = None;
    let mut updated = None;
    for part in inner.split('·') {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "id" => id = (!value.is_empty()).then(|| value.to_string()),
            "created" => created = parse_ts(value),
            "updated" => updated = parse_ts(value),
            _ => {}
        }
    }
    (id, created, updated)
}

fn parse_ts(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Import a pre-structure file (flat bullets, free text) as one memory so an
/// upgrade is lossless. Returns empty when there's nothing but a header.
fn import_legacy(lines: &[&str]) -> Vec<Memory> {
    let body: String = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && !t.starts_with("<!--")
        })
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if body.trim().is_empty() {
        return Vec::new();
    }
    let now = Utc::now();
    vec![Memory {
        id: generate_id(&[], "imported notes", now),
        title: "Imported notes".to_string(),
        created: now,
        updated: now,
        body: body.trim().to_string(),
    }]
}

/// Serialize all memories back to `MEMORY.md` text (header + sections).
fn serialize(memories: &[Memory]) -> String {
    let mut out = String::from(MEMORY_HEADER);
    for m in memories {
        out.push('\n');
        out.push_str(&m.render());
    }
    out
}

/// Atomic write: stage into a sibling temp file, `fsync`, then `rename` over the
/// target. The temp file is created 0o600 on Unix so memory — which may hold
/// personal facts — stays owner-only. Mirrors `settings::save_to`.
fn save_to(path: &Path, memories: &[Memory]) -> Result<()> {
    let content = serialize(memories);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create memory dir {}", parent.display()))?;

    let file_name = path
        .file_name()
        .with_context(|| format!("memory path has no file name: {}", path.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = parent.join(tmp_name);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write_result = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp_path)
            .with_context(|| format!("open temp memory {}", tmp_path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("write temp memory {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp memory {}", tmp_path.display()))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

// ---------- system prompt ----------

/// Render the `<memory>` block: framing plus the disclosed titles (newest
/// first). Bodies are never injected — they are fetched on demand via `recall`.
/// Pure so the branch logic is unit-testable.
fn render_memory_block(
    memory_path: &Path,
    titles: &[(String, String, DateTime<Utc>)],
    total: usize,
) -> String {
    let mut out = String::new();
    out.push_str("<memory>\n");
    out.push_str(
        "Global, durable user memory across all yolop sessions — preferences, facts, and \
         conventions about the user (\"prefer terse answers\", \"my name is Mike\"). Only memory \
         TITLES are shown here: call `recall` with an id or a search query to read a memory's full \
         text, `remember` to add or update one, and `forget` to delete one. This is GLOBAL \
         personalization about the user, NOT project guidance (that belongs in the repo's \
         AGENTS.md).\n",
    );
    out.push_str(&format!("memory file: {}\n", memory_path.display()));

    if total == 0 {
        out.push_str("memory is empty — use `remember` to store a durable preference or fact.\n");
    } else {
        out.push_str(&format!(
            "known memories (most recent first, {} of {}):\n",
            titles.len(),
            total
        ));
        for (id, title, updated) in titles {
            out.push_str(&format!(
                "- [{id}] {title} ({date})\n",
                date = updated.format("%Y-%m-%d")
            ));
        }
        if titles.len() < total {
            out.push_str(
                "(more memories exist than shown — use `recall` with a search query to find them.)\n",
            );
        }
    }
    out.push_str("</memory>");
    out
}

// ---------- capability ----------

pub(crate) struct GlobalMemoryCapability {
    pub(crate) memory: Arc<MemoryStore>,
}

impl GlobalMemoryCapability {
    /// Resolve config leniently for the per-turn read paths: invalid config
    /// falls back to defaults (and is logged) rather than dropping the whole
    /// capability. `validate_config` is the strict guardrail on the write path.
    fn resolved_config(config: &Value) -> MemoryConfig {
        MemoryConfig::from_value(config).unwrap_or_else(|error| {
            tracing::warn!(error = %error, "invalid memory config, using defaults");
            MemoryConfig::default()
        })
    }

    fn build_tools(&self, config: MemoryConfig) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(RememberTool {
                memory: self.memory.clone(),
                config,
            }),
            Box::new(RecallTool {
                memory: self.memory.clone(),
                config,
            }),
            Box::new(ForgetTool {
                memory: self.memory.clone(),
            }),
        ]
    }
}

#[async_trait]
impl Capability for GlobalMemoryCapability {
    fn id(&self) -> &str {
        MEMORY_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Global memory"
    }
    fn description(&self) -> &str {
        "Global, durable, structured user memory. Titles are disclosed every turn; bodies are \
         recalled on demand via `remember` / `recall` / `forget`."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Personalization")
    }

    fn config_schema(&self) -> Option<Value> {
        Some(MemoryConfig::schema())
    }

    fn validate_config(&self, config: &Value) -> Result<(), String> {
        MemoryConfig::from_value(config).map(|_| ())
    }

    async fn system_prompt_contribution(&self, ctx: &SystemPromptContext) -> Option<String> {
        self.system_prompt_contribution_with_config(ctx, &Value::Null)
            .await
    }

    async fn system_prompt_contribution_with_config(
        &self,
        _ctx: &SystemPromptContext,
        config: &Value,
    ) -> Option<String> {
        let cfg = Self::resolved_config(config);
        let (titles, total) = self.memory.titles(cfg.disclosed_titles);
        Some(render_memory_block(self.memory.path(), &titles, total))
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "\
<memory>
Global, durable user memory. Titles disclosed here; read full text with `recall`,
add/update with `remember`, delete with `forget`.
known memories (most recent first, 1 of 1):
- [m-1a2b3c] Prefer terse answers (2026-06-13)
</memory>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.build_tools(MemoryConfig::default())
    }

    fn tools_with_config(&self, config: &Value) -> Vec<Box<dyn Tool>> {
        self.build_tools(Self::resolved_config(config))
    }
}

/// One-line nudge once memory has outgrown the soft cap.
fn soft_cap_warning(total: usize, soft_cap: usize) -> Option<String> {
    (soft_cap > 0 && total > soft_cap).then(|| {
        format!(
            "memory is large ({total} memories, soft cap {soft_cap}) — consider `forget`ting stale \
             memories or moving stable, topic-specific guidance into a skill."
        )
    })
}

// ---------- tools ----------

struct RememberTool {
    memory: Arc<MemoryStore>,
    config: MemoryConfig,
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Remember")
    }
    fn description(&self) -> &str {
        "Store a durable, GLOBAL memory about the user or how yolop should behave across all \
         projects (e.g. \"prefer terse answers\"). `title` is a short description; `memory` is the \
         full text. A timestamp is set automatically. Pass an existing `id` (or reuse an existing \
         title) to update in place instead of adding a duplicate. Use for personalization about \
         the user — NOT project-specific guidance (that belongs in AGENTS.md)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short description of the memory; also what search boosts and disclosure shows."
                },
                "memory": {
                    "type": "string",
                    "description": "The full memory text, phrased to stand alone on later turns."
                },
                "id": {
                    "type": "string",
                    "description": "Optional id of an existing memory to update in place (from `recall`)."
                }
            },
            "required": ["title", "memory"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let title = match arguments.get("title").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                return ToolExecutionResult::tool_error(
                    "'title' is required and must be non-empty",
                );
            }
        };
        let body = match arguments.get("memory").and_then(Value::as_str) {
            Some(b) if !b.trim().is_empty() => b,
            _ => {
                return ToolExecutionResult::tool_error(
                    "'memory' is required and must be non-empty",
                );
            }
        };
        let id = arguments
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match self.memory.remember(title, body, id) {
            Ok(outcome) => {
                let verb = if outcome.created {
                    "remembered"
                } else {
                    "updated"
                };
                let mut message = format!(
                    "{verb} [{}] ({} memories total)",
                    outcome.memory.id, outcome.total
                );
                if let Some(note) = soft_cap_warning(outcome.total, self.config.soft_cap) {
                    message.push_str(". ");
                    message.push_str(&note);
                }
                ToolExecutionResult::success(json!({
                    "ok": true,
                    "created": outcome.created,
                    "id": outcome.memory.id,
                    "total": outcome.total,
                    "message": message,
                }))
            }
            Err(e) => ToolExecutionResult::tool_error(format!("could not save memory: {e}")),
        }
    }
}

struct RecallTool {
    memory: Arc<MemoryStore>,
    config: MemoryConfig,
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Recall")
    }
    fn description(&self) -> &str {
        "Read durable memory. Pass an `id` to fetch one memory's full text, or a `query` to search \
         (title matches rank above body matches, and newer memories win ties). With neither, \
         returns the most recent memories. Results are capped (`limit`); when more match, the \
         response says so — narrow the query rather than trying to read everything."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search text. Omit to list the most recent memories."
                },
                "id": {
                    "type": "string",
                    "description": "Fetch exactly this memory by id (from disclosure or an earlier recall)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Max memories to return (defaults to the configured recall limit)."
                }
            },
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        // By-id retrieval is exact and ignores limit.
        if let Some(id) = arguments
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return match self.memory.get(id) {
                Some(m) => ToolExecutionResult::success(json!({
                    "ok": true,
                    "memory": m.to_json(),
                })),
                None => ToolExecutionResult::success(json!({
                    "ok": true,
                    "memory": Value::Null,
                    "message": format!("no memory with id '{id}'"),
                })),
            };
        }

        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .filter(|v| *v > 0)
            .unwrap_or(self.config.recall_limit)
            .max(1);
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        let result = self.memory.search(query, limit);
        let shown = result.matches.len();
        let mut message = if query.is_empty() {
            format!("{shown} most recent of {} memories", result.total_matched)
        } else {
            format!("{shown} of {} matching memories", result.total_matched)
        };
        if result.total_matched > shown {
            message.push_str("; more exist — narrow the query or raise `limit` to see them.");
        }
        ToolExecutionResult::success(json!({
            "ok": true,
            "query": query,
            "shown": shown,
            "total_matched": result.total_matched,
            "truncated": result.total_matched > shown,
            "matches": result.matches.iter().map(Memory::to_json).collect::<Vec<_>>(),
            "message": message,
        }))
    }
}

struct ForgetTool {
    memory: Arc<MemoryStore>,
}

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &str {
        "forget"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Forget")
    }
    fn description(&self) -> &str {
        "Delete one durable memory by `id` (preferred) or by its exact title. Use when a stored \
         preference or fact is no longer true."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The memory id (from `recall`/disclosure), or the exact memory title."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let key = match arguments.get("id").and_then(Value::as_str) {
            Some(k) if !k.trim().is_empty() => k,
            _ => return ToolExecutionResult::tool_error("'id' is required and must be non-empty"),
        };
        match self.memory.forget(key) {
            Ok(Some(removed)) => ToolExecutionResult::success(json!({
                "ok": true,
                "removed": true,
                "id": removed.id,
                "title": removed.title,
                "total": self.memory.len(),
            })),
            Ok(None) => ToolExecutionResult::success(json!({
                "ok": true,
                "removed": false,
                "message": format!("no memory matched '{}'", key.trim()),
            })),
            Err(e) => ToolExecutionResult::tool_error(format!("could not update memory: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in_tmp() -> (tempfile::TempDir, MemoryStore) {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = MemoryStore::open(tmp.path().join(MEMORY_FILE_NAME));
        (tmp, store)
    }

    #[test]
    fn remember_creates_then_updates_in_place_by_title() {
        let (_tmp, store) = store_in_tmp();
        let first = store
            .remember("Prefer terse", "Keep answers short.", None)
            .expect("remember");
        assert!(first.created);
        assert_eq!(first.total, 1);

        // Same title (case-insensitive) updates rather than duplicating.
        let second = store
            .remember("prefer TERSE", "Keep answers very short.", None)
            .expect("remember");
        assert!(!second.created);
        assert_eq!(second.total, 1);
        assert_eq!(second.memory.id, first.memory.id);
        assert_eq!(
            store.get(&first.memory.id).unwrap().body,
            "Keep answers very short."
        );
    }

    #[test]
    fn remember_update_by_id_changes_title_and_body() {
        let (_tmp, store) = store_in_tmp();
        let m = store.remember("Old title", "old", None).expect("remember");
        let updated = store
            .remember("New title", "new", Some(&m.memory.id))
            .expect("update");
        assert!(!updated.created);
        let got = store.get(&m.memory.id).expect("present");
        assert_eq!(got.title, "New title");
        assert_eq!(got.body, "new");
    }

    #[test]
    fn forget_removes_by_id_and_by_title() {
        let (_tmp, store) = store_in_tmp();
        let a = store.remember("Alpha", "a", None).expect("a");
        let _b = store.remember("Beta", "b", None).expect("b");
        assert_eq!(store.len(), 2);

        assert!(store.forget(&a.memory.id).expect("forget").is_some());
        assert_eq!(store.len(), 1);
        assert!(store.forget("beta").expect("forget by title").is_some());
        assert_eq!(store.len(), 0);
        assert!(store.forget("nope").expect("forget missing").is_none());
    }

    #[test]
    fn search_boosts_title_over_body_and_caps_with_total() {
        let (_tmp, store) = store_in_tmp();
        // "rust" only in body.
        store
            .remember("Editor", "Uses rust-analyzer in the editor.", None)
            .expect("m1");
        // "rust" in the title — should rank first.
        store
            .remember("Rust style", "Prefers explicit error handling.", None)
            .expect("m2");
        store
            .remember("Unrelated", "nothing here", None)
            .expect("m3");

        let result = store.search("rust", 1);
        assert_eq!(result.total_matched, 2);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].title, "Rust style");
    }

    #[test]
    fn empty_query_returns_recent() {
        let (_tmp, store) = store_in_tmp();
        store.remember("One", "1", None).expect("m1");
        store.remember("Two", "2", None).expect("m2");
        let result = store.search("   ", 5);
        assert_eq!(result.total_matched, 2);
        // Most recently remembered comes first.
        assert_eq!(result.matches[0].title, "Two");
    }

    #[test]
    fn persists_and_reparses_across_reopen() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("nested").join(MEMORY_FILE_NAME);
        let store = MemoryStore::open(path.clone());
        let m = store
            .remember("Likes blue", "The user likes the color blue.", None)
            .expect("remember");

        let reopened = MemoryStore::open(path);
        let got = reopened.get(&m.memory.id).expect("survives reopen");
        assert_eq!(got.title, "Likes blue");
        assert_eq!(got.body, "The user likes the color blue.");
        assert_eq!(got.created, m.memory.created);
    }

    #[test]
    fn legacy_bullet_file_is_imported_losslessly() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join(MEMORY_FILE_NAME);
        std::fs::write(
            &path,
            "# yolop memory\n\n- prefer terse answers\n- name is Mike\n",
        )
        .expect("seed legacy");
        let store = MemoryStore::open(path);
        assert_eq!(store.len(), 1);
        let imported = &store.recent(1).matches[0];
        assert_eq!(imported.title, "Imported notes");
        assert!(imported.body.contains("prefer terse answers"));
        assert!(imported.body.contains("name is Mike"));
    }

    #[test]
    fn body_with_markdown_heading_survives_round_trip() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join(MEMORY_FILE_NAME);
        let store = MemoryStore::open(path.clone());
        // A body that itself contains a `## ` heading line must not be re-parsed
        // into a second spurious memory on reload.
        let body = "Commit style:\n\n## Heading rules\nUse `## ` for sections.";
        let m = store
            .remember("Markdown habits", body, None)
            .expect("remember");

        let reopened = MemoryStore::open(path);
        assert_eq!(reopened.len(), 1);
        let got = reopened.get(&m.memory.id).expect("single memory survives");
        assert_eq!(got.title, "Markdown habits");
        assert!(got.body.contains("## Heading rules"));
        assert!(got.body.contains("Use `## ` for sections."));
    }

    #[test]
    fn titles_disclosure_caps_and_reports_total() {
        let (_tmp, store) = store_in_tmp();
        for i in 0..20 {
            store
                .remember(&format!("Memory {i}"), "body", None)
                .expect("remember");
        }
        let (titles, total) = store.titles(15);
        assert_eq!(total, 20);
        assert_eq!(titles.len(), 15);
    }

    #[test]
    fn render_block_handles_empty_and_overflow() {
        let empty = render_memory_block(Path::new("/cfg/MEMORY.md"), &[], 0);
        assert!(empty.starts_with("<memory>\n"));
        assert!(empty.contains("memory is empty"));
        assert!(empty.ends_with("</memory>"));

        let now = Utc::now();
        let titles = vec![("m-1".to_string(), "Prefer terse".to_string(), now)];
        let block = render_memory_block(Path::new("/cfg/MEMORY.md"), &titles, 9);
        assert!(block.contains("known memories (most recent first, 1 of 9)"));
        assert!(block.contains("- [m-1] Prefer terse"));
        assert!(block.contains("more memories exist than shown"));
    }

    #[test]
    fn config_parses_from_capability_config_value() {
        // Null / empty → defaults.
        assert_eq!(
            MemoryConfig::from_value(&Value::Null).unwrap(),
            MemoryConfig::default()
        );
        assert_eq!(
            MemoryConfig::from_value(&json!({})).unwrap(),
            MemoryConfig::default()
        );

        // Present keys override; unset keys keep defaults.
        let cfg = MemoryConfig::from_value(&json!({ "disclosed_titles": 5, "recall_limit": 3 }))
            .expect("valid");
        assert_eq!(cfg.disclosed_titles, 5);
        assert_eq!(cfg.recall_limit, 3);
        assert_eq!(cfg.soft_cap, MemoryConfig::default().soft_cap);

        // Wrong types are rejected — this is the `validate_config` guardrail.
        assert!(MemoryConfig::from_value(&json!({ "soft_cap": -1 })).is_err());
        assert!(MemoryConfig::from_value(&json!({ "recall_limit": "lots" })).is_err());
        assert!(MemoryConfig::from_value(&json!([1, 2, 3])).is_err());
    }

    #[test]
    fn capability_config_schema_validates_and_tools_read_config() {
        let (_tmp, store) = store_in_tmp();
        let cap = GlobalMemoryCapability {
            memory: Arc::new(store),
        };
        assert!(cap.config_schema().is_some());
        assert!(cap.validate_config(&json!({ "recall_limit": 2 })).is_ok());
        assert!(cap.validate_config(&json!({ "recall_limit": -2 })).is_err());

        // Config drives the tools built via the capability-config path.
        let tools = cap.tools_with_config(&json!({ "recall_limit": 2 }));
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn soft_cap_warning_only_past_limit() {
        assert!(soft_cap_warning(5, 200).is_none());
        assert!(soft_cap_warning(201, 200).is_some());
        // A zero cap disables the warning entirely.
        assert!(soft_cap_warning(10_000, 0).is_none());
    }

    #[tokio::test]
    async fn remember_tool_then_recall_by_query_and_id() {
        let (_tmp, store) = store_in_tmp();
        let store = Arc::new(store);
        let remember = RememberTool {
            memory: store.clone(),
            config: MemoryConfig::default(),
        };
        let res = remember
            .execute(json!({ "title": "Prefer spaces", "memory": "Use spaces, not tabs." }))
            .await;
        assert!(res.is_success());

        let recall = RecallTool {
            memory: store.clone(),
            config: MemoryConfig::default(),
        };
        let by_query = recall.execute(json!({ "query": "spaces" })).await;
        assert!(by_query.is_success());
        let text = format!("{by_query:?}");
        assert!(text.contains("Use spaces, not tabs."), "got: {text}");
    }

    #[tokio::test]
    async fn recall_reports_more_when_truncated() {
        let (_tmp, store) = store_in_tmp();
        let store = Arc::new(store);
        for i in 0..4 {
            store
                .remember(&format!("Note {i}"), "shared keyword body", None)
                .expect("seed");
        }
        let recall = RecallTool {
            memory: store.clone(),
            config: MemoryConfig::default(),
        };
        let res = recall
            .execute(json!({ "query": "keyword", "limit": 2 }))
            .await;
        assert!(res.is_success());
        let text = format!("{res:?}");
        assert!(text.contains("more exist"), "got: {text}");
    }

    #[tokio::test]
    async fn remember_tool_rejects_empty_fields() {
        let (_tmp, store) = store_in_tmp();
        let tool = RememberTool {
            memory: Arc::new(store),
            config: MemoryConfig::default(),
        };
        assert!(
            tool.execute(json!({ "title": " ", "memory": "x" }))
                .await
                .is_error()
        );
        assert!(
            tool.execute(json!({ "title": "x", "memory": " " }))
                .await
                .is_error()
        );
    }

    #[tokio::test]
    async fn forget_tool_reports_removal() {
        let (_tmp, store) = store_in_tmp();
        let store = Arc::new(store);
        store.remember("Throwaway", "temp", None).expect("seed");
        let tool = ForgetTool {
            memory: store.clone(),
        };
        let hit = tool.execute(json!({ "id": "Throwaway" })).await;
        assert!(hit.is_success());
        assert!(format!("{hit:?}").contains("\"removed\": true") || store.len() == 0);

        let miss = tool.execute(json!({ "id": "ghost" })).await;
        assert!(miss.is_success());
    }

    #[test]
    fn capability_exposes_memory_tools_without_slash_command() {
        let (_tmp, store) = store_in_tmp();
        let capability = GlobalMemoryCapability {
            memory: Arc::new(store),
        };
        let names: Vec<String> = capability
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["remember", "recall", "forget"]);
        assert!(capability.commands().is_empty());
    }
}

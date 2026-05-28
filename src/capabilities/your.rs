// The `your` capability — yolop's personalization layer.
//
// "your" is how a user addresses yolop itself: "what is your config?",
// "update your memory", "set yolop blue", "remember that I prefer terse
// answers". These are GLOBAL personalization requests about yolop the tool,
// distinct from changes to the current project (which belong in the repo's
// AGENTS.md, source, and tests).
//
// v1 owns a single piece of state: a central MEMORY.md of durable, cross-
// session user preferences. It is injected into every turn under a managed
// byte budget, with a soft limit that nudges bulky guidance toward skills.
// The model edits it through natural-language tools (`remember`,
// `read_memory`, `write_memory`); the user inspects it via `/your`.
//
// See specs/your.md for the full vision (global skills, hooks, user-defined
// capabilities) — all of which hang off the same central config dir.

use crate::settings::SettingsStore;
use anyhow::{Context, Result};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::command::{
    CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource, ExecuteCommandRequest,
};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const YOUR_CAPABILITY_ID: &str = "your";

/// Central memory file name, co-located with `settings.toml` in the yolop
/// config dir.
pub(crate) const MEMORY_FILE_NAME: &str = "MEMORY.md";

/// Hard cap on how many bytes of memory are injected into a turn. Beyond
/// this the injection is truncated (at a char boundary) with a notice; the
/// full file is still reachable via `read_memory`.
const MEMORY_INJECT_BUDGET_BYTES: usize = 8 * 1024;

/// Soft limit: once memory exceeds this, both the injected block and tool
/// results suggest extracting stable, topic-specific guidance into a skill
/// rather than letting memory grow unbounded.
const MEMORY_SUGGEST_SKILL_BYTES: usize = 4 * 1024;

const MEMORY_HEADER: &str = "# yolop memory\n\nDurable, global user preferences. Edit via the `remember` / `write_memory` tools or by chatting with yolop (\"remember that …\", \"what is your config?\").\n";

/// Thread-safe handle to the central `MEMORY.md`. Mirrors `SettingsStore`:
/// the cached string is the in-memory source of truth and mutations flush to
/// disk via an atomic temp-file + rename, so readers and crashes never see a
/// half-written file.
pub(crate) struct YourStore {
    path: PathBuf,
    inner: Mutex<String>,
}

impl YourStore {
    pub(crate) fn open(path: PathBuf) -> Self {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(content),
        }
    }

    /// Derive the memory path from the settings file's directory so memory
    /// lives next to `settings.toml` and tests that point settings at a
    /// tempdir get an isolated memory file for free.
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

    pub(crate) fn snapshot(&self) -> String {
        self.inner.lock().expect("memory lock poisoned").clone()
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.inner.lock().expect("memory lock poisoned").len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("memory lock poisoned")
            .trim()
            .is_empty()
    }

    /// Append a single durable note as a markdown bullet, seeding the header
    /// on first write. Returns the new byte length.
    pub(crate) fn append_note(&self, note: &str) -> Result<usize> {
        let note = note.trim();
        let mut guard = self.inner.lock().expect("memory lock poisoned");
        if guard.trim().is_empty() {
            *guard = MEMORY_HEADER.to_string();
        }
        if !guard.ends_with('\n') {
            guard.push('\n');
        }
        guard.push_str("- ");
        guard.push_str(note);
        guard.push('\n');
        save_to(&self.path, &guard)?;
        Ok(guard.len())
    }

    /// Replace the entire memory file. Returns the new byte length.
    pub(crate) fn write(&self, content: &str) -> Result<usize> {
        let mut guard = self.inner.lock().expect("memory lock poisoned");
        *guard = content.to_string();
        save_to(&self.path, &guard)?;
        Ok(guard.len())
    }
}

/// Atomic write: stage into a sibling temp file, `fsync`, then `rename` over
/// the target (POSIX rename is atomic). The temp file is created 0o600 on
/// Unix so memory — which may hold personal facts — stays owner-only. Mirrors
/// `settings::save_to`.
fn save_to(path: &Path, content: &str) -> Result<()> {
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

/// Truncate to at most `budget` bytes, snapping back to a char boundary so we
/// never split a UTF-8 sequence. Returns the slice and whether it was clipped.
fn clip_to_budget(content: &str, budget: usize) -> (&str, bool) {
    if content.len() <= budget {
        return (content, false);
    }
    let mut end = budget;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    (&content[..end], true)
}

/// One-line nudge appended to tool results / status once memory is large.
fn skill_suggestion(byte_len: usize) -> Option<&'static str> {
    (byte_len > MEMORY_SUGGEST_SKILL_BYTES).then_some(
        "Memory is getting large — consider moving stable, topic-specific \
         guidance into a skill so it loads on demand instead of every turn.",
    )
}

/// Render the `<your>` system-prompt block: the framing, the memory file
/// path, and the (budget-clipped) memory contents with truncation /
/// skill-suggestion notices. Pure so the branch logic is unit-testable
/// without constructing a `SystemPromptContext`.
fn render_memory_block(memory_path: &Path, content: &str) -> String {
    let mut out = String::new();
    out.push_str("<your>\n");
    out.push_str(
        "yolop's personalization layer. When the user addresses \"you\" or \"yolop\" itself \
         — e.g. \"what is your config?\", \"update your settings\", \"set yolop blue\", \
         \"remember that I prefer X\" — treat it as a GLOBAL personalization request about \
         yolop, NOT a change to the current project. Persist durable user preferences with \
         the `remember` tool, reorganize with `write_memory`, and read the full set with \
         `read_memory`. Project-specific guidance belongs in the repo's AGENTS.md, not here.\n",
    );
    out.push_str(&format!("memory file: {}\n", memory_path.display()));

    if content.trim().is_empty() {
        out.push_str("memory is empty.\n");
    } else {
        let (shown, clipped) = clip_to_budget(content, MEMORY_INJECT_BUDGET_BYTES);
        out.push_str("<your_memory>\n");
        out.push_str(shown);
        if !shown.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</your_memory>\n");
        if clipped {
            out.push_str(
                "(memory truncated to fit the prompt budget — call `read_memory` for the full text.)\n",
            );
        }
        if let Some(note) = skill_suggestion(content.len()) {
            out.push_str(note);
            out.push('\n');
        }
    }
    out.push_str("</your>");
    out
}

/// Render the `/your` status line. Pure for testability.
fn memory_status(memory_path: &Path, is_empty: bool, byte_len: usize) -> String {
    let state = if is_empty {
        "empty".to_string()
    } else if byte_len > MEMORY_SUGGEST_SKILL_BYTES {
        format!("{byte_len} bytes — large, consider a skill")
    } else {
        format!("{byte_len} bytes")
    };
    format!(
        "your — global personalization. memory: {} ({state}). \
         Edit by chatting (\"remember that …\", \"what is your config?\") \
         or with the remember/read_memory/write_memory tools.",
        memory_path.display()
    )
}

// ---------- capability ----------

pub(crate) struct YourCapability {
    pub(crate) memory: Arc<YourStore>,
}

#[async_trait]
impl Capability for YourCapability {
    fn id(&self) -> &str {
        YOUR_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Your (personalization)"
    }
    fn description(&self) -> &str {
        "Global yolop personalization: durable user memory, injected every turn, edited in natural language."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Personalization")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(render_memory_block(
            self.memory.path(),
            &self.memory.snapshot(),
        ))
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "\
<your>
yolop's personalization layer (global). Use `remember` / `read_memory` /
`write_memory` for durable user preferences.
<your_memory>
- example: prefer terse answers
</your_memory>
</your>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(RememberTool {
                memory: self.memory.clone(),
            }),
            Box::new(ReadMemoryTool {
                memory: self.memory.clone(),
            }),
            Box::new(WriteMemoryTool {
                memory: self.memory.clone(),
            }),
        ]
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "your".to_string(),
            description: "Show yolop's personalization (global memory) status.".to_string(),
            source: CommandSource::System,
            args: vec![],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "your" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        Ok(CommandResult {
            success: true,
            message: memory_status(
                self.memory.path(),
                self.memory.is_empty(),
                self.memory.byte_len(),
            ),
            error_code: None,
            error_fields: None,
        })
    }
}

// ---------- tools ----------

struct RememberTool {
    memory: Arc<YourStore>,
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
        "Persist a durable, GLOBAL user preference or fact about how yolop should behave across \
         all projects (e.g. \"prefer terse answers\"). Appends one note to yolop's central \
         memory, which is injected every turn. Use for personalization requests about yolop \
         itself — NOT for project-specific guidance (that belongs in AGENTS.md)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "A single durable preference or fact, phrased so it stands alone on later turns."
                }
            },
            "required": ["note"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let note = match arguments.get("note").and_then(Value::as_str) {
            Some(n) if !n.trim().is_empty() => n,
            _ => {
                return ToolExecutionResult::tool_error("'note' is required and must be non-empty");
            }
        };
        match self.memory.append_note(note) {
            Ok(len) => {
                let mut msg = format!("remembered ({len} bytes total)");
                if let Some(note) = skill_suggestion(len) {
                    msg.push_str(". ");
                    msg.push_str(note);
                }
                ToolExecutionResult::success(json!({ "ok": true, "message": msg }))
            }
            Err(e) => ToolExecutionResult::tool_error(format!("could not save memory: {e}")),
        }
    }
}

struct ReadMemoryTool {
    memory: Arc<YourStore>,
}

#[async_trait]
impl Tool for ReadMemoryTool {
    fn name(&self) -> &str {
        "read_memory"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Read memory")
    }
    fn description(&self) -> &str {
        "Read the full contents of yolop's central personalization memory. Use this to answer \
         \"what is your config?\"-style questions, or before `write_memory` when the injected \
         copy may have been truncated."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        let content = self.memory.snapshot();
        ToolExecutionResult::success(json!({
            "path": self.memory.path().display().to_string(),
            "bytes": content.len(),
            "content": content,
        }))
    }
}

struct WriteMemoryTool {
    memory: Arc<YourStore>,
}

#[async_trait]
impl Tool for WriteMemoryTool {
    fn name(&self) -> &str {
        "write_memory"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Write memory")
    }
    fn description(&self) -> &str {
        "Replace yolop's entire central personalization memory with new markdown. Use to edit, \
         remove, or reorganize preferences — read the current contents first with `read_memory`. \
         Appending a single new preference is better done with `remember`."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The full new memory file contents (markdown)."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let content = match arguments.get("content").and_then(Value::as_str) {
            Some(c) => c,
            None => return ToolExecutionResult::tool_error("'content' is required"),
        };
        match self.memory.write(content) {
            Ok(len) => {
                let mut msg = format!("memory updated ({len} bytes)");
                if let Some(note) = skill_suggestion(len) {
                    msg.push_str(". ");
                    msg.push_str(note);
                }
                ToolExecutionResult::success(json!({ "ok": true, "message": msg }))
            }
            Err(e) => ToolExecutionResult::tool_error(format!("could not save memory: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in_tmp() -> (tempfile::TempDir, YourStore) {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("MEMORY.md");
        let store = YourStore::open(path);
        (tmp, store)
    }

    #[test]
    fn append_seeds_header_then_bullets() {
        let (_tmp, store) = store_in_tmp();
        assert!(store.is_empty());
        store.append_note("prefer terse answers").expect("append");
        store.append_note("name is Mike").expect("append");

        let content = store.snapshot();
        assert!(content.starts_with("# yolop memory"));
        assert!(content.contains("- prefer terse answers\n"));
        assert!(content.contains("- name is Mike\n"));
        assert!(!store.is_empty());
    }

    #[test]
    fn append_persists_across_reopen() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("nested").join("MEMORY.md");
        let store = YourStore::open(path.clone());
        store.append_note("likes blue").expect("append");

        let reopened = YourStore::open(path);
        assert!(reopened.snapshot().contains("- likes blue\n"));
    }

    #[test]
    fn write_replaces_whole_file() {
        let (_tmp, store) = store_in_tmp();
        store.append_note("old fact").expect("append");
        store.write("# fresh\n\n- only this\n").expect("write");
        let content = store.snapshot();
        assert!(!content.contains("old fact"));
        assert!(content.contains("- only this"));
    }

    #[test]
    fn clip_respects_budget_and_char_boundaries() {
        let (short, clipped) = clip_to_budget("hello", 100);
        assert_eq!(short, "hello");
        assert!(!clipped);

        // Multi-byte chars: budget falls mid-sequence, must snap back.
        let s = "héllo wörld"; // é and ö are 2 bytes each
        let (slice, clipped) = clip_to_budget(s, 2);
        assert!(clipped);
        assert!(s.starts_with(slice));
        // Did not panic and produced valid UTF-8 (guaranteed by &str type).
        assert!(slice.len() <= 2);
    }

    #[test]
    fn skill_suggestion_only_past_soft_limit() {
        assert!(skill_suggestion(10).is_none());
        assert!(skill_suggestion(MEMORY_SUGGEST_SKILL_BYTES + 1).is_some());
    }

    #[tokio::test]
    async fn remember_tool_appends_and_reports() {
        let (_tmp, store) = store_in_tmp();
        let tool = RememberTool {
            memory: Arc::new(store),
        };
        let res = tool.execute(json!({ "note": "use spaces not tabs" })).await;
        assert!(res.is_success());
        assert!(tool.memory.snapshot().contains("- use spaces not tabs"));
    }

    #[tokio::test]
    async fn remember_tool_rejects_empty_note() {
        let (_tmp, store) = store_in_tmp();
        let tool = RememberTool {
            memory: Arc::new(store),
        };
        let res = tool.execute(json!({ "note": "   " })).await;
        assert!(res.is_error());
    }

    #[tokio::test]
    async fn read_memory_tool_returns_content() {
        let (_tmp, store) = store_in_tmp();
        store.append_note("fact one").expect("append");
        let tool = ReadMemoryTool {
            memory: Arc::new(store),
        };
        let res = tool.execute(json!({})).await;
        assert!(res.is_success());
    }

    #[tokio::test]
    async fn write_memory_tool_replaces_and_reports() {
        let (_tmp, store) = store_in_tmp();
        let tool = WriteMemoryTool {
            memory: Arc::new(store),
        };
        let res = tool.execute(json!({ "content": "# x\n- one\n" })).await;
        assert!(res.is_success());
        assert_eq!(tool.memory.snapshot(), "# x\n- one\n");
    }

    #[tokio::test]
    async fn write_memory_tool_requires_content() {
        let (_tmp, store) = store_in_tmp();
        let tool = WriteMemoryTool {
            memory: Arc::new(store),
        };
        let res = tool.execute(json!({})).await;
        assert!(res.is_error());
    }

    #[tokio::test]
    async fn remember_tool_suggests_skill_when_large() {
        let (_tmp, store) = store_in_tmp();
        // Seed past the soft limit so the next append crosses it.
        store
            .write(&format!("# m\n{}", "- x\n".repeat(1200)))
            .expect("seed");
        let tool = RememberTool {
            memory: Arc::new(store),
        };
        let res = tool.execute(json!({ "note": "one more" })).await;
        assert!(res.is_success());
        let text = format!("{res:?}");
        assert!(text.contains("Memory is getting large"), "got: {text}");
    }

    #[test]
    fn render_block_reports_empty_memory() {
        let block = render_memory_block(Path::new("/cfg/MEMORY.md"), "   \n");
        assert!(block.starts_with("<your>\n"));
        assert!(block.contains("memory file: /cfg/MEMORY.md"));
        assert!(block.contains("memory is empty."));
        assert!(!block.contains("<your_memory>"));
        assert!(block.ends_with("</your>"));
    }

    #[test]
    fn render_block_wraps_small_memory_without_notices() {
        let block = render_memory_block(Path::new("/cfg/MEMORY.md"), "- prefer terse\n");
        assert!(block.contains("<your_memory>\n- prefer terse\n</your_memory>\n"));
        assert!(!block.contains("truncated"));
        assert!(!block.contains("Memory is getting large"));
    }

    #[test]
    fn render_block_truncates_and_suggests_when_oversized() {
        let big = "a".repeat(MEMORY_INJECT_BUDGET_BYTES + 100);
        let block = render_memory_block(Path::new("/cfg/MEMORY.md"), &big);
        assert!(block.contains("truncated to fit the prompt budget"));
        // Over the inject budget is also over the soft limit, so both fire.
        assert!(block.contains("Memory is getting large"));
        // The injected slice itself is capped at the budget.
        assert!(!block.contains(&"a".repeat(MEMORY_INJECT_BUDGET_BYTES + 1)));
    }

    #[test]
    fn memory_status_covers_all_states() {
        let p = Path::new("/cfg/MEMORY.md");
        assert!(memory_status(p, true, 0).contains("(empty)"));
        assert!(memory_status(p, false, 100).contains("(100 bytes)"));
        let large = memory_status(p, false, MEMORY_SUGGEST_SKILL_BYTES + 1);
        assert!(large.contains("large, consider a skill"));
    }
}

//! TODO(EVE-620): remove this entire module once everruns ships an `edits[]`-only
//! `edit_file`. Then restore the plain `capabilities.register(FileSystemCapability)`
//! call in `runtime.rs`. See
//! <https://linear.app/everruns/issue/EVE-620>.
//!
//! Why this exists: everruns' `edit_file` advertises two overlapping ways to
//! specify an edit in one call — top-level `old_text`/`new_text` *and* an
//! `edits[]` array. gpt-5.5 reliably populates *both* on its first attempt
//! (duplicating the same edit), which the tool rejects as ambiguous — burning a
//! turn before it self-corrects. pi/codex/claude-code each expose a single edit
//! shape and never hit this. This wrapper makes yolop's `edit_file` `edits[]`-only
//! (dropping the scalar fields from the advertised schema) and folds any stray
//! top-level `old_text`/`new_text` into `edits[]` before delegating, so the
//! upstream tool never sees the ambiguous shape. Execution and narration are
//! delegated unchanged to the upstream tool.

use async_trait::async_trait;
use everruns_core::capabilities::{
    Capability, CapabilityLocalization, CapabilityStatus, SystemPromptContext,
};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::tool_types::{ToolCall, ToolHints, ToolPolicy};
use everruns_core::tools::{Tool, ToolExecutionResult};
use everruns_core::{FileSystemCapability, ToolContext};
use serde_json::{Value, json};

const EDIT_FILE: &str = "edit_file";

const EDIT_FILE_DESCRIPTION: &str = "Apply one or more exact text replacements to an existing text \
file. Requires the current content hash from read_file or write_file. Put every replacement in the \
`edits` array — each `old_text` must match the file exactly and be unique.";

/// `FileSystemCapability` with `edit_file` swapped for an `edits[]`-only variant.
/// Everything else delegates to the inner capability unchanged.
pub(crate) struct EditsOnlyFileSystemCapability {
    inner: FileSystemCapability,
}

impl EditsOnlyFileSystemCapability {
    pub(crate) fn new() -> Self {
        Self {
            inner: FileSystemCapability,
        }
    }
}

impl Default for EditsOnlyFileSystemCapability {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capability for EditsOnlyFileSystemCapability {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn localizations(&self) -> Vec<CapabilityLocalization> {
        self.inner.localizations()
    }

    fn status(&self) -> CapabilityStatus {
        self.inner.status()
    }

    fn icon(&self) -> Option<&str> {
        self.inner.icon()
    }

    fn category(&self) -> Option<&str> {
        self.inner.category()
    }

    fn features(&self) -> Vec<&'static str> {
        self.inner.features()
    }

    fn system_prompt_preview(&self) -> Option<String> {
        self.inner.system_prompt_preview()
    }

    async fn system_prompt_contribution(&self, ctx: &SystemPromptContext) -> Option<String> {
        self.inner.system_prompt_contribution(ctx).await
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.inner
            .tools()
            .into_iter()
            .map(|tool| {
                if tool.name() == EDIT_FILE {
                    Box::new(EditsOnlyEditFileTool { inner: tool }) as Box<dyn Tool>
                } else {
                    tool
                }
            })
            .collect()
    }
}

/// `edit_file` that advertises only the `edits[]` shape. Delegates execution and
/// narration to the upstream tool; coerces away any stray top-level
/// `old_text`/`new_text` first so the upstream never sees the ambiguous dual mode.
struct EditsOnlyEditFileTool {
    inner: Box<dyn Tool>,
}

impl EditsOnlyEditFileTool {
    /// Strip top-level `old_text`/`new_text`, folding them into `edits[]` when no
    /// `edits` was supplied. Defensive: the advertised schema no longer offers
    /// the scalar fields, but a model may still emit them out of habit.
    fn coerce_arguments(mut arguments: Value) -> Value {
        if let Some(obj) = arguments.as_object_mut() {
            let old_text = obj.remove("old_text");
            let new_text = obj.remove("new_text");
            let has_edits = obj.get("edits").is_some_and(|edits| !edits.is_null());
            if !has_edits && let (Some(old_text), Some(new_text)) = (old_text, new_text) {
                obj.insert(
                    "edits".to_string(),
                    json!([{ "old_text": old_text, "new_text": new_text }]),
                );
            }
        }
        arguments
    }
}

#[async_trait]
impl Tool for EditsOnlyEditFileTool {
    fn name(&self) -> &str {
        EDIT_FILE
    }

    fn display_name(&self) -> Option<&str> {
        self.inner.display_name()
    }

    fn description(&self) -> &str {
        EDIT_FILE_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the existing text file (e.g., '/workspace/src/main.rs')"
                },
                "expected_hash": {
                    "type": "string",
                    "description": "Current content hash from read_file or write_file (format: 'sha256:...')"
                },
                "edits": {
                    "type": "array",
                    "description": "One or more exact text replacements, each matched against the original file content. Order-independent; replacements must not overlap.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "description": "Exact text to replace (must be unique in the file)"
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement text"
                            }
                        },
                        "required": ["old_text", "new_text"],
                        "additionalProperties": false
                    },
                    "minItems": 1
                }
            },
            "required": ["path", "expected_hash", "edits"],
            "additionalProperties": false
        })
    }

    fn requires_context(&self) -> bool {
        self.inner.requires_context()
    }

    fn policy(&self) -> ToolPolicy {
        self.inner.policy()
    }

    fn hints(&self) -> ToolHints {
        self.inner.hints()
    }

    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        self.inner.narrate(tool_call, phase, locale, ctx)
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        self.inner.execute(Self::coerce_arguments(arguments)).await
    }

    async fn execute_with_context(
        &self,
        arguments: Value,
        context: &ToolContext,
    ) -> ToolExecutionResult {
        self.inner
            .execute_with_context(Self::coerce_arguments(arguments), context)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_edits_only() {
        let tool = EditsOnlyEditFileTool {
            inner: FileSystemCapability
                .tools()
                .into_iter()
                .find(|t| t.name() == EDIT_FILE)
                .expect("edit_file tool"),
        };
        let schema = tool.parameters_schema();
        let props = &schema["properties"];
        assert!(props.get("old_text").is_none(), "old_text must be gone");
        assert!(props.get("new_text").is_none(), "new_text must be gone");
        assert!(props.get("edits").is_some(), "edits[] must remain");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r == "edits"));
    }

    #[test]
    fn coerce_folds_scalars_into_edits() {
        let out = EditsOnlyEditFileTool::coerce_arguments(json!({
            "path": "/workspace/f.rs", "expected_hash": "sha256:x",
            "old_text": "a", "new_text": "b"
        }));
        assert!(out.get("old_text").is_none());
        assert!(out.get("new_text").is_none());
        assert_eq!(out["edits"], json!([{ "old_text": "a", "new_text": "b" }]));
    }

    #[test]
    fn coerce_drops_scalars_when_edits_present() {
        let out = EditsOnlyEditFileTool::coerce_arguments(json!({
            "path": "/workspace/f.rs", "expected_hash": "sha256:x",
            "old_text": "a", "new_text": "b",
            "edits": [{ "old_text": "c", "new_text": "d" }]
        }));
        assert!(out.get("old_text").is_none());
        assert!(out.get("new_text").is_none());
        assert_eq!(out["edits"], json!([{ "old_text": "c", "new_text": "d" }]));
    }
}

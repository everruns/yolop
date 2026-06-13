// The `hooks` capability — authoring and inspecting Yolop hook config.
//
// The hook engine itself is upstream `user_hooks`; this capability is only the
// natural-language-safe write surface for Yolop's global/workspace `hooks.json`
// files.

use crate::hooks_config::{HookScope, HooksStore};
use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const HOOKS_CAPABILITY_ID: &str = "hooks";

pub(crate) struct HooksCapability {
    pub(crate) hooks: Arc<HooksStore>,
}

#[async_trait]
impl Capability for HooksCapability {
    fn id(&self) -> &str {
        HOOKS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Hooks"
    }

    fn description(&self) -> &str {
        "Authoring and inspection tools for global and workspace hook configuration."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Extensibility")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(
            "<capability id=\"hooks\">\n\
             Configure hook requests such as \"setup a hook to prevent calls to git\" with \
             `validate_hook` and `upsert_hook`. Use `list_hooks` before changing existing hooks \
             and `remove_hook` when removing or disabling one. Do not store hook requests as \
             memory notes; hooks are real global/workspace configuration.\n\
             </capability>"
                .to_string(),
        )
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "<capability id=\"hooks\">\n\
             Configure global/workspace hooks with `list_hooks`, `validate_hook`, `upsert_hook`, \
             and `remove_hook`.\n\
             </capability>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(ListHooksTool {
                hooks: self.hooks.clone(),
            }),
            Box::new(ValidateHookTool {
                hooks: self.hooks.clone(),
            }),
            Box::new(UpsertHookTool {
                hooks: self.hooks.clone(),
            }),
            Box::new(RemoveHookTool {
                hooks: self.hooks.clone(),
            }),
        ]
    }
}

struct ListHooksTool {
    hooks: Arc<HooksStore>,
}

#[async_trait]
impl Tool for ListHooksTool {
    fn name(&self) -> &str {
        "list_hooks"
    }

    fn display_name(&self) -> Option<&str> {
        Some("List hooks")
    }

    fn description(&self) -> &str {
        "List Yolop hooks from global and workspace hook config. Use for \
         \"what hooks are configured?\" or before changing an existing hook."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        let effective = self.hooks.effective();
        ToolExecutionResult::success(json!({
            "ok": true,
            "global_path": effective.global_path.display().to_string(),
            "workspace_path": effective.workspace_path.display().to_string(),
            "count": effective.hooks.len(),
            "scope_counts": effective.scope_counts(),
            "hooks": effective.summaries(),
        }))
    }
}

struct ValidateHookTool {
    hooks: Arc<HooksStore>,
}

#[async_trait]
impl Tool for ValidateHookTool {
    fn name(&self) -> &str {
        "validate_hook"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Validate hook")
    }

    fn description(&self) -> &str {
        "Validate a candidate Yolop hook spec without writing it. Use before `upsert_hook`, \
         especially when translating a natural-language request into hook JSON."
    }

    fn parameters_schema(&self) -> Value {
        hook_value_schema()
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let hook = match arguments.get("hook") {
            Some(hook) => hook.clone(),
            None => return ToolExecutionResult::tool_error("'hook' is required"),
        };
        match self.hooks.validate_hook(&hook) {
            Ok(entry) => ToolExecutionResult::success(json!({
                "ok": true,
                "hook": entry.to_validation_json(),
            })),
            Err(error) => ToolExecutionResult::tool_error(format!("invalid hook: {error}")),
        }
    }
}

struct UpsertHookTool {
    hooks: Arc<HooksStore>,
}

#[async_trait]
impl Tool for UpsertHookTool {
    fn name(&self) -> &str {
        "upsert_hook"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Save hook")
    }

    fn description(&self) -> &str {
        "Create or replace one Yolop hook by id. Use global scope for personal Yolop behavior \
         and workspace scope for project-owned hook config. Validates before writing."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["global", "workspace"],
                    "description": "Where to write the hook. Use global for personal Yolop configuration; workspace for this repo."
                },
                "hook": {
                    "type": "object",
                    "description": "A UserHookSpec object with a stable id."
                }
            },
            "required": ["scope", "hook"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let scope = match parse_scope_arg(&arguments) {
            Ok(scope) => scope,
            Err(error) => return ToolExecutionResult::tool_error(error),
        };
        let hook = match arguments.get("hook") {
            Some(hook) => hook.clone(),
            None => return ToolExecutionResult::tool_error("'hook' is required"),
        };
        match self.hooks.upsert_hook(scope, hook) {
            Ok(entry) => ToolExecutionResult::success(json!({
                "ok": true,
                "message": format!("saved {} hook", scope.as_str()),
                "hook": entry.to_summary_json(),
                "path": self.hooks.path_for(scope).display().to_string(),
            })),
            Err(error) => ToolExecutionResult::tool_error(format!("could not save hook: {error}")),
        }
    }
}

struct RemoveHookTool {
    hooks: Arc<HooksStore>,
}

#[async_trait]
impl Tool for RemoveHookTool {
    fn name(&self) -> &str {
        "remove_hook"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Remove hook")
    }

    fn description(&self) -> &str {
        "Remove one Yolop hook by id from the selected scope. Workspace removal also writes a \
         disabled marker so a lower-precedence global hook with the same id stays disabled."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["global", "workspace"]
                },
                "id": {
                    "type": "string",
                    "description": "Stable hook id to remove or disable."
                }
            },
            "required": ["scope", "id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let scope = match parse_scope_arg(&arguments) {
            Ok(scope) => scope,
            Err(error) => return ToolExecutionResult::tool_error(error),
        };
        let id = match arguments.get("id").and_then(Value::as_str) {
            Some(id) if !id.trim().is_empty() => id,
            _ => return ToolExecutionResult::tool_error("'id' is required"),
        };
        match self.hooks.remove_hook(scope, id) {
            Ok(removed) => ToolExecutionResult::success(json!({
                "ok": true,
                "removed": removed,
                "id": id,
                "scope": scope.as_str(),
                "path": self.hooks.path_for(scope).display().to_string(),
            })),
            Err(error) => {
                ToolExecutionResult::tool_error(format!("could not remove hook: {error}"))
            }
        }
    }
}

fn parse_scope_arg(arguments: &Value) -> std::result::Result<HookScope, String> {
    let scope = arguments
        .get("scope")
        .and_then(Value::as_str)
        .ok_or_else(|| "'scope' is required".to_string())?;
    HookScope::parse(scope).map_err(|error| error.to_string())
}

fn hook_value_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "hook": {
                "type": "object",
                "description": "A UserHookSpec object."
            }
        },
        "required": ["hook"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_in_tmp() -> (tempfile::TempDir, HooksStore) {
        let tmp = tempfile::tempdir().expect("hooks tmp");
        let store = HooksStore::new(tmp.path().join("hooks.json"), tmp.path().join("workspace"));
        (tmp, store)
    }

    #[test]
    fn capability_exposes_unprefixed_hook_tools_without_slash_command() {
        let (_hooks_tmp, hooks) = hooks_in_tmp();
        let capability = HooksCapability {
            hooks: Arc::new(hooks),
        };

        let names = capability
            .tools()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["list_hooks", "validate_hook", "upsert_hook", "remove_hook"]
        );
        assert!(capability.commands().is_empty());
    }

    fn block_git_hook() -> Value {
        json!({
            "id": "block-git",
            "event": "pre_tool_use",
            "matcher": {
                "tool_name": "bash",
                "args_jsonpath": "$.command",
                "match_regex": "(^|[;&|()[:space:]])git([[:space:]]|$)"
            },
            "executor": {
                "type": "bash",
                "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"blocked\"}'"
            },
            "timeout_ms": 1000,
            "on_error": "block",
            "description": "Block git"
        })
    }

    #[tokio::test]
    async fn hook_tools_validate_save_list_and_remove() {
        let (_tmp, hooks) = hooks_in_tmp();
        let hooks = Arc::new(hooks);
        let validate = ValidateHookTool {
            hooks: hooks.clone(),
        };
        let validated = validate.execute(json!({ "hook": block_git_hook() })).await;
        assert!(validated.is_success());

        let upsert = UpsertHookTool {
            hooks: hooks.clone(),
        };
        let saved = upsert
            .execute(json!({ "scope": "global", "hook": block_git_hook() }))
            .await;
        assert!(saved.is_success());

        let list = ListHooksTool {
            hooks: hooks.clone(),
        };
        let listed = list.execute(json!({})).await;
        assert!(listed.is_success());

        let remove = RemoveHookTool {
            hooks: hooks.clone(),
        };
        let removed = remove
            .execute(json!({ "scope": "global", "id": "block-git" }))
            .await;
        assert!(removed.is_success());
        assert!(hooks.effective().hooks.is_empty());
    }
}

use async_trait::async_trait;
use everruns_core::{Capability, CapabilityStatus, Tool, ToolExecutionResult};
use serde_json::{Value, json};

use crate::config::hooks::{HookScope, HooksStore};
use crate::control::{ControlCapability, ControlResponse, ControlRoute};

pub const HOOKS_CAPABILITY_ID: &str = "hooks";

pub struct HooksCapability {
    hooks: HooksStore,
}

impl HooksCapability {
    pub fn new(hooks: HooksStore) -> Self {
        Self { hooks }
    }
}

#[async_trait]
impl Capability for HooksCapability {
    fn id(&self) -> &str {
        HOOKS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Hook Management"
    }

    fn description(&self) -> &str {
        "Manage global and workspace lifecycle hooks through the control plane."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Examples")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        None
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        Vec::new()
    }
}

#[async_trait]
impl ControlCapability for HooksCapability {
    fn control_route(&self) -> ControlRoute {
        ControlRoute {
            resource: HOOKS_CAPABILITY_ID,
            cli_subcommand: "config hooks",
            read_only_operations: &["list", "validate"],
            summary: "global and workspace hook management",
        }
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        let operation = action
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match operation {
            "list" => {
                let effective = self.hooks.effective();
                ToolExecutionResult::success(json!({
                    "hooks": effective.summaries(),
                    "disabled_contributions": effective.disabled_contributions,
                    "paths": {
                        "global": effective.global_path,
                        "workspace": effective.workspace_path,
                    }
                }))
            }
            "validate" => {
                let Some(hook) = action.get("hook") else {
                    return ToolExecutionResult::tool_error("hook is required");
                };
                match self.hooks.validate_hook(hook) {
                    Ok(validated) => ToolExecutionResult::success(json!({
                        "valid": true,
                        "hook": validated.to_validation_json(),
                    })),
                    Err(error) => ToolExecutionResult::tool_error(error.to_string()),
                }
            }
            "upsert" => {
                let scope = match parse_scope(action) {
                    Ok(scope) => scope,
                    Err(error) => return ToolExecutionResult::tool_error(error),
                };
                let Some(hook) = action.get("hook") else {
                    return ToolExecutionResult::tool_error("hook is required");
                };
                match self.hooks.upsert_hook(scope, hook.clone()) {
                    Ok(saved) => ToolExecutionResult::success(json!({
                        "ok": true,
                        "hook": saved.to_summary_json(),
                    })),
                    Err(error) => ToolExecutionResult::tool_error(error.to_string()),
                }
            }
            "remove" => {
                let scope = match parse_scope(action) {
                    Ok(scope) => scope,
                    Err(error) => return ToolExecutionResult::tool_error(error),
                };
                let id = action.get("id").and_then(Value::as_str).unwrap_or_default();
                match self.hooks.remove_hook(scope, id) {
                    Ok(removed) => ToolExecutionResult::success(json!({
                        "ok": true,
                        "id": id,
                        "scope": scope.as_str(),
                        "removed": removed,
                    })),
                    Err(error) => ToolExecutionResult::tool_error(error.to_string()),
                }
            }
            _ => ToolExecutionResult::tool_error(format!(
                "unsupported hooks operation `{operation}`; expected list, validate, upsert, or remove"
            )),
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        response.render_default()
    }
}

fn parse_scope(action: &Value) -> Result<HookScope, String> {
    let raw = action
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("global");
    HookScope::parse(raw).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn hooks_capability_exposes_control_not_agent_tools() {
        let capability = HooksCapability::new(HooksStore::new(
            PathBuf::from("/tmp/yolop-hooks.json"),
            PathBuf::from("/tmp/yolop-workspace"),
        ));
        assert!(capability.tools().is_empty());
        assert_eq!(capability.control_route().resource, "hooks");
        assert_eq!(capability.control_route().cli_subcommand, "config hooks");
    }
}

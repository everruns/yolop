//! Argument-aware narration for the everruns `spawn_agent` tool.
//!
//! `spawn_agent` is assembled by everruns core from the registered delegation
//! targets rather than contributed by a capability, so no capability narration
//! hook covers it and every spawn renders as the generic "Running Spawn Agent".
//! Yolop wraps `SubagentCapability` so its narration hook answers for
//! `spawn_agent` and the transcript says which agent is being spawned.
//!
//! Upstream fixed the root cause in everruns#3277: the act atom now asks the
//! executing tool in the session tool registry for its own narration before the
//! display-name fallback. That is merged but unreleased, and yolop consumes
//! published crates, so this wrapper carries the behavior until the version bump
//! and is deleted then.

use crate::capabilities::narration::narrate_spawn_agent;
use async_trait::async_trait;
use everruns_core::capabilities::{CapabilityLocalization, DelegationTargetProvider, RiskLevel};
use everruns_core::tool_narration::ToolNarrationPhase;
use everruns_core::{Capability, CapabilityStatus, Tool};
use everruns_platform::capabilities::SubagentCapability;
use everruns_provider::{ToolCall, ToolDefinition};
use serde_json::Value;

pub(crate) struct NarratedSubagentCapability {
    inner: SubagentCapability,
}

impl NarratedSubagentCapability {
    pub(crate) fn new() -> Self {
        Self {
            inner: SubagentCapability,
        }
    }
}

#[async_trait]
impl Capability for NarratedSubagentCapability {
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

    fn risk_level(&self) -> RiskLevel {
        self.inner.risk_level()
    }

    fn features(&self) -> Vec<&'static str> {
        self.inner.features()
    }

    fn config_schema(&self) -> Option<Value> {
        self.inner.config_schema()
    }

    fn validate_config(&self, config: &Value) -> Result<(), String> {
        self.inner.validate_config(config)
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        self.inner.system_prompt_addition()
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.inner.tools()
    }

    /// The delegation seam is what actually contributes `spawn_agent`; a
    /// wrapper that forgot it would withhold subagents from the model.
    fn delegation_target_with_config(&self, config: &Value) -> Option<DelegationTargetProvider> {
        self.inner.delegation_target_with_config(config)
    }

    fn narrate(
        &self,
        _tool_def: Option<&ToolDefinition>,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        _locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        (tool_call.name == "spawn_agent").then(|| narrate_spawn_agent(tool_call, phase))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spawn_call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "call-1".to_owned(),
            name: "spawn_agent".to_owned(),
            arguments,
        }
    }

    #[test]
    fn narrates_spawn_agent_with_the_target_agent() {
        let capability = NarratedSubagentCapability::new();
        let narration = capability.narrate(
            None,
            &spawn_call(json!({
                "name": "Orbit Scout",
                "target": { "type": "subagent" },
                "blueprint": "github_scout"
            })),
            ToolNarrationPhase::Started,
            None,
            everruns_core::tool_narration::ToolNarrationContext::default(),
        );
        assert_eq!(
            narration.as_deref(),
            Some("Launching Orbit Scout subagent (github_scout)")
        );
    }

    #[test]
    fn leaves_other_tools_to_their_owners() {
        let capability = NarratedSubagentCapability::new();
        let mut call = spawn_call(json!({}));
        call.name = "read_file".to_owned();
        assert!(
            capability
                .narrate(
                    None,
                    &call,
                    ToolNarrationPhase::Started,
                    None,
                    everruns_core::tool_narration::ToolNarrationContext::default(),
                )
                .is_none()
        );
    }

    /// The wrapper must keep contributing the delegation target, otherwise
    /// `spawn_agent` disappears from the model's tools entirely.
    #[test]
    fn keeps_contributing_the_subagent_delegation_target() {
        let capability = NarratedSubagentCapability::new();
        let target = capability
            .delegation_target_with_config(&json!({}))
            .expect("subagent delegation target");
        assert_eq!(target.target_type, "subagent");
        assert_eq!(target.tool.name(), "spawn_agent");
    }
}

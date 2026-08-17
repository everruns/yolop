//! The `connectors` capability — discover, connect, and disconnect sandbox
//! and integration backends through a uniform tool surface.

use crate::capabilities::narration::stable_labeled;
use crate::connectors::catalog::{ConnectionCatalog, ConnectorInfo};
use crate::connectors::store::ConnectionStore;
use async_trait::async_trait;
use everruns_core::tool_narration::{ToolNarrationPhase, arg_str, truncate};
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{Tool, ToolExecutionResult};
use everruns_platform::ConnectorType;
use everruns_provider::ToolCall;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) const CONNECTORS_CAPABILITY_ID: &str = "connectors";

pub(crate) struct ConnectorsCapability {
    pub(crate) catalog: Arc<ConnectionCatalog>,
    pub(crate) store: Arc<ConnectionStore>,
}

#[async_trait]
impl Capability for ConnectorsCapability {
    fn id(&self) -> &str {
        CONNECTORS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Connectors"
    }

    fn description(&self) -> &str {
        "Connect external sandbox and integration backends (Daytona, …) with a uniform interface."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Integrations")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        // The four tool descriptions cover discovery, setup, and credential
        // management. What they cannot say is what a *connected* provider unlocks,
        // because the `daytona_*` tools belong to another capability — so
        // contribute that routing hint, and only once Daytona is actually
        // connected. Nothing connected means nothing to route to, and the block
        // costs zero.
        self.store.is_connected("daytona").then(|| {
            format!(
                "<capability id=\"{id}\">\n\
                 Daytona is connected: use the `daytona_*` tools to create isolated sandboxes \
                 and outsource risky or heavy work away from the host workspace. Host `bash` \
                 and file tools still operate on the local workspace.\n\
                 </capability>",
                id = self.id()
            )
        })
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        let shared = ConnectorTools {
            catalog: self.catalog.clone(),
            store: self.store.clone(),
        };
        vec![
            Box::new(ListConnectorsTool {
                catalog: self.catalog.clone(),
                store: self.store.clone(),
            }),
            Box::new(GetConnectorTool {
                catalog: self.catalog.clone(),
                store: self.store.clone(),
            }),
            Box::new(ConnectTool {
                inner: shared.clone(),
            }),
            Box::new(DisconnectTool {
                catalog: self.catalog.clone(),
                store: self.store.clone(),
            }),
        ]
    }
}

#[derive(Clone)]
struct ConnectorTools {
    catalog: Arc<ConnectionCatalog>,
    store: Arc<ConnectionStore>,
}

fn connector_json_with_path(info: &ConnectorInfo, path: &str) -> Value {
    json!({
        "provider": info.provider_id,
        "display_name": info.display_name,
        "description": info.description,
        "icon": info.icon,
        "connection_type": info.connection_type,
        "connected": info.connected,
        "form_schema": info.form_schema,
        "storage_path": path,
    })
}

struct ListConnectorsTool {
    catalog: Arc<ConnectionCatalog>,
    store: Arc<ConnectionStore>,
}

#[async_trait]
impl Tool for ListConnectorsTool {
    fn narrate(
        &self,
        _tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = None;
        Some(stable_labeled("List connectors", detail, phase))
    }

    fn name(&self) -> &str {
        "list_connectors"
    }
    fn display_name(&self) -> Option<&str> {
        Some("List connectors")
    }
    fn description(&self) -> &str {
        "List available connector providers (Daytona, …) with connection status and form hints."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": false })
    }
    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        let path = self.store.path().display().to_string();
        let connectors: Vec<Value> = self
            .catalog
            .list_connectors(&self.store)
            .iter()
            .map(|info| connector_json_with_path(info, &path))
            .collect();
        ToolExecutionResult::success(json!({
            "connectors": connectors,
            "count": connectors.len(),
            "storage_path": path,
        }))
    }
}

struct GetConnectorTool {
    catalog: Arc<ConnectionCatalog>,
    store: Arc<ConnectionStore>,
}

#[async_trait]
impl Tool for GetConnectorTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = arg_str(&tool_call.arguments, &["provider"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Get connector", detail, phase))
    }

    fn name(&self) -> &str {
        "get_connector"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Get connector")
    }
    fn description(&self) -> &str {
        "Describe one connector provider: setup instructions, form fields, and whether it is connected."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "description": "Connector provider id, e.g. `daytona`."
                }
            },
            "required": ["provider"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let provider = match arguments.get("provider").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.trim(),
            _ => {
                return ToolExecutionResult::tool_error("Missing required parameter: provider");
            }
        };
        let Some(entry) = self.catalog.get(provider) else {
            return ToolExecutionResult::tool_error(format!("Unknown connector `{provider}`"));
        };
        let info = self.catalog.connector_info(entry, &self.store);
        ToolExecutionResult::success(connector_json_with_path(
            &info,
            &self.store.path().display().to_string(),
        ))
    }
}

struct ConnectTool {
    inner: ConnectorTools,
}

#[async_trait]
impl Tool for ConnectTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = arg_str(&tool_call.arguments, &["provider"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Connect provider", detail, phase))
    }

    fn name(&self) -> &str {
        "connect"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Connect")
    }
    fn description(&self) -> &str {
        "Validate and save connector credentials. For API-key providers pass `fields.api_key`. \
         Credentials are stored locally and never echoed back."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "description": "Connector provider id, e.g. `daytona`."
                },
                "fields": {
                    "type": "object",
                    "description": "Provider form fields (typically `{ \"api_key\": \"...\" }`).",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["provider", "fields"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let provider = match arguments.get("provider").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                return ToolExecutionResult::tool_error("Missing required parameter: provider");
            }
        };
        let fields_value = match arguments.get("fields") {
            Some(v) if v.is_object() => v,
            _ => {
                return ToolExecutionResult::tool_error(
                    "Missing required parameter: fields (object of form field names to values)",
                );
            }
        };
        let mut fields = HashMap::new();
        if let Some(obj) = fields_value.as_object() {
            for (key, value) in obj {
                let Some(text) = value.as_str() else {
                    return ToolExecutionResult::tool_error(format!(
                        "Field `{key}` must be a string"
                    ));
                };
                if text.trim().is_empty() {
                    continue;
                }
                fields.insert(key.clone(), text.to_string());
            }
        }
        if fields.is_empty() {
            return ToolExecutionResult::tool_error(
                "fields must include at least one non-empty value",
            );
        }

        let Some(entry) = self.inner.catalog.get(&provider) else {
            return ToolExecutionResult::tool_error(format!("Unknown connector `{provider}`"));
        };
        if entry.connection_type() == ConnectorType::OAuth {
            return ToolExecutionResult::tool_error(format!(
                "Connector `{provider}` uses OAuth and cannot be configured through this tool yet"
            ));
        }

        match self
            .inner
            .catalog
            .validate_and_store(&self.inner.store, &provider, fields)
            .await
        {
            Ok(validation) => ToolExecutionResult::success(json!({
                "provider": provider,
                "connected": true,
                "provider_username": validation.provider_username,
                "storage_path": self.inner.store.path().display().to_string(),
            })),
            Err(message) => ToolExecutionResult::tool_error(message),
        }
    }
}

struct DisconnectTool {
    catalog: Arc<ConnectionCatalog>,
    store: Arc<ConnectionStore>,
}

#[async_trait]
impl Tool for DisconnectTool {
    fn narrate(
        &self,
        tool_call: &ToolCall,
        phase: ToolNarrationPhase,
        locale: Option<&str>,
        _ctx: everruns_core::tool_narration::ToolNarrationContext<'_>,
    ) -> Option<String> {
        let _ = locale;
        let detail = arg_str(&tool_call.arguments, &["provider"]).map(|value| truncate(value, 48));
        Some(stable_labeled("Disconnect provider", detail, phase))
    }

    fn name(&self) -> &str {
        "disconnect"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Disconnect")
    }
    fn description(&self) -> &str {
        "Remove stored credentials for a connector provider. Does not delete remote resources."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "description": "Connector provider id, e.g. `daytona`."
                }
            },
            "required": ["provider"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let provider = match arguments.get("provider").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p.trim(),
            _ => {
                return ToolExecutionResult::tool_error("Missing required parameter: provider");
            }
        };
        if self.catalog.get(provider).is_none() {
            return ToolExecutionResult::tool_error(format!("Unknown connector `{provider}`"));
        }
        match self.store.clear(provider) {
            Ok(existed) => ToolExecutionResult::success(json!({
                "provider": provider,
                "connected": false,
                "cleared": existed,
            })),
            Err(err) => {
                ToolExecutionResult::internal_error_msg(format!("disconnect failed: {err}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_narration_names_action_and_provider() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        let catalog = Arc::new(ConnectionCatalog::with_defaults());
        let tool = GetConnectorTool { catalog, store };
        let call = ToolCall {
            id: "call-1".into(),
            name: "get_connector".into(),
            arguments: json!({ "provider": "daytona" }),
        };

        assert_eq!(
            tool.narrate(
                &call,
                ToolNarrationPhase::Started,
                None,
                everruns_core::tool_narration::ToolNarrationContext::default(),
            ),
            Some("Get connector: daytona".into())
        );
    }

    #[tokio::test]
    async fn list_connectors_includes_daytona() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        let catalog = Arc::new(ConnectionCatalog::with_defaults());
        let tool = ListConnectorsTool { catalog, store };
        let result = tool.execute(json!({})).await;
        assert!(result.is_success(), "{result:?}");
        match result {
            everruns_core::tools::ToolExecutionResult::Success(value) => {
                let names: Vec<&str> = value["connectors"]
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|c| c["provider"].as_str())
                    .collect();
                assert!(names.contains(&"daytona"));
            }
            other => panic!("expected success, got {other:?}"),
        }
    }
}

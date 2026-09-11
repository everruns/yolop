//! External connector credentials (Daytona, …).
//!
//! The management surface is CLI-only: `yolop connectors ...` owns every
//! mutation, and the control route below serves read-only inspection. No
//! model-invoked tools remain.

use crate::connectors::catalog::{ConnectionCatalog, ConnectorInfo};
use crate::connectors::store::ConnectionStore;
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches, Command};
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext, ToolExecutionResult};
use everruns_platform::ConnectorType;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) const CONNECTORS_CAPABILITY_ID: &str = "connectors";

pub(crate) const CONNECTORS_CONTROL_ROUTE: ControlRoute = ControlRoute {
    resource: CONNECTORS_CAPABILITY_ID,
    cli_subcommand: "connectors",
    read_only_operations: &["list", "get"],
    summary: "Inspect and manage external connector credentials. Reads are served inline; connect and disconnect run through `yolop connectors ...`.",
};

#[derive(Clone)]
pub(crate) struct ConnectorsCapability {
    catalog: Arc<ConnectionCatalog>,
    store: Arc<ConnectionStore>,
}

impl ConnectorsCapability {
    pub(crate) fn new(catalog: Arc<ConnectionCatalog>, store: Arc<ConnectionStore>) -> Self {
        Self { catalog, store }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum ConnectorsAction {
    List,
    Get {
        provider: String,
    },
    Connect {
        provider: String,
        fields: HashMap<String, String>,
    },
    Disconnect {
        provider: String,
    },
}

impl ConnectorsCapability {
    async fn execute_action(&self, action: &ConnectorsAction) -> ToolExecutionResult {
        match action {
            ConnectorsAction::List => {
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
            ConnectorsAction::Get { provider } => {
                let provider = provider.trim();
                if provider.is_empty() {
                    return ToolExecutionResult::tool_error("Missing required parameter: provider");
                }
                let Some(entry) = self.catalog.get(provider) else {
                    return ToolExecutionResult::tool_error(format!(
                        "Unknown connector `{provider}`"
                    ));
                };
                let info = self.catalog.connector_info(entry, &self.store);
                ToolExecutionResult::success(connector_json_with_path(
                    &info,
                    &self.store.path().display().to_string(),
                ))
            }
            ConnectorsAction::Connect { provider, fields } => {
                let provider = provider.trim();
                if provider.is_empty() {
                    return ToolExecutionResult::tool_error("Missing required parameter: provider");
                }
                let Some(entry) = self.catalog.get(provider) else {
                    return ToolExecutionResult::tool_error(format!(
                        "Unknown connector `{provider}`"
                    ));
                };
                if entry.connection_type() == ConnectorType::OAuth {
                    return ToolExecutionResult::tool_error(format!(
                        "Connector `{provider}` uses OAuth and cannot be configured through the CLI yet"
                    ));
                }
                match self
                    .catalog
                    .validate_and_store(&self.store, provider, fields.clone())
                    .await
                {
                    Ok(validation) => ToolExecutionResult::success(json!({
                        "provider": provider,
                        "connected": true,
                        "provider_username": validation.provider_username,
                        "storage_path": self.store.path().display().to_string(),
                    })),
                    Err(message) => ToolExecutionResult::tool_error(message),
                }
            }
            ConnectorsAction::Disconnect { provider } => {
                let provider = provider.trim();
                if provider.is_empty() {
                    return ToolExecutionResult::tool_error("Missing required parameter: provider");
                }
                if self.catalog.get(provider).is_none() {
                    return ToolExecutionResult::tool_error(format!(
                        "Unknown connector `{provider}`"
                    ));
                }
                match self.store.clear(provider) {
                    Ok(existed) => ToolExecutionResult::success(json!({
                        "disconnected": true,
                        "provider": provider,
                        "removed": existed,
                    })),
                    Err(err) => ToolExecutionResult::tool_error(err.to_string()),
                }
            }
        }
    }
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
        // Only Daytona currently has runtime behavior worth routing; without it
        // connected this whole capability costs zero prompt tokens.
        if !self.store.is_connected("daytona") {
            return None;
        }
        let id = self.id();
        Some(format!(
            "<capability id=\"{id}\">\n\
            The `daytona` connector is active: prefer the `daytona_*` tools over raw shell for \
            sandbox work. Manage credentials with `yolop connectors ...` (foreground Bash).\n\
            </capability>",
        ))
    }

    fn tools(&self) -> Vec<Box<dyn everruns_core::Tool>> {
        // Fully CLI-driven (`yolop connectors ...`); no model-invoked tools remain.
        Vec::new()
    }
}

#[async_trait]
impl ControlCapability for ConnectorsCapability {
    fn control_route(&self) -> ControlRoute {
        CONNECTORS_CONTROL_ROUTE
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        let parsed: ConnectorsAction = match serde_json::from_value(action.clone()) {
            Ok(action) => action,
            Err(err) => {
                return ToolExecutionResult::tool_error(format!(
                    "invalid connectors action: {err}"
                ));
            }
        };
        match &parsed {
            ConnectorsAction::List | ConnectorsAction::Get { .. } => {
                return self.execute_action(&parsed).await;
            }
            ConnectorsAction::Connect { .. } | ConnectorsAction::Disconnect { .. } => {
                ToolExecutionResult::tool_error(
                    "connect and disconnect run through `yolop connectors ...` so credentials stay on the human-driven path",
                )
            }
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        response.render_default()
    }
}

#[async_trait]
impl CliCapability for ConnectorsCapability {
    fn cli_command(&self) -> Command {
        Command::new(CONNECTORS_CONTROL_ROUTE.cli_subcommand)
            .about("Inspect and manage external connector credentials")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                Command::new("list").about("List connector providers with connection status"),
            )
            .subcommand(
                Command::new("get")
                    .about("Show setup instructions and form fields for one provider")
                    .arg(Arg::new("provider").required(true)),
            )
            .subcommand(
                Command::new("connect")
                    .about("Validate and store credentials for one provider")
                    .arg(Arg::new("provider").required(true))
                    .arg(
                        Arg::new("field")
                            .long("field")
                            .action(ArgAction::Append)
                            .value_names(["KEY=VALUE"])
                            .help("Credential field; repeat per field"),
                    ),
            )
            .subcommand(
                Command::new("disconnect")
                    .about("Remove stored credentials for one provider")
                    .arg(Arg::new("provider").required(true)),
            )
    }

    fn control_request_from_cli(&self, matches: &ArgMatches) -> anyhow::Result<ControlRequest> {
        let action = match matches.subcommand() {
            Some(("list", _)) => ConnectorsAction::List,
            Some(("get", args)) => ConnectorsAction::Get {
                provider: args
                    .get_one::<String>("provider")
                    .cloned()
                    .unwrap_or_default(),
            },
            Some(("connect", args)) => {
                let mut fields = HashMap::new();
                for raw in args.get_many::<String>("field").into_iter().flatten() {
                    let Some((key, value)) = raw.split_once('=') else {
                        anyhow::bail!("--field must look like KEY=VALUE, got `{raw}`");
                    };
                    fields.insert(key.trim().to_string(), value.trim().to_string());
                }
                ConnectorsAction::Connect {
                    provider: args
                        .get_one::<String>("provider")
                        .cloned()
                        .unwrap_or_default(),
                    fields,
                }
            }
            Some(("disconnect", args)) => ConnectorsAction::Disconnect {
                provider: args
                    .get_one::<String>("provider")
                    .cloned()
                    .unwrap_or_default(),
            },
            _ => anyhow::bail!("unknown connectors command"),
        };
        Ok(ControlRequest::new(CONNECTORS_CAPABILITY_ID, action)?)
    }

    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        let action: ConnectorsAction = serde_json::from_value(request.action.clone())?;
        let response = ControlResponse::from_tool_result(self.execute_action(&action).await);
        let rendered = self.render_control(&serde_json::to_value(&action)?, &response);
        println!("{rendered}");
        if !response.ok {
            anyhow::bail!("connectors command failed");
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_capability() -> (tempfile::TempDir, ConnectorsCapability) {
        let tmp = tempfile::tempdir().expect("tmp");
        let capability = ConnectorsCapability::new(
            Arc::new(ConnectionCatalog::with_defaults()),
            Arc::new(ConnectionStore::open(tmp.path().join("connections.toml"))),
        );
        (tmp, capability)
    }

    #[test]
    fn tools_exposes_no_model_tools() {
        let (_tmp, capability) = test_capability();
        assert!(capability.tools().is_empty());
    }

    #[test]
    fn control_route_points_at_connectors_cli() {
        let (_tmp, capability) = test_capability();
        let route = capability.control_route();
        assert_eq!(route.resource, "connectors");
        assert_eq!(route.cli_subcommand, "connectors");
        assert_eq!(route.read_only_operations, &["list", "get"]);
    }

    #[tokio::test]
    async fn control_list_reports_providers() {
        let (_tmp, capability) = test_capability();
        let response = capability
            .execute_action(&ConnectorsAction::List)
            .await;
        let ToolExecutionResult::Success(value) = response else {
            panic!("expected JSON success, got {response:?}");
        };
        assert!(value["connectors"].is_array());
        assert!(value["count"].as_u64().unwrap_or(0) >= 1);
    }

    #[tokio::test]
    async fn control_get_rejects_unknown_provider() {
        let (_tmp, capability) = test_capability();
        let response = capability
            .execute_action(&ConnectorsAction::Get {
                provider: "nope".to_string(),
            })
            .await;
        let ToolExecutionResult::ToolError(message) = response else {
            panic!("expected error, got {response:?}");
        };
        assert!(message.contains("Unknown connector"), "{message}");
    }

    #[tokio::test]
    async fn control_rejects_mutations_toward_cli() {
        let (_tmp, capability) = test_capability();
        let response = capability
            .execute_control(&json!({
                "operation": "connect",
                "provider": "daytona",
                "fields": {},
            }))
            .await;
        let ToolExecutionResult::ToolError(message) = response else {
            panic!("expected error, got {response:?}");
        };
        assert!(message.contains("yolop connectors"), "{message}");
    }

    #[test]
    fn cli_parses_connect_fields() {
        let (_tmp, capability) = test_capability();
        let matches = capability
            .cli_command()
            .try_get_matches_from([
                "connectors",
                "connect",
                "daytona",
                "--field",
                "api_key=secret",
            ])
            .expect("cli parses");
        let request = capability
            .control_request_from_cli(&matches)
            .expect("control request");
        let action: ConnectorsAction = serde_json::from_value(request.action.clone())
            .expect("round-trips through the control request");
        let ConnectorsAction::Connect { provider, fields } = action else {
            panic!("expected connect, got {action:?}");
        };
        assert_eq!(provider, "daytona");
        assert_eq!(fields.get("api_key").map(String::as_str), Some("secret"));
    }
}

use std::sync::Arc;

use async_trait::async_trait;
use clap::{Arg, ArgMatches, Command};
use everruns_core::{Capability, CapabilityStatus, ToolExecutionResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::host::SetupController;
use super::model_list::{ModelListAction, ModelListCapability};
use crate::config::SettingsStore;
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};

const MODEL_ROUTE: &str = "model";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ModelAction {
    Show,
    Use { target: String },
}

pub(crate) struct ModelCliCapability {
    settings: Option<Arc<SettingsStore>>,
    model_list: Option<Arc<ModelListCapability>>,
    controller: Option<SetupController>,
}

impl ModelCliCapability {
    pub(crate) fn detached(settings: Arc<SettingsStore>) -> Self {
        Self {
            settings: Some(settings),
            model_list: None,
            controller: None,
        }
    }

    pub(crate) fn live(model_list: Arc<ModelListCapability>, controller: SetupController) -> Self {
        Self {
            settings: None,
            model_list: Some(model_list),
            controller: Some(controller),
        }
    }

    /// Terminal-native show for headless `yolop model`: reports the saved
    /// default provider and model from settings. There is no live session
    /// here, so this is the persisted default, not a session override.
    fn detached_show(&self) -> ToolExecutionResult {
        let Some(settings) = self.settings.as_deref() else {
            return ToolExecutionResult::tool_error(
                "model status is unavailable: settings store is not configured",
            );
        };
        let snapshot = settings.snapshot();
        let provider = snapshot
            .default_provider
            .clone()
            .unwrap_or_else(|| "<unset>".to_string());
        let model = snapshot
            .default_models
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| "<default>".to_string());
        ToolExecutionResult::Success(json!({ "message": format!("{provider}/{model}") }))
    }

    fn command() -> Command {
        Command::new(MODEL_ROUTE)
            .about("Show or switch the current session model")
            .after_help("Examples:\n  Switch this session to the configured model labeled review:\n    yolop model use review\n\n  Temporarily use a high-effort model without changing defaults:\n    yolop model use openai/gpt-5.4:high")
            .subcommand(
                Command::new("use")
                    .about("Switch the current session model without changing defaults")
                    .arg(Arg::new("target").required(true)),
            )
    }

    fn action(matches: &ArgMatches) -> anyhow::Result<ModelAction> {
        Ok(match matches.subcommand() {
            None => ModelAction::Show,
            Some(("use", matches)) => ModelAction::Use {
                target: matches
                    .get_one::<String>("target")
                    .expect("target is required")
                    .clone(),
            },
            Some((action, _)) => anyhow::bail!("unsupported model action: {action}"),
        })
    }
}

#[async_trait]
impl Capability for ModelCliCapability {
    fn id(&self) -> &str {
        MODEL_ROUTE
    }

    fn name(&self) -> &str {
        "Current Model"
    }

    fn description(&self) -> &str {
        "The model selected for the current session."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
}

#[async_trait]
impl ControlCapability for ModelCliCapability {
    fn control_route(&self) -> ControlRoute {
        ControlRoute {
            resource: MODEL_ROUTE,
            cli_subcommand: MODEL_ROUTE,
            read_only_operations: &["show"],
            summary: "the current session model",
        }
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        let action = match serde_json::from_value::<ModelAction>(action.clone()) {
            Ok(action) => action,
            Err(error) => return ToolExecutionResult::tool_error(error.to_string()),
        };
        match action {
            ModelAction::Show => {
                if let Some(controller) = &self.controller {
                    let choice = controller.current_choice();
                    let message = format!("{}/{}", choice.provider_name(), choice.model_id());
                    return ToolExecutionResult::Success(json!({ "message": message }));
                }
                self.detached_show()
            }
            ModelAction::Use { target } => {
                if let Some(model_list) = &self.model_list {
                    return model_list
                        .execute_action(&ModelListAction::Use { model: target })
                        .await;
                }
                ToolExecutionResult::tool_error(
                    "model use switches the current session model; outside a session, set the default with `yolop config model set <provider> <model>`",
                )
            }
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        if let Some(message) = response
            .value
            .as_ref()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
        {
            return message.to_owned();
        }
        if let Some(using) = response
            .value
            .as_ref()
            .and_then(|value| value.get("using"))
            .and_then(Value::as_str)
        {
            if let Some(detail) = response
                .value
                .as_ref()
                .and_then(|value| value.get("detail"))
                .and_then(Value::as_str)
            {
                return format!("{using}\n{detail}");
            }
            return using.to_owned();
        }
        response
            .error
            .as_deref()
            .unwrap_or("model command failed")
            .to_owned()
    }
}

#[async_trait]
impl CliCapability for ModelCliCapability {
    fn cli_command(&self) -> Command {
        Self::command()
    }

    fn control_request_from_cli(&self, matches: &ArgMatches) -> anyhow::Result<ControlRequest> {
        Ok(ControlRequest::new(
            MODEL_ROUTE,
            serde_json::to_value(Self::action(matches)?)?,
        )?)
    }

    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        let response =
            ControlResponse::from_tool_result(self.execute_control(&request.action).await);
        let rendered = self.render_control(&request.action, &response);
        if response.ok {
            println!("{rendered}");
            Ok(())
        } else {
            anyhow::bail!(rendered)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_model() {
        let matches = ModelCliCapability::command()
            .try_get_matches_from([MODEL_ROUTE])
            .expect("bare model should parse");
        assert_eq!(
            ModelCliCapability::action(&matches).unwrap(),
            ModelAction::Show
        );
    }

    #[test]
    fn parses_model_use_target() {
        let matches = ModelCliCapability::command()
            .try_get_matches_from([MODEL_ROUTE, "use", "openai/gpt-5"])
            .expect("model use should parse");
        assert_eq!(
            ModelCliCapability::action(&matches).unwrap(),
            ModelAction::Use {
                target: "openai/gpt-5".to_owned(),
            }
        );
    }

    fn test_settings() -> Arc<SettingsStore> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::mem::forget(dir);
        Arc::new(SettingsStore::open(path))
    }

    #[test]
    fn cli_request_targets_the_singular_control_route() {
        let capability = ModelCliCapability::detached(test_settings());
        let matches = ModelCliCapability::command()
            .try_get_matches_from([MODEL_ROUTE, "use", "openai/gpt-5"])
            .expect("model use should parse");
        let request = capability
            .control_request_from_cli(&matches)
            .expect("CLI request should be valid");

        assert_eq!(request.resource, MODEL_ROUTE);
        assert_eq!(
            serde_json::from_value::<ModelAction>(request.action).unwrap(),
            ModelAction::Use {
                target: "openai/gpt-5".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn detached_show_reports_saved_default() {
        let settings = test_settings();
        settings
            .set_default_provider(Some("openai".to_string()))
            .expect("default provider");
        let capability = ModelCliCapability::detached(settings);

        let show = capability
            .execute_control(&serde_json::to_value(ModelAction::Show).unwrap())
            .await;
        let ToolExecutionResult::Success(value) = show else {
            panic!("expected detached show success");
        };
        assert_eq!(
            value.get("message").and_then(Value::as_str),
            Some("openai/<default>")
        );
    }

    #[tokio::test]
    async fn detached_use_guides_to_config_command() {
        let capability = ModelCliCapability::detached(test_settings());

        let use_model = capability
            .execute_control(
                &serde_json::to_value(ModelAction::Use {
                    target: "openai/gpt-5".to_owned(),
                })
                .unwrap(),
            )
            .await;
        let ToolExecutionResult::ToolError(message) = use_model else {
            panic!("expected detached use error");
        };
        assert!(
            message.contains("yolop config model set"),
            "message: {message}"
        );
    }

    #[test]
    fn model_use_response_renders_the_selected_model_and_detail() {
        let capability = ModelCliCapability::detached(test_settings());
        let response = ControlResponse::from_tool_result(ToolExecutionResult::Success(json!({
            "using": "review",
            "detail": "setup provider changed: OpenAI gpt-5.6-terra (current session only)"
        })));

        assert_eq!(
            capability.render_control(&json!({"action": "use"}), &response),
            "review\nsetup provider changed: OpenAI gpt-5.6-terra (current session only)"
        );
    }
}

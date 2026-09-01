use std::sync::Arc;

use async_trait::async_trait;
use clap::{Arg, ArgMatches, Command};
use everruns_core::{Capability, CapabilityStatus, ToolExecutionResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::host::SetupController;
use super::model_list::{ModelListAction, ModelListCapability};
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
    model_list: Option<Arc<ModelListCapability>>,
    controller: Option<SetupController>,
}

impl ModelCliCapability {
    pub(crate) fn detached() -> Self {
        Self {
            model_list: None,
            controller: None,
        }
    }

    pub(crate) fn live(model_list: Arc<ModelListCapability>, controller: SetupController) -> Self {
        Self {
            model_list: Some(model_list),
            controller: Some(controller),
        }
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
                let Some(controller) = &self.controller else {
                    return ToolExecutionResult::tool_error("model requires an attached session");
                };
                let choice = controller.current_choice();
                let message = format!("{}/{}", choice.provider_name(), choice.model_id());
                ToolExecutionResult::Success(json!({ "message": message }))
            }
            ModelAction::Use { target } => {
                let Some(model_list) = &self.model_list else {
                    return ToolExecutionResult::tool_error(
                        "model use requires an attached session",
                    );
                };
                model_list
                    .execute_action(&ModelListAction::Use { model: target })
                    .await
            }
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        response
            .value
            .as_ref()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| response.error.as_deref().unwrap_or("model command failed"))
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

    #[test]
    fn cli_request_targets_the_singular_control_route() {
        let capability = ModelCliCapability::detached();
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
    async fn detached_control_refuses_live_model_operations() {
        let capability = ModelCliCapability::detached();

        let show = capability
            .execute_control(&serde_json::to_value(ModelAction::Show).unwrap())
            .await;
        assert!(show.is_error(), "{show:?}");

        let use_model = capability
            .execute_control(
                &serde_json::to_value(ModelAction::Use {
                    target: "openai/gpt-5".to_owned(),
                })
                .unwrap(),
            )
            .await;
        assert!(use_model.is_error(), "{use_model:?}");
    }
}

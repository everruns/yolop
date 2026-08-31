use async_trait::async_trait;
use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::host::SetupController;
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::tui::host_ui::{HostUi, UiCommand};
use everruns_core::{Capability, CapabilityStatus, ToolExecutionResult};
use std::sync::Arc;

pub(crate) const SETUP_CONTROL_ROUTE: &str = "setup";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum SetupAction {
    Guided,
    Status,
    Login { provider: String },
    Reauthenticate { provider: String },
}

pub(crate) struct SetupCliCapability {
    controller: Option<SetupController>,
    ui: Option<Arc<dyn HostUi>>,
}

impl SetupCliCapability {
    pub(crate) fn detached() -> Self {
        Self {
            controller: None,
            ui: None,
        }
    }

    pub(crate) fn live(controller: SetupController, ui: Option<Arc<dyn HostUi>>) -> Self {
        Self {
            controller: Some(controller),
            ui,
        }
    }

    fn command() -> Command {
        Command::new(SETUP_CONTROL_ROUTE)
            .about("Set up provider authentication")
            .subcommand(Command::new("status").about("Show setup status"))
            .subcommand(
                Command::new("login")
                    .about("Log in to a provider")
                    .arg(Arg::new("provider").required(true)),
            )
            .subcommand(
                Command::new("reauthenticate")
                    .about("Log in to a provider again")
                    .arg(Arg::new("provider").required(true)),
            )
    }

    fn action(matches: &ArgMatches) -> anyhow::Result<SetupAction> {
        Ok(match matches.subcommand() {
            None => SetupAction::Guided,
            Some(("status", _)) => SetupAction::Status,
            Some(("login", matches)) => SetupAction::Login {
                provider: matches
                    .get_one::<String>("provider")
                    .expect("provider is required")
                    .clone(),
            },
            Some(("reauthenticate", matches)) => SetupAction::Reauthenticate {
                provider: matches
                    .get_one::<String>("provider")
                    .expect("provider is required")
                    .clone(),
            },
            Some((action, _)) => anyhow::bail!("unsupported setup action: {action}"),
        })
    }

    fn open_setup(&self, provider: Option<String>, reauthenticate: bool) -> ToolExecutionResult {
        let Some(ui) = &self.ui else {
            return ToolExecutionResult::tool_error(
                "setup requires an interactive attached session",
            );
        };
        ui.send(UiCommand::OpenSetup {
            provider,
            reauthenticate,
        });
        ToolExecutionResult::Success(json!({ "message": "setup opened" }))
    }
}

#[async_trait]
impl CliCapability for SetupCliCapability {
    fn cli_command(&self) -> Command {
        Self::command()
    }

    fn control_request_from_cli(&self, matches: &ArgMatches) -> anyhow::Result<ControlRequest> {
        Ok(ControlRequest::new(
            SETUP_CONTROL_ROUTE,
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

#[async_trait]
impl Capability for SetupCliCapability {
    fn id(&self) -> &str {
        SETUP_CONTROL_ROUTE
    }

    fn name(&self) -> &str {
        "Setup"
    }

    fn description(&self) -> &str {
        "Provider setup and authentication."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
}

#[async_trait]
impl ControlCapability for SetupCliCapability {
    fn control_route(&self) -> ControlRoute {
        ControlRoute {
            resource: SETUP_CONTROL_ROUTE,
            cli_subcommand: SETUP_CONTROL_ROUTE,
            read_only_operations: &["status"],
            summary: "provider setup and authentication",
        }
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        let action = match serde_json::from_value::<SetupAction>(action.clone()) {
            Ok(action) => action,
            Err(error) => return ToolExecutionResult::tool_error(error.to_string()),
        };
        match action {
            SetupAction::Guided => self.open_setup(None, false),
            SetupAction::Status => {
                let Some(controller) = &self.controller else {
                    return ToolExecutionResult::tool_error("setup requires an attached session");
                };
                let result = controller.status_result();
                ToolExecutionResult::Success(json!({ "message": result.message }))
            }
            SetupAction::Login { provider } => self.open_setup(Some(provider), false),
            SetupAction::Reauthenticate { provider } => self.open_setup(Some(provider), true),
        }
    }

    fn render_control(&self, _action: &Value, response: &ControlResponse) -> String {
        response
            .value
            .as_ref()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| response.render_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingUi {
        commands: Mutex<Vec<UiCommand>>,
    }

    impl HostUi for RecordingUi {
        fn send(&self, command: UiCommand) {
            self.commands.lock().expect("commands lock").push(command);
        }

        fn request(&self, command: UiCommand) -> tokio::sync::oneshot::Receiver<Vec<String>> {
            self.send(command);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }
    }
    fn parse(args: &[&str]) -> SetupAction {
        let matches = SetupCliCapability::command()
            .try_get_matches_from(args)
            .unwrap();
        SetupCliCapability::action(&matches).unwrap()
    }

    #[test]
    fn bare_setup_opens_guided_setup_and_status_reports_status() {
        assert_eq!(parse(&["setup"]), SetupAction::Guided);
        assert_eq!(parse(&["setup", "status"]), SetupAction::Status);
    }

    #[test]
    fn login_and_reauthenticate_capture_provider() {
        assert_eq!(
            parse(&["setup", "login", "codex"]),
            SetupAction::Login {
                provider: "codex".to_string()
            }
        );
        assert_eq!(
            parse(&["setup", "reauthenticate", "codex"]),
            SetupAction::Reauthenticate {
                provider: "codex".to_string()
            }
        );
    }

    #[test]
    fn login_requires_provider() {
        assert!(
            SetupCliCapability::command()
                .try_get_matches_from(["setup", "login"])
                .is_err()
        );
    }

    #[tokio::test]
    async fn guided_and_provider_actions_reuse_setup_ui() {
        let ui = Arc::new(RecordingUi::default());
        let capability = SetupCliCapability {
            controller: None,
            ui: Some(ui.clone()),
        };

        let guided = capability
            .execute_control(&serde_json::to_value(SetupAction::Guided).unwrap())
            .await;
        let login = capability
            .execute_control(
                &serde_json::to_value(SetupAction::Login {
                    provider: "openai".to_owned(),
                })
                .unwrap(),
            )
            .await;
        let reauthenticate = capability
            .execute_control(
                &serde_json::to_value(SetupAction::Reauthenticate {
                    provider: "anthropic".to_owned(),
                })
                .unwrap(),
            )
            .await;

        assert!(matches!(guided, ToolExecutionResult::Success(_)));
        assert!(matches!(login, ToolExecutionResult::Success(_)));
        assert!(matches!(reauthenticate, ToolExecutionResult::Success(_)));
        let commands = ui.commands.lock().expect("commands lock");
        assert!(matches!(
            commands[0],
            UiCommand::OpenSetup {
                provider: None,
                reauthenticate: false
            }
        ));
        assert!(matches!(
            &commands[1],
            UiCommand::OpenSetup { provider: Some(provider), reauthenticate: false } if provider == "openai"
        ));
        assert!(matches!(
            &commands[2],
            UiCommand::OpenSetup { provider: Some(provider), reauthenticate: true } if provider == "anthropic"
        ));
    }

    #[tokio::test]
    async fn detached_execution_requires_live_session() {
        let result = SetupCliCapability::detached()
            .execute_control(&serde_json::to_value(SetupAction::Status).unwrap())
            .await;
        assert!(matches!(result, ToolExecutionResult::ToolError { .. }));
    }
}

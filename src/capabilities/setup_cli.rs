use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::host::SetupController;
use crate::auth::{codex, openrouter};
use crate::config::SettingsStore;
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::runtime::{Provider, SUPPORTED_PROVIDERS};
use crate::tui::host_ui::{HostUi, UiCommand};
use everruns_core::{Capability, CapabilityStatus, ToolExecutionResult};
use std::io::{IsTerminal, Write};
use std::sync::Arc;

pub(crate) const SETUP_CONTROL_ROUTE: &str = "setup";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum SetupAction {
    Guided,
    Status,
    Login {
        provider: String,
        #[serde(default)]
        device: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    Reauthenticate {
        provider: String,
        #[serde(default)]
        device: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
}

pub(crate) struct SetupCliCapability {
    settings: Option<Arc<SettingsStore>>,
    controller: Option<SetupController>,
    ui: Option<Arc<dyn HostUi>>,
}

impl SetupCliCapability {
    pub(crate) fn detached(settings: Arc<SettingsStore>) -> Self {
        Self {
            settings: Some(settings),
            controller: None,
            ui: None,
        }
    }

    pub(crate) fn live(controller: SetupController, ui: Option<Arc<dyn HostUi>>) -> Self {
        Self {
            settings: None,
            controller: Some(controller),
            ui,
        }
    }

    fn command() -> Command {
        Command::new(SETUP_CONTROL_ROUTE)
            .about("Set up provider authentication")
            .after_help("Examples:\n  Replace an expired OpenAI credential:\n    yolop setup reauthenticate openai\n\n  Authenticate Codex using its device-code flow:\n    yolop setup login codex --device\n  yolop setup login openai --api-key $OPENAI_API_KEY\n  yolop setup reauthenticate anthropic")
            .subcommand(Command::new("status").about("Show setup status"))
            .subcommand(
                Command::new("login")
                    .about("Authenticate a provider in the terminal (no setup screen)")
                    .arg(
                        Arg::new("provider")
                            .help("Provider to authenticate (codex, openai, anthropic, openrouter, ...)")
                            .required(true),
                    )
                    .arg(
                        Arg::new("device")
                            .long("device")
                            .help("Use device authorization instead of opening a browser (codex)")
                            .action(ArgAction::SetTrue),
                    )
                    .arg(
                        Arg::new("api-key")
                            .long("api-key")
                            .help("API key for key-based providers; avoids the interactive prompt")
                            .action(ArgAction::Set),
                    ),
            )
            .subcommand(
                Command::new("reauthenticate")
                    .about("Replace the saved credential for a provider in the terminal")
                    .arg(Arg::new("provider").help("Provider to reauthenticate").required(true))
                    .arg(
                        Arg::new("device")
                            .long("device")
                            .help("Use device authorization instead of opening a browser (codex)")
                            .action(ArgAction::SetTrue),
                    )
                    .arg(
                        Arg::new("api-key")
                            .long("api-key")
                            .help("API key for key-based providers; avoids the interactive prompt")
                            .action(ArgAction::Set),
                    ),
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
                device: matches.get_flag("device"),
                api_key: matches.get_one::<String>("api-key").cloned(),
            },
            Some(("reauthenticate", matches)) => SetupAction::Reauthenticate {
                provider: matches
                    .get_one::<String>("provider")
                    .expect("provider is required")
                    .clone(),
                device: matches.get_flag("device"),
                api_key: matches.get_one::<String>("api-key").cloned(),
            },
            Some((action, _)) => anyhow::bail!("unsupported setup action: {action}"),
        })
    }

    fn open_setup(&self, provider: Option<String>, reauthenticate: bool) -> ToolExecutionResult {
        let Some(ui) = &self.ui else {
            return ToolExecutionResult::tool_error(
                "setup requires an interactive attached session; run `yolop` without --prompt for the setup screen, or `yolop setup login <provider>` for headless login",
            );
        };
        ui.send(UiCommand::OpenSetup {
            provider,
            reauthenticate,
        });
        ToolExecutionResult::Success(json!({ "message": "setup opened" }))
    }

    fn detached_settings(&self) -> Result<&SettingsStore, ToolExecutionResult> {
        self.settings.as_deref().ok_or_else(|| {
            ToolExecutionResult::tool_error(
                "setup status is unavailable: settings store is not configured",
            )
        })
    }

    /// Terminal-native status for headless `yolop setup status`. Mirrors the
    /// attached `SetupController::status_result` shape but reports the saved
    /// default as the current provider: there is no live session here.
    fn detached_status(&self) -> ToolExecutionResult {
        let settings = match self.detached_settings() {
            Ok(settings) => settings,
            Err(err) => return err,
        };
        let snapshot = settings.snapshot();
        let saved = snapshot
            .default_provider
            .clone()
            .unwrap_or_else(|| "<unset>".to_string());
        let model = snapshot
            .default_models
            .get(&saved)
            .cloned()
            .unwrap_or_else(|| "<default>".to_string());
        let mut lines = vec![
            format!("provider={saved} saved={saved} model={model}"),
            format!(
                "attribution={} approval={}",
                on_off(snapshot.attribution),
                snapshot.approval_mode
            ),
        ];
        for provider in SUPPORTED_PROVIDERS {
            let stored = if *provider == "codex" {
                snapshot.has_codex_auth()
            } else {
                snapshot.tokens.contains_key(*provider)
            };
            lines.push(format!(
                "{provider}: stored={} env={}",
                on_off(stored),
                on_off(detached_env_present(provider))
            ));
        }
        ToolExecutionResult::Success(json!({ "message": lines.join("\n") }))
    }

    /// Terminal-native login for headless `yolop setup login <provider>`.
    /// Browser and device OAuth flows print to stdout and block there instead
    /// of opening the setup overlay.
    async fn detached_login(
        &self,
        provider_raw: &str,
        device: bool,
        api_key: Option<String>,
    ) -> ToolExecutionResult {
        let settings = match self.detached_settings() {
            Ok(settings) => settings,
            Err(err) => return err,
        };
        let normalized = provider_raw.trim().to_lowercase();
        let Some(provider) = Provider::from_name(&normalized) else {
            return ToolExecutionResult::tool_error(format!(
                "unknown provider '{provider_raw}'. Supported providers: {}",
                SUPPORTED_PROVIDERS.join(", ")
            ));
        };
        let name = provider.as_str();
        match provider {
            Provider::Codex => {
                if let Some(key) = api_key {
                    let key = key.trim().to_string();
                    if key.is_empty() {
                        return ToolExecutionResult::tool_error("no API key entered for codex");
                    }
                    let auth = codex::auth_from_access_token(key);
                    if let Err(err) = settings.set_codex_auth(auth) {
                        return ToolExecutionResult::tool_error(format!(
                            "failed to save Codex credentials: {err}"
                        ));
                    }
                } else if device {
                    let pending = match codex::start_device_login().await {
                        Ok(pending) => pending,
                        Err(err) => {
                            return ToolExecutionResult::tool_error(format!(
                                "failed to start Codex device login: {err}"
                            ));
                        }
                    };
                    println!(
                        "Open {} and enter code {}",
                        pending.verification_uri, pending.user_code
                    );
                    let auth = match codex::complete_device_login(pending).await {
                        Ok(auth) => auth,
                        Err(err) => {
                            return ToolExecutionResult::tool_error(format!(
                                "Codex device login failed: {err}"
                            ));
                        }
                    };
                    if let Err(err) = settings.set_codex_auth(auth) {
                        return ToolExecutionResult::tool_error(format!(
                            "failed to save Codex credentials: {err}"
                        ));
                    }
                } else {
                    println!("Opening browser for Codex login...");
                    let auth = match codex::login_with_browser().await {
                        Ok(auth) => auth,
                        Err(err) => {
                            return ToolExecutionResult::tool_error(format!(
                                "Codex browser login failed: {err}. Retry with `yolop setup login codex --device`."
                            ));
                        }
                    };
                    if let Err(err) = settings.set_codex_auth(auth) {
                        return ToolExecutionResult::tool_error(format!(
                            "failed to save Codex credentials: {err}"
                        ));
                    }
                }
                if let Err(err) = settings.set_default_provider(Some(name.to_string())) {
                    return ToolExecutionResult::tool_error(format!(
                        "Codex login succeeded but saving the default provider failed: {err}"
                    ));
                }
                ToolExecutionResult::Success(
                    json!({ "message": "logged in to codex; default provider is now codex" }),
                )
            }
            Provider::OpenAi | Provider::Anthropic | Provider::Meta | Provider::Google => {
                let key = match api_key {
                    Some(key) => {
                        let key = key.trim().to_string();
                        if key.is_empty() {
                            return ToolExecutionResult::tool_error(format!(
                                "no API key entered for {name}"
                            ));
                        }
                        key
                    }
                    None => match prompt_for_api_key(name) {
                        Ok(key) => key,
                        Err(err) => return ToolExecutionResult::tool_error(err),
                    },
                };
                if let Err(err) = settings.set_token(name.to_string(), key) {
                    return ToolExecutionResult::tool_error(format!(
                        "failed to save {name} API key: {err}"
                    ));
                }
                if let Err(err) = settings.set_default_provider(Some(name.to_string())) {
                    return ToolExecutionResult::tool_error(format!(
                        "{name} key saved but setting the default provider failed: {err}"
                    ));
                }
                ToolExecutionResult::Success(json!({ "message": format!(
                    "saved {name} API key; default provider is now {name}"
                ) }))
            }
            Provider::OpenRouter => {
                if let Some(key) = api_key {
                    let key = key.trim().to_string();
                    if key.is_empty() {
                        return ToolExecutionResult::tool_error(
                            "no API key entered for openrouter",
                        );
                    }
                    if let Err(err) = settings.set_token(name.to_string(), key) {
                        return ToolExecutionResult::tool_error(format!(
                            "failed to save openrouter API key: {err}"
                        ));
                    }
                } else {
                    println!("Opening browser for OpenRouter login...");
                    let api_key = match openrouter::login_with_browser().await {
                        Ok(api_key) => api_key,
                        Err(err) => {
                            return ToolExecutionResult::tool_error(format!(
                                "OpenRouter browser login failed: {err}. Retry with `yolop setup login openrouter --api-key <key>`."
                            ));
                        }
                    };
                    if let Err(err) = settings.set_token(name.to_string(), api_key) {
                        return ToolExecutionResult::tool_error(format!(
                            "failed to save openrouter API key: {err}"
                        ));
                    }
                }
                if let Err(err) = settings.set_default_provider(Some(name.to_string())) {
                    return ToolExecutionResult::tool_error(format!(
                        "openrouter login succeeded but setting the default provider failed: {err}"
                    ));
                }
                ToolExecutionResult::Success(
                    json!({ "message": "logged in to openrouter; default provider is now openrouter" }),
                )
            }
            Provider::Ollama | Provider::Local | Provider::Custom | Provider::Sim => {
                ToolExecutionResult::Success(json!({ "message": format!(
                    "provider '{name}' needs no login; it is ready to use"
                ) }))
            }
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Env-var side of the detached status rows. Same mapping as
/// `model_discovery::provider_env_present`, duplicated so the headless setup
/// path does not depend on the TUI overlay.
fn detached_env_present(provider: &str) -> bool {
    let names: &[&str] = match provider {
        "openai" => &["OPENAI_API_KEY"],
        "codex" => &["CODEX_ACCESS_TOKEN"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "meta" => &["MODEL_API_KEY"],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "ollama" => &["OLLAMA_BASE_URL", "OLLAMA_API_KEY"],
        "custom" => &["CUSTOM_API_KEY"],
        _ => &[],
    };
    names.iter().any(|name| {
        std::env::var(name)
            .map(|value| !value.is_empty())
            .unwrap_or(false)
    })
}

fn prompt_for_api_key(provider: &str) -> Result<String, String> {
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "no API key provided for {provider}; pass --api-key <key> or run interactively"
        ));
    }
    print!("Enter API key for {provider}: ");
    std::io::stdout()
        .flush()
        .map_err(|err| format!("failed to prompt for {provider} API key: {err}"))?;
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|err| format!("failed to read {provider} API key: {err}"))?;
    let trimmed = input.trim().to_string();
    if trimmed.is_empty() {
        return Err(format!("no API key entered for {provider}"));
    }
    Ok(trimmed)
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
            SetupAction::Guided => {
                if self.ui.is_some() {
                    self.open_setup(None, false)
                } else {
                    let status = self.detached_status();
                    match status {
                        ToolExecutionResult::Success(value) => {
                            let message = value
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            ToolExecutionResult::Success(json!({ "message": format!(
                                "{message}\n\nRun `yolop setup login <provider>` to authenticate."
                            ) }))
                        }
                        other => other,
                    }
                }
            }
            SetupAction::Status => {
                if let Some(controller) = &self.controller {
                    let result = controller.status_result();
                    return ToolExecutionResult::Success(json!({ "message": result.message }));
                }
                self.detached_status()
            }
            SetupAction::Login {
                provider,
                device,
                api_key,
            } => {
                if self.ui.is_some() {
                    self.open_setup(Some(provider), false)
                } else {
                    self.detached_login(&provider, device, api_key).await
                }
            }
            SetupAction::Reauthenticate {
                provider,
                device,
                api_key,
            } => {
                if self.ui.is_some() {
                    self.open_setup(Some(provider), true)
                } else {
                    self.detached_login(&provider, device, api_key).await
                }
            }
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
                provider: "codex".to_string(),
                device: false,
                api_key: None,
            }
        );
        assert_eq!(
            parse(&["setup", "reauthenticate", "codex"]),
            SetupAction::Reauthenticate {
                provider: "codex".to_string(),
                device: false,
                api_key: None,
            }
        );
        assert_eq!(
            parse(&["setup", "login", "codex", "--device"]),
            SetupAction::Login {
                provider: "codex".to_string(),
                device: true,
                api_key: None,
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
            settings: None,
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
                    device: false,
                    api_key: None,
                })
                .unwrap(),
            )
            .await;
        let reauthenticate = capability
            .execute_control(
                &serde_json::to_value(SetupAction::Reauthenticate {
                    provider: "anthropic".to_owned(),
                    device: false,
                    api_key: None,
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

    fn test_settings() -> Arc<SettingsStore> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::mem::forget(dir);
        Arc::new(SettingsStore::open(path))
    }

    #[tokio::test]
    async fn detached_status_reports_saved_default() {
        let settings = test_settings();
        settings
            .set_default_provider(Some("openai".to_string()))
            .expect("default provider");
        settings
            .set_token("openai".to_string(), "sk-test".to_string())
            .expect("token");
        let result = SetupCliCapability::detached(settings)
            .execute_control(&serde_json::to_value(SetupAction::Status).unwrap())
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected status success");
        };
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(message.contains("provider=openai"), "message: {message}");
        assert!(message.contains("openai: stored=on"), "message: {message}");
    }

    #[tokio::test]
    async fn detached_guided_shows_status_with_hint() {
        let result = SetupCliCapability::detached(test_settings())
            .execute_control(&serde_json::to_value(SetupAction::Guided).unwrap())
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected guided status success");
        };
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            message.contains("yolop setup login <provider>"),
            "message: {message}"
        );
    }

    #[tokio::test]
    async fn detached_login_rejects_unknown_provider() {
        let result = SetupCliCapability::detached(test_settings())
            .execute_control(
                &serde_json::to_value(SetupAction::Login {
                    provider: "nope".to_string(),
                    device: false,
                    api_key: None,
                })
                .unwrap(),
            )
            .await;
        assert!(matches!(result, ToolExecutionResult::ToolError { .. }));
    }

    #[tokio::test]
    async fn detached_token_login_saves_without_prompt() {
        let settings = test_settings();
        let result = SetupCliCapability::detached(settings.clone())
            .execute_control(
                &serde_json::to_value(SetupAction::Login {
                    provider: "openai".to_string(),
                    device: false,
                    api_key: Some("sk-headless".to_string()),
                })
                .unwrap(),
            )
            .await;
        assert!(matches!(result, ToolExecutionResult::Success(_)));
        let snapshot = settings.snapshot();
        assert_eq!(snapshot.default_provider.as_deref(), Some("openai"));
        assert!(snapshot.tokens.contains_key("openai"));
    }

    #[tokio::test]
    async fn detached_local_provider_needs_no_login() {
        let result = SetupCliCapability::detached(test_settings())
            .execute_control(
                &serde_json::to_value(SetupAction::Login {
                    provider: "llmsim".to_string(),
                    device: false,
                    api_key: None,
                })
                .unwrap(),
            )
            .await;
        let ToolExecutionResult::Success(value) = result else {
            panic!("expected no-login success");
        };
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(message.contains("needs no login"), "message: {message}");
    }
}

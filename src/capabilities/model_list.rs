//! The model list: one ordered, cross-provider menu of the models a user
//! actually switches between.
//!
//! Providers publish hundreds of models. Before this capability, `/model`
//! showed one provider's whole catalog and ACP was offered every discovered
//! model of every credentialed provider, which is a catalog, not a choice. The
//! list is the choice: `[[models]]` in settings (see
//! [`crate::config::model_list`]), served to the `/model` picker, the status
//! bar, and ACP's `session/set_config_option` model options.
//!
//! Administration follows the attached-CLI pattern extensions and session
//! coordination use (`knowledge/specs/conversational-control.md`): `yolop
//! models ...` rather than model tools, so the agent can edit the list on
//! request without the schemas costing prompt budget every turn. `use` reaches
//! the live session through [`SetupController::change_provider`], the same call
//! `/setup provider <name> <model>` makes, so switching provider and model
//! together is atomic and has one implementation.

use crate::capabilities::host::SetupController;
use crate::capabilities::model_discovery::provider_is_usable;
use crate::config::SettingsStore;
use crate::config::model_list::{ModelEntry, default_models};
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use crate::runtime::SUPPORTED_PROVIDERS;
use async_trait::async_trait;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use everruns_core::{Capability, CapabilityStatus, ToolExecutionResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const MODEL_LIST_CAPABILITY_ID: &str = "model_list";

/// A constant so the shared control-plane prompt block can be measured against
/// the always-on prompt budget without constructing a session.
pub(crate) const MODEL_LIST_CONTROL_ROUTE: ControlRoute = ControlRoute {
    resource: "models",
    cli_subcommand: "models",
    read_only_operations: &["list"],
    summary: "show, reorder, and edit the model list, and switch this session to one of its models",
};

#[derive(Parser, Debug)]
#[command(
    name = "models",
    about = "Show and edit the model list yolop offers when switching models"
)]
struct ModelsCommandLine {
    #[command(subcommand)]
    command: ModelsCliCommand,
}

#[derive(Subcommand, Debug)]
enum ModelsCliCommand {
    /// Show the model list.
    List {
        /// Emit the full machine-readable result.
        #[arg(long)]
        json: bool,
        /// Show only entries whose provider has a credential.
        #[arg(long)]
        connected: bool,
    },
    /// Add a model to the list.
    Add(AddArgs),
    /// Remove a model from the list.
    Rm {
        /// `provider/model`, `provider:model`, or a model id unique in the list.
        model: String,
    },
    /// Move a model to a different position (1-based).
    Move {
        /// `provider/model`, `provider:model`, or a model id unique in the list.
        model: String,
        /// New 1-based position.
        position: usize,
    },
    /// Switch this session to a model on the list, provider included.
    Use {
        /// `provider/model`, `provider:model`, or a model id unique in the list.
        model: String,
    },
    /// Restore the built-in default list.
    Reset,
}

#[derive(Args, Debug)]
struct AddArgs {
    /// Provider the model is reached through.
    provider: String,
    /// Provider-relative model id (`gpt-5.6-sol`, `anthropic/claude-opus-4-8`).
    model: String,
    /// Reasoning effort to apply with this entry.
    #[arg(long)]
    effort: Option<String>,
    /// Display name shown instead of `provider/model`.
    #[arg(long)]
    label: Option<String>,
    /// 1-based position to insert at (default: end of the list).
    #[arg(long)]
    position: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum ModelListAction {
    List {
        json: bool,
        /// Show only entries whose provider has a credential. The listing
        /// otherwise shows every entry, marking the ones needing sign-in:
        /// hiding a row from the command that edits it is how a user loses
        /// track of their own list. The credential filter belongs to the
        /// *offered* menu (`offered_models`), not to this listing.
        connected: bool,
    },
    Add {
        provider: String,
        model: String,
        effort: Option<String>,
        label: Option<String>,
        position: Option<usize>,
    },
    Rm {
        model: String,
    },
    Move {
        model: String,
        position: usize,
    },
    Use {
        model: String,
    },
    Reset,
}

impl ModelListAction {
    fn request(&self) -> ControlRequest {
        ControlRequest::new(MODEL_LIST_CONTROL_ROUTE.resource, self)
            .expect("model list action serializes")
    }
}

impl From<ModelsCliCommand> for ModelListAction {
    fn from(command: ModelsCliCommand) -> Self {
        match command {
            ModelsCliCommand::List { json, connected } => Self::List { json, connected },
            ModelsCliCommand::Add(args) => Self::Add {
                provider: args.provider,
                model: args.model,
                effort: args.effort,
                label: args.label,
                position: args.position,
            },
            ModelsCliCommand::Rm { model } => Self::Rm { model },
            ModelsCliCommand::Move { model, position } => Self::Move { model, position },
            ModelsCliCommand::Use { model } => Self::Use { model },
            ModelsCliCommand::Reset => Self::Reset,
        }
    }
}

pub(crate) struct ModelListCapability {
    settings: Arc<SettingsStore>,
    /// Present only when the capability runs inside a live session. The
    /// detached CLI can edit the list (an ordinary global-settings write) but
    /// cannot switch a session that does not exist.
    controller: Option<SetupController>,
}

impl ModelListCapability {
    pub(crate) fn new(settings: Arc<SettingsStore>, controller: Option<SetupController>) -> Self {
        Self {
            settings,
            controller,
        }
    }

    /// Resolve a user-supplied model reference against the list.
    ///
    /// Accepts `provider/model`, `provider:model`, or a bare model id when it
    /// names exactly one entry — ambiguity is an error rather than a guess,
    /// because the whole point of the list is that one model can be reachable
    /// through more than one provider.
    fn find(models: &[ModelEntry], reference: &str) -> Result<usize, String> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err("name a model from the list; run `yolop models list`".to_string());
        }
        let qualified = |entry: &ModelEntry| {
            let key = entry.key();
            reference == key || reference == format!("{}/{}", entry.provider, entry.model)
        };
        if let Some(index) = models.iter().position(qualified) {
            return Ok(index);
        }
        let matches: Vec<usize> = models
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.model == reference)
            .map(|(index, _)| index)
            .collect();
        match matches.as_slice() {
            [index] => Ok(*index),
            [] => Err(format!(
                "`{reference}` is not on the model list; run `yolop models list`, or add it with `yolop models add <provider> {reference}`"
            )),
            _ => {
                let providers: Vec<&str> = matches
                    .iter()
                    .map(|index| models[*index].provider.as_str())
                    .collect();
                Err(format!(
                    "`{reference}` is on the list for {}; qualify it as `<provider>/{reference}`",
                    providers.join(" and ")
                ))
            }
        }
    }

    fn entry_json(&self, entry: &ModelEntry, index: usize, usable: bool, current: bool) -> Value {
        json!({
            "position": index + 1,
            "provider": entry.provider,
            "model": entry.model,
            "effort": entry.effort,
            "label": entry.label,
            "display": entry.display(),
            "connected": usable,
            "current": current,
        })
    }

    fn current_key(&self) -> Option<String> {
        let controller = self.controller.as_ref()?;
        let choice = controller.current_choice();
        Some(format!("{}:{}", choice.provider_name(), choice.model_id()))
    }

    fn list_value(&self, all: bool) -> Value {
        let settings = self.settings.snapshot();
        let current = self.current_key();
        let models = settings.model_list();
        let entries: Vec<Value> = models
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let usable = provider_is_usable(&settings, &entry.provider);
                let is_current = current.as_deref() == Some(entry.key().as_str());
                (entry, index, usable, is_current)
            })
            .filter(|(_, _, usable, is_current)| all || *usable || *is_current)
            .map(|(entry, index, usable, is_current)| {
                self.entry_json(entry, index, usable, is_current)
            })
            .collect();
        json!({
            "models": entries,
            "configured": !settings.models.is_empty(),
            "hidden": models.len() - entries.len(),
        })
    }

    /// Read the list, change it, write the whole thing back. Every mutation
    /// goes through here so ordering is explicit and there is one write path.
    fn write(&self, models: Vec<ModelEntry>) -> Result<(), String> {
        self.settings
            .set_models(models)
            .map_err(|err| format!("could not save the model list: {err}"))
    }

    pub(crate) async fn execute_action(&self, action: &ModelListAction) -> ToolExecutionResult {
        let mut models = self.settings.snapshot().model_list();
        match action {
            ModelListAction::List { connected, .. } => {
                ToolExecutionResult::Success(self.list_value(!*connected))
            }
            ModelListAction::Add {
                provider,
                model,
                effort,
                label,
                position,
            } => {
                let provider = provider.trim();
                if !SUPPORTED_PROVIDERS.contains(&provider) {
                    return ToolExecutionResult::tool_error(format!(
                        "unknown provider `{provider}`; expected one of {}",
                        SUPPORTED_PROVIDERS.join(", ")
                    ));
                }
                let model = model.trim();
                if model.is_empty() {
                    return ToolExecutionResult::tool_error("model id is required");
                }
                let entry = ModelEntry {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    effort: effort.clone(),
                    label: label.clone(),
                };
                // Adding a model already on the list updates it in place rather
                // than growing a duplicate row in the picker.
                if let Some(existing) = models.iter_mut().find(|item| item.key() == entry.key()) {
                    *existing = entry.clone();
                } else {
                    let at = position
                        .map(|position| position.saturating_sub(1).min(models.len()))
                        .unwrap_or(models.len());
                    models.insert(at, entry.clone());
                }
                if let Err(error) = self.write(models) {
                    return ToolExecutionResult::tool_error(error);
                }
                ToolExecutionResult::Success(json!({
                    "added": entry.display(),
                    "models": self.list_value(true)["models"],
                }))
            }
            ModelListAction::Rm { model } => {
                let index = match Self::find(&models, model) {
                    Ok(index) => index,
                    Err(error) => return ToolExecutionResult::tool_error(error),
                };
                let removed = models.remove(index);
                if let Err(error) = self.write(models) {
                    return ToolExecutionResult::tool_error(error);
                }
                ToolExecutionResult::Success(json!({
                    "removed": removed.display(),
                    "models": self.list_value(true)["models"],
                }))
            }
            ModelListAction::Move { model, position } => {
                let index = match Self::find(&models, model) {
                    Ok(index) => index,
                    Err(error) => return ToolExecutionResult::tool_error(error),
                };
                if *position == 0 || *position > models.len() {
                    return ToolExecutionResult::tool_error(format!(
                        "position must be between 1 and {}",
                        models.len()
                    ));
                }
                let entry = models.remove(index);
                models.insert(position - 1, entry.clone());
                if let Err(error) = self.write(models) {
                    return ToolExecutionResult::tool_error(error);
                }
                ToolExecutionResult::Success(json!({
                    "moved": entry.display(),
                    "position": position,
                    "models": self.list_value(true)["models"],
                }))
            }
            ModelListAction::Use { model } => {
                let index = match Self::find(&models, model) {
                    Ok(index) => index,
                    Err(error) => return ToolExecutionResult::tool_error(error),
                };
                let entry = &models[index];
                let Some(controller) = &self.controller else {
                    return ToolExecutionResult::tool_error(
                        "switching models requires a running Yolop session; invoke `yolop models use` through that session's foreground Bash",
                    );
                };
                // One implementation: this is exactly what `/setup provider
                // <name> <model> [effort]` does, so provider and model switch
                // together and persist the same way.
                let result = controller
                    .change_provider(&format!("{} {}", entry.provider, entry.spec()))
                    .await;
                match result {
                    Ok(command) if command.success => ToolExecutionResult::Success(json!({
                        "using": entry.display(),
                        "detail": command.message,
                    })),
                    Ok(command) => ToolExecutionResult::tool_error(command.message),
                    Err(error) => ToolExecutionResult::tool_error(error.to_string()),
                }
            }
            ModelListAction::Reset => {
                if let Err(error) = self.write(Vec::new()) {
                    return ToolExecutionResult::tool_error(error);
                }
                ToolExecutionResult::Success(json!({
                    "reset": true,
                    "models": self.list_value(true)["models"],
                }))
            }
        }
    }
}

#[async_trait]
impl Capability for ModelListCapability {
    fn id(&self) -> &str {
        MODEL_LIST_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Model List"
    }
    fn description(&self) -> &str {
        "The ordered, cross-provider list of models this session offers and switches between."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Examples")
    }
}

#[async_trait]
impl ControlCapability for ModelListCapability {
    fn control_route(&self) -> ControlRoute {
        MODEL_LIST_CONTROL_ROUTE
    }
    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        match serde_json::from_value::<ModelListAction>(action.clone()) {
            Ok(action) => self.execute_action(&action).await,
            Err(error) => {
                ToolExecutionResult::tool_error(format!("invalid model list action: {error}"))
            }
        }
    }
    fn render_control(&self, action: &Value, response: &ControlResponse) -> String {
        serde_json::from_value::<ModelListAction>(action.clone())
            .map(|action| render_response(&action, response))
            .unwrap_or_else(|_| response.render_default())
    }
}

#[async_trait]
impl CliCapability for ModelListCapability {
    fn cli_command(&self) -> clap::Command {
        ModelsCommandLine::command()
    }
    fn control_request_from_cli(
        &self,
        matches: &clap::ArgMatches,
    ) -> anyhow::Result<ControlRequest> {
        let parsed = ModelsCommandLine::from_arg_matches(matches)?;
        Ok(ModelListAction::from(parsed.command).request())
    }
    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        let action = serde_json::from_value::<ModelListAction>(request.action.clone())?;
        // Editing the list is a global-settings write and works detached;
        // only switching the live session needs a session to switch.
        if matches!(action, ModelListAction::Use { .. }) {
            anyhow::bail!(
                "switching models requires a running Yolop session; invoke the command directly through that session's foreground Bash"
            );
        }
        let response = ControlResponse::from_tool_result(self.execute_action(&action).await);
        let rendered = self.render_control(&request.action, &response);
        if response.ok {
            println!("{rendered}");
            Ok(())
        } else {
            anyhow::bail!(rendered)
        }
    }
}

fn render_response(action: &ModelListAction, response: &ControlResponse) -> String {
    if !response.ok {
        return response.render_default();
    }
    let value = response.value.as_ref().unwrap_or(&Value::Null);
    if let ModelListAction::List { json: true, .. } = action {
        return response.render_default();
    }
    let Some(models) = value.get("models").and_then(Value::as_array) else {
        return response.render_default();
    };
    let mut lines = Vec::new();
    match action {
        ModelListAction::List { .. } => {}
        ModelListAction::Add { .. } => lines.push(format!(
            "added {}",
            value["added"].as_str().unwrap_or("model")
        )),
        ModelListAction::Rm { .. } => lines.push(format!(
            "removed {}",
            value["removed"].as_str().unwrap_or("model")
        )),
        ModelListAction::Move { .. } => lines.push(format!(
            "moved {} to {}",
            value["moved"].as_str().unwrap_or("model"),
            value["position"].as_u64().unwrap_or_default()
        )),
        ModelListAction::Use { .. } => {
            return format!("using {}", value["using"].as_str().unwrap_or("model"));
        }
        ModelListAction::Reset => lines.push("restored the default model list".to_string()),
    }
    if models.is_empty() {
        lines.push(
            "No models listed. Add one with `yolop models add <provider> <model>`.".to_string(),
        );
        return lines.join("\n");
    }
    for entry in models {
        let marker = if entry["current"].as_bool().unwrap_or(false) {
            "*"
        } else {
            " "
        };
        let note = if entry["connected"].as_bool().unwrap_or(true) {
            String::new()
        } else {
            "  (no credential)".to_string()
        };
        lines.push(format!(
            "{marker} {:>2}. {}{note}",
            entry["position"].as_u64().unwrap_or_default(),
            entry["display"].as_str().unwrap_or("?"),
        ));
    }
    if let ModelListAction::List { .. } = action
        && !value["configured"].as_bool().unwrap_or(false)
    {
        lines
            .push("(built-in default list; `yolop models add|rm|move` makes it yours)".to_string());
    }
    lines.join("\n")
}

/// The list as the pickers and ACP should offer it: configured order, entries
/// whose provider has no credential dropped, and the currently selected model
/// kept even when its provider looks unusable (dropping the model in use would
/// make the picker disagree with the status bar).
pub(crate) fn offered_models(
    settings: &crate::config::Settings,
    current: Option<&str>,
) -> Vec<ModelEntry> {
    offer(settings.model_list(), current, |provider| {
        provider_is_usable(settings, provider)
    })
}

/// The credential filter itself, over an injected usability predicate so it is
/// testable without the ambient provider environment deciding the outcome.
fn offer(
    models: Vec<ModelEntry>,
    current: Option<&str>,
    usable: impl Fn(&str) -> bool,
) -> Vec<ModelEntry> {
    let offered: Vec<ModelEntry> = models
        .into_iter()
        .filter(|entry| usable(&entry.provider) || current == Some(entry.key().as_str()))
        .collect();
    // A user whose only credential is for a provider nobody listed would
    // otherwise face an empty picker; fall back to the default menu's entries
    // they can actually reach before giving up.
    if offered.is_empty() {
        return default_models()
            .into_iter()
            .filter(|entry| usable(&entry.provider))
            .collect();
    }
    offered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability() -> (tempfile::TempDir, ModelListCapability) {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = Arc::new(SettingsStore::open(dir.path().join("settings.toml")));
        (dir, ModelListCapability::new(settings, None))
    }

    fn saved(capability: &ModelListCapability) -> Vec<ModelEntry> {
        capability.settings.snapshot().model_list()
    }

    async fn run(capability: &ModelListCapability, action: ModelListAction) -> Value {
        match capability.execute_action(&action).await {
            ToolExecutionResult::Success(value) => value,
            other => panic!("expected success, got {other:?}"),
        }
    }

    async fn error(capability: &ModelListCapability, action: ModelListAction) -> String {
        match capability.execute_action(&action).await {
            ToolExecutionResult::ToolError(error) => error,
            other => panic!("expected a tool error, got {other:?}"),
        }
    }

    fn add(provider: &str, model: &str, position: Option<usize>) -> ModelListAction {
        ModelListAction::Add {
            provider: provider.to_string(),
            model: model.to_string(),
            effort: None,
            label: None,
            position,
        }
    }

    #[tokio::test]
    async fn editing_an_unconfigured_list_materializes_the_defaults_first() {
        let (_dir, capability) = capability();
        assert!(
            capability.settings.snapshot().models.is_empty(),
            "nothing configured yet"
        );

        run(&capability, add("openrouter", "some/model", None)).await;

        let models = saved(&capability);
        assert_eq!(
            models.first(),
            Some(&ModelEntry::new("openai", "gpt-5.6-sol")),
            "the default list is kept, not replaced by the first edit: {models:?}"
        );
        assert_eq!(
            models.last(),
            Some(&ModelEntry::new("openrouter", "some/model"))
        );
    }

    #[tokio::test]
    async fn add_inserts_at_a_position_and_updates_in_place_rather_than_duplicating() {
        let (_dir, capability) = capability();
        run(&capability, add("openrouter", "some/model", Some(1))).await;
        assert_eq!(
            saved(&capability)[0],
            ModelEntry::new("openrouter", "some/model")
        );

        let before = saved(&capability).len();
        run(
            &capability,
            ModelListAction::Add {
                provider: "openrouter".to_string(),
                model: "some/model".to_string(),
                effort: Some("high".to_string()),
                label: None,
                position: None,
            },
        )
        .await;
        let models = saved(&capability);
        assert_eq!(models.len(), before, "re-adding must not duplicate a row");
        assert_eq!(
            models[0].effort.as_deref(),
            Some("high"),
            "it updates in place"
        );
    }

    #[tokio::test]
    async fn rm_and_move_reorder_the_list_and_reset_restores_the_defaults() {
        let (_dir, capability) = capability();
        run(&capability, add("openrouter", "some/model", None)).await;

        run(
            &capability,
            ModelListAction::Move {
                model: "openrouter/some/model".to_string(),
                position: 1,
            },
        )
        .await;
        assert_eq!(saved(&capability)[0].provider, "openrouter");

        run(
            &capability,
            ModelListAction::Rm {
                model: "openrouter/some/model".to_string(),
            },
        )
        .await;
        assert!(
            !saved(&capability)
                .iter()
                .any(|entry| entry.provider == "openrouter")
        );

        run(&capability, add("openrouter", "some/model", None)).await;
        run(&capability, ModelListAction::Reset).await;
        assert!(
            capability.settings.snapshot().models.is_empty(),
            "reset clears the stored list so the defaults apply again"
        );
        assert_eq!(saved(&capability), default_models());
    }

    #[tokio::test]
    async fn a_move_past_the_end_is_refused_with_the_valid_range() {
        let (_dir, capability) = capability();
        let message = error(
            &capability,
            ModelListAction::Move {
                model: "openai/gpt-5.6-sol".to_string(),
                position: 99,
            },
        )
        .await;
        assert!(message.contains("between 1 and"), "{message}");
    }

    #[tokio::test]
    async fn switching_models_without_a_session_says_so_instead_of_failing_silently() {
        let (_dir, capability) = capability();
        let message = error(
            &capability,
            ModelListAction::Use {
                model: "openai/gpt-5.6-sol".to_string(),
            },
        )
        .await;
        assert!(message.contains("running Yolop session"), "{message}");
    }

    #[tokio::test]
    async fn the_listing_shows_entries_that_still_need_sign_in() {
        let (_dir, capability) = capability();
        let value = run(
            &capability,
            ModelListAction::List {
                json: false,
                connected: false,
            },
        )
        .await;
        let listed = value["models"].as_array().expect("models");
        assert_eq!(
            listed.len(),
            default_models().len(),
            "the editing view hides nothing: {listed:?}"
        );
        assert_eq!(value["configured"], false);
    }

    fn list() -> Vec<ModelEntry> {
        vec![
            ModelEntry::new("openai", "gpt-5.6-sol"),
            ModelEntry::new("anthropic", "claude-opus-4-8"),
            ModelEntry::new("openrouter", "anthropic/claude-opus-4-8"),
        ]
    }

    #[test]
    fn a_reference_resolves_by_qualified_name_or_unique_model_id() {
        let models = list();
        assert_eq!(
            ModelListCapability::find(&models, "openai/gpt-5.6-sol"),
            Ok(0)
        );
        assert_eq!(
            ModelListCapability::find(&models, "anthropic:claude-opus-4-8"),
            Ok(1)
        );
        assert_eq!(ModelListCapability::find(&models, "gpt-5.6-sol"), Ok(0));
    }

    #[test]
    fn the_same_model_on_two_providers_must_be_qualified() {
        // `claude-opus-4-8` is reachable directly and through OpenRouter; the
        // list exists to distinguish those, so a bare id must not guess.
        let models = vec![
            ModelEntry::new("anthropic", "claude-opus-4-8"),
            ModelEntry::new("openrouter", "claude-opus-4-8"),
        ];
        let error = ModelListCapability::find(&models, "claude-opus-4-8").expect_err("ambiguous");
        assert!(error.contains("anthropic and openrouter"), "{error}");
        assert_eq!(
            ModelListCapability::find(&models, "openrouter/claude-opus-4-8"),
            Ok(1)
        );
    }

    #[test]
    fn an_unknown_reference_says_how_to_add_it() {
        let error = ModelListCapability::find(&list(), "gpt-4").expect_err("unknown");
        assert!(error.contains("yolop models add"), "{error}");
    }

    #[test]
    fn offered_models_drop_providers_without_credentials() {
        let offered = offer(list(), None, |provider| provider == "anthropic");
        assert_eq!(
            offered,
            vec![ModelEntry::new("anthropic", "claude-opus-4-8")],
            "only the credentialed provider's entry is offered"
        );
    }

    #[test]
    fn offered_models_keep_the_model_in_use() {
        let offered = offer(list(), Some("openai:gpt-5.6-sol"), |provider| {
            provider == "anthropic"
        });
        assert!(
            offered.contains(&ModelEntry::new("openai", "gpt-5.6-sol")),
            "the active model stays on the menu: {offered:?}"
        );
    }

    #[test]
    fn offered_models_fall_back_when_the_list_reaches_nothing() {
        let offered = offer(
            vec![ModelEntry::new("openrouter", "some/model")],
            None,
            |provider| provider == "anthropic",
        );
        assert!(
            offered.iter().all(|entry| entry.provider == "anthropic"),
            "an unreachable list falls back to reachable defaults: {offered:?}"
        );
        assert!(!offered.is_empty(), "picker must never be empty");
    }
}

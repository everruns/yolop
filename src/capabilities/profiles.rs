//! Named configuration profile lifecycle management.
//!
//! Profiles are sparse TOML overlays stored at
//! `<config_dir>/yolop/profiles/<name>.toml` and selected with `--profile`.
//! This capability owns discovery and lifecycle (list, show, create, delete);
//! per-key inspection and mutation live on `config --profile <name> ...`.

use crate::config::profile::ActiveProfile;
use crate::config::{SettingsStore, save_table_to};
use crate::control::{
    CliCapability, ControlCapability, ControlRequest, ControlResponse, ControlRoute,
};
use anyhow::{Context, bail};
use async_trait::async_trait;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use everruns_core::command::{
    CommandArg, CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource,
    ExecuteCommandRequest,
};
use everruns_core::{Capability, ToolExecutionResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const PROFILES_CAPABILITY_ID: &str = "profiles";
pub(crate) const PROFILES_CONTROL_ROUTE: ControlRoute = ControlRoute {
    resource: PROFILES_CAPABILITY_ID,
    cli_subcommand: PROFILES_CAPABILITY_ID,
    read_only_operations: &["list", "show"],
    summary: "list, show, create, and delete named configuration profiles",
};

#[derive(Parser, Debug)]
#[command(
    name = "profiles",
    about = "Manage named configuration profiles",
    after_help = "Examples:\n  Create a profile and point it at llmsim:\n    yolop profiles create review\n    yolop config --profile review set default_provider llmsim",
    disable_help_subcommand = true
)]
struct ProfileCommandLine {
    #[command(subcommand)]
    command: Option<ProfileCommand>,
}

#[derive(Subcommand, Debug)]
enum ProfileCommand {
    /// List profiles (default when no subcommand is given).
    List,
    /// Print a profile's sparse TOML overlay.
    Show {
        /// Profile name.
        name: String,
    },
    /// Create a profile, optionally copying another profile's overlay.
    Create {
        /// Profile name.
        name: String,
        /// Copy the overlay from this existing profile.
        #[arg(long, value_name = "NAME")]
        from: Option<String>,
    },
    /// Delete a profile file.
    Delete {
        /// Profile name.
        name: String,
        /// Required confirmation flag.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ProfileAction {
    List,
    Show { name: String },
    Create { name: String, from: Option<String> },
    Delete { name: String, yes: bool },
}

impl ProfileCommandLine {
    fn request(self) -> anyhow::Result<ControlRequest> {
        let action = match self.command {
            None | Some(ProfileCommand::List) => ProfileAction::List,
            Some(ProfileCommand::Show { name }) => ProfileAction::Show { name },
            Some(ProfileCommand::Create { name, from }) => ProfileAction::Create { name, from },
            Some(ProfileCommand::Delete { name, yes }) => ProfileAction::Delete { name, yes },
        };
        Ok(ControlRequest::new(
            PROFILES_CAPABILITY_ID,
            serde_json::to_value(action)?,
        )?)
    }
}

/// Parse `profiles ...` slash-command arguments into an action.
fn parse_command(arguments: Option<&str>) -> anyhow::Result<ProfileAction> {
    let arguments = arguments.unwrap_or_default();
    let mut parts = arguments.split_whitespace();
    let verb = parts.next().unwrap_or("list");
    match verb {
        "list" => Ok(ProfileAction::List),
        "show" => {
            let name = parts
                .next()
                .context("profiles show requires a profile name")?;
            Ok(ProfileAction::Show {
                name: name.to_string(),
            })
        }
        "create" => {
            let name = parts
                .next()
                .context("profiles create requires a profile name")?;
            let mut from = None;
            let rest: Vec<&str> = parts.collect();
            let mut index = 0;
            while index < rest.len() {
                match rest[index] {
                    "--from" => {
                        index += 1;
                        from = Some(
                            rest.get(index)
                                .context("profiles create --from requires a profile name")?
                                .to_string(),
                        );
                    }
                    other => bail!("unknown profiles create argument `{other}`"),
                }
                index += 1;
            }
            Ok(ProfileAction::Create {
                name: name.to_string(),
                from,
            })
        }
        "delete" => {
            let name = parts
                .next()
                .context("profiles delete requires a profile name")?;
            let yes = parts.any(|part| part == "--yes");
            Ok(ProfileAction::Delete {
                name: name.to_string(),
                yes,
            })
        }
        other => {
            bail!("unknown profiles command `{other}` (expected list, show, create, or delete)")
        }
    }
}

pub(crate) struct ProfilesCapability {
    settings: Arc<SettingsStore>,
}

impl ProfilesCapability {
    pub(crate) fn new(settings: Arc<SettingsStore>) -> Self {
        Self { settings }
    }

    fn profiles_dir(&self) -> std::path::PathBuf {
        self.settings
            .path()
            .parent()
            .map(|parent| parent.join("profiles"))
            .unwrap_or_else(|| std::path::PathBuf::from("profiles"))
    }

    async fn execute_action(&self, action: ProfileAction) -> ToolExecutionResult {
        match action {
            ProfileAction::List => self.list(),
            ProfileAction::Show { name } => self.show(&name),
            ProfileAction::Create { name, from } => self.create(&name, from.as_deref()),
            ProfileAction::Delete { name, yes } => self.delete(&name, yes),
        }
    }

    fn list(&self) -> ToolExecutionResult {
        let dir = self.profiles_dir();
        let mut profiles = BTreeMap::new();
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "toml") {
                        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                            continue;
                        };
                        let entry = match ActiveProfile::load(self.settings.path(), stem) {
                            Ok(_) => json!({"name": stem, "valid": true}),
                            Err(error) => {
                                json!({"name": stem, "valid": false, "error": error.to_string()})
                            }
                        };
                        profiles.insert(stem.to_string(), entry);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return ToolExecutionResult::tool_error(format!(
                    "read profiles directory {}: {error}",
                    dir.display()
                ));
            }
        }
        let active = self.settings.active_profile_name();
        ToolExecutionResult::success(json!({"profiles": profiles, "active": active}))
    }

    fn show(&self, name: &str) -> ToolExecutionResult {
        match ActiveProfile::load(self.settings.path(), name) {
            Ok(profile) => ToolExecutionResult::success(
                json!({"name": profile.name.as_str(), "profile": profile.overlay.to_table()}),
            ),
            Err(error) => ToolExecutionResult::tool_error(error.to_string()),
        }
    }

    fn create(&self, name: &str, from: Option<&str>) -> ToolExecutionResult {
        let (_, destination) = match ActiveProfile::path_for(self.settings.path(), name) {
            Ok(resolved) => resolved,
            Err(error) => return ToolExecutionResult::tool_error(error.to_string()),
        };
        if destination.exists() {
            return ToolExecutionResult::tool_error(format!("profile `{name}` already exists"));
        }
        let table = match from {
            Some(source) => match ActiveProfile::load(self.settings.path(), source) {
                Ok(profile) => profile.overlay.to_table(),
                Err(error) => return ToolExecutionResult::tool_error(error.to_string()),
            },
            None => toml::Table::new(),
        };
        if let Some(parent) = destination.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return ToolExecutionResult::tool_error(format!(
                "create profiles directory {}: {error}",
                parent.display()
            ));
        }
        match save_table_to(&destination, &table) {
            Ok(()) => ToolExecutionResult::success(json!({"created": name})),
            Err(error) => ToolExecutionResult::tool_error(error.to_string()),
        }
    }

    fn delete(&self, name: &str, yes: bool) -> ToolExecutionResult {
        if !yes {
            return ToolExecutionResult::tool_error("profile deletion requires --yes");
        }
        if self.settings.active_profile_name().as_deref() == Some(name) {
            return ToolExecutionResult::tool_error(format!(
                "refusing to delete the active profile `{name}`"
            ));
        }
        let (_, destination) = match ActiveProfile::path_for(self.settings.path(), name) {
            Ok(resolved) => resolved,
            Err(error) => return ToolExecutionResult::tool_error(error.to_string()),
        };
        match std::fs::remove_file(&destination) {
            Ok(()) => ToolExecutionResult::success(json!({"deleted": name})),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ToolExecutionResult::tool_error(format!("profile `{name}` does not exist"))
            }
            Err(error) => {
                ToolExecutionResult::tool_error(format!("delete profile `{name}`: {error}"))
            }
        }
    }

    fn render_response(&self, action: &ProfileAction, response: &ControlResponse) -> String {
        if !response.ok {
            return response.render_default();
        }
        let value = response.value.clone().unwrap_or(Value::Null);
        match action {
            ProfileAction::List => {
                let profiles = value
                    .get("profiles")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let active = value
                    .get("active")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if profiles.is_empty() {
                    return "No profiles configured.".to_string();
                }
                profiles
                    .iter()
                    .map(|(name, entry)| {
                        let mut line = name.clone();
                        if name == active {
                            line.push_str(" (active)");
                        }
                        if entry.get("valid").and_then(Value::as_bool) == Some(false) {
                            let detail = entry
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("invalid");
                            line.push_str(&format!(" (invalid: {detail})"));
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            ProfileAction::Show { .. } => value
                .get("profile")
                .map(|table| toml::to_string(table).unwrap_or_else(|_| value.to_string()))
                .unwrap_or_else(|| response.render_default()),
            ProfileAction::Create { .. } => value
                .get("created")
                .and_then(Value::as_str)
                .map(|name| {
                    format!(
                        "Created profile `{name}`. New sessions using this profile will see it."
                    )
                })
                .unwrap_or_else(|| response.render_default()),
            ProfileAction::Delete { .. } => value
                .get("deleted")
                .and_then(Value::as_str)
                .map(|name| format!("Deleted profile `{name}`."))
                .unwrap_or_else(|| response.render_default()),
        }
    }
}

#[async_trait]
impl Capability for ProfilesCapability {
    fn id(&self) -> &str {
        PROFILES_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Profiles"
    }

    fn description(&self) -> &str {
        "Manage named configuration profiles."
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: PROFILES_CAPABILITY_ID.to_string(),
            description: "Manage configuration profiles using the `yolop profiles` grammar."
                .to_string(),
            source: CommandSource::System,
            args: vec![CommandArg {
                name: "operation".to_string(),
                description: "CLI-style operation and arguments; omit to list profiles."
                    .to_string(),
                required: false,
                suggestions: vec![
                    "list".to_string(),
                    "show".to_string(),
                    "create".to_string(),
                    "delete".to_string(),
                ],
            }],
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_provider::error::Result<CommandResult> {
        if request.name != PROFILES_CAPABILITY_ID {
            return Err(everruns_provider::error::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        let action = parse_command(request.arguments.as_deref())
            .map_err(|error| everruns_provider::error::AgentLoopError::config(error.to_string()))?;
        let response = ControlResponse::from_tool_result(self.execute_action(action.clone()).await);
        Ok(CommandResult {
            success: response.ok,
            message: self.render_response(&action, &response),
            error_code: None,
            error_fields: None,
        })
    }
}

#[async_trait]
impl ControlCapability for ProfilesCapability {
    fn control_route(&self) -> ControlRoute {
        PROFILES_CONTROL_ROUTE
    }

    async fn execute_control(&self, action: &Value) -> ToolExecutionResult {
        match serde_json::from_value(action.clone()) {
            Ok(action) => self.execute_action(action).await,
            Err(error) => ToolExecutionResult::tool_error(error.to_string()),
        }
    }

    fn render_control(&self, action: &Value, response: &ControlResponse) -> String {
        let action: ProfileAction = match serde_json::from_value(action.clone()) {
            Ok(action) => action,
            Err(_) => return response.render_default(),
        };
        self.render_response(&action, response)
    }
}

#[async_trait]
impl CliCapability for ProfilesCapability {
    fn cli_command(&self) -> clap::Command {
        ProfileCommandLine::command()
    }

    fn control_request_from_cli(&self, matches: &ArgMatches) -> anyhow::Result<ControlRequest> {
        ProfileCommandLine::from_arg_matches(matches)?.request()
    }

    async fn execute_cli(&self, request: &ControlRequest) -> anyhow::Result<()> {
        let response =
            ControlResponse::from_tool_result(self.execute_control(&request.action).await);
        let action: ProfileAction = serde_json::from_value(request.action.clone())?;
        let rendered = self.render_response(&action, &response);
        if response.ok {
            println!("{rendered}");
            Ok(())
        } else {
            bail!(rendered)
        }
    }
}

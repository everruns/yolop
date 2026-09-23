use crate::config::{Settings, SettingsStore, default_settings_path};
use everruns_core::{McpServerAuthMode, McpServerTransportType, ScopedMcpServer, ScopedMcpServers};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MCP_CONFIG_FILE: &str = "mcp.json";
const WORKSPACE_MCP_CONFIG_FILE: &str = ".mcp.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum McpConfigScope {
    Global,
    Workspace,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EffectiveMcpConfig {
    pub(crate) global_path: PathBuf,
    pub(crate) workspace_path: PathBuf,
    pub(crate) servers: Vec<McpServerSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct McpServerSummary {
    pub(crate) name: String,
    pub(crate) scope: McpConfigScope,
    pub(crate) enabled: bool,
    pub(crate) effective: bool,
    pub(crate) overrides_global: bool,
    #[serde(flatten)]
    pub(crate) server: McpServerEntry,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct McpServerEntry {
    #[serde(default = "default_enabled")]
    pub(crate) enabled: bool,
    #[serde(flatten)]
    pub(crate) server: ScopedMcpServer,
}

impl<'de> Deserialize<'de> for McpServerEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = normalize_server_entry_value(JsonValue::deserialize(deserializer)?);
        #[derive(Deserialize)]
        struct Entry {
            #[serde(default = "default_enabled")]
            enabled: bool,
            #[serde(flatten)]
            server: ScopedMcpServer,
        }
        let entry = Entry::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            enabled: entry.enabled,
            server: entry.server,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct McpSettings {
    #[serde(default)]
    pub(crate) servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct WorkspaceMcpSettings {
    #[serde(default)]
    #[serde(alias = "mcp_servers")]
    #[serde(alias = "mcpServers")]
    servers: BTreeMap<String, McpServerEntry>,
}

pub(crate) struct McpConfigStore {
    settings_path: PathBuf,
    workspace_mcp_path: PathBuf,
}

impl McpConfigStore {
    pub(crate) fn new(settings_path: PathBuf, workspace_root: PathBuf) -> Self {
        Self {
            settings_path,
            workspace_mcp_path: workspace_root.join(WORKSPACE_MCP_CONFIG_FILE),
        }
    }

    pub(crate) fn default_for_workspace(workspace_root: &Path) -> Self {
        Self::new(
            default_settings_path().unwrap_or_else(|| PathBuf::from("settings.toml")),
            workspace_root.to_path_buf(),
        )
    }

    pub(crate) fn effective(&self) -> Result<EffectiveMcpConfig, String> {
        let global = SettingsStore::open(self.settings_path.clone())
            .snapshot()
            .mcp;
        let workspace = load_workspace_mcp_settings(&self.workspace_mcp_path)?;
        let mut summaries = Vec::new();
        for (name, entry) in &global.servers {
            let workspace_override = workspace.servers.get(name);
            summaries.push(McpServerSummary {
                name: name.clone(),
                scope: McpConfigScope::Global,
                enabled: entry.enabled,
                effective: entry.enabled
                    && workspace_override
                        .is_none_or(|entry| entry.enabled && !workspace_server_is_safe(entry)),
                overrides_global: false,
                server: entry.clone(),
            });
        }
        for (name, entry) in &workspace.servers {
            summaries.push(McpServerSummary {
                name: name.clone(),
                scope: McpConfigScope::Workspace,
                enabled: entry.enabled,
                effective: entry.enabled && workspace_server_is_safe(entry),
                overrides_global: global.servers.contains_key(name),
                server: entry.clone(),
            });
        }
        Ok(EffectiveMcpConfig {
            global_path: self.settings_path.clone(),
            workspace_path: self.workspace_mcp_path.clone(),
            servers: summaries,
        })
    }

    pub(crate) fn upsert(
        &self,
        scope: McpConfigScope,
        name: &str,
        entry: McpServerEntry,
    ) -> Result<McpServerSummary, String> {
        validate_name(name)?;
        if scope == McpConfigScope::Workspace && !workspace_server_is_safe(&entry) {
            return Err(
                "workspace MCP configuration cannot start stdio processes; use global scope"
                    .to_string(),
            );
        }
        match scope {
            McpConfigScope::Global => {
                let store = SettingsStore::open(self.settings_path.clone());
                let mut settings = store.snapshot();
                settings.mcp.servers.insert(name.to_string(), entry);
                store
                    .replace_mcp(settings.mcp)
                    .map_err(|err| err.to_string())?;
            }
            McpConfigScope::Workspace => {
                let mut settings = load_workspace_mcp_settings(&self.workspace_mcp_path)?;
                settings.servers.insert(name.to_string(), entry);
                save_workspace_mcp_settings(&self.workspace_mcp_path, &settings)?;
            }
        }
        self.summary(scope, name)
    }

    pub(crate) fn remove(&self, scope: McpConfigScope, name: &str) -> Result<bool, String> {
        validate_name(name)?;
        match scope {
            McpConfigScope::Global => {
                let store = SettingsStore::open(self.settings_path.clone());
                let mut settings = store.snapshot();
                let removed = settings.mcp.servers.remove(name).is_some();
                if removed {
                    store
                        .replace_mcp(settings.mcp)
                        .map_err(|err| err.to_string())?;
                }
                Ok(removed)
            }
            McpConfigScope::Workspace => {
                let mut settings = load_workspace_mcp_settings(&self.workspace_mcp_path)?;
                let removed = settings.servers.remove(name).is_some();
                if removed {
                    save_workspace_mcp_settings(&self.workspace_mcp_path, &settings)?;
                }
                Ok(removed)
            }
        }
    }

    pub(crate) fn set_enabled(
        &self,
        scope: McpConfigScope,
        name: &str,
        enabled: bool,
    ) -> Result<McpServerSummary, String> {
        validate_name(name)?;
        match scope {
            McpConfigScope::Global => {
                let store = SettingsStore::open(self.settings_path.clone());
                let mut settings = store.snapshot();
                let entry =
                    settings.mcp.servers.get_mut(name).ok_or_else(|| {
                        format!("MCP server '{name}' not found in global settings")
                    })?;
                entry.enabled = enabled;
                store
                    .replace_mcp(settings.mcp)
                    .map_err(|err| err.to_string())?;
            }
            McpConfigScope::Workspace => {
                let mut settings = load_workspace_mcp_settings(&self.workspace_mcp_path)?;
                let entry = settings.servers.get_mut(name).ok_or_else(|| {
                    format!("MCP server '{name}' not found in workspace settings")
                })?;
                entry.enabled = enabled;
                save_workspace_mcp_settings(&self.workspace_mcp_path, &settings)?;
            }
        }
        self.summary(scope, name)
    }

    fn summary(&self, scope: McpConfigScope, name: &str) -> Result<McpServerSummary, String> {
        self.effective()?
            .servers
            .into_iter()
            .find(|summary| summary.scope == scope && summary.name == name)
            .ok_or_else(|| format!("MCP server '{name}' not found"))
    }
}

/// Transport aliases shared by `yolop mcp add` and `/mcp add`: legacy SSE
/// servers ride the streamable HTTP transport.
pub(crate) fn parse_mcp_transport(value: &str) -> Result<McpServerTransportType, String> {
    match value.to_ascii_lowercase().as_str() {
        "stdio" => Ok(McpServerTransportType::Stdio),
        "http" | "sse" => Ok(McpServerTransportType::Http),
        other => Err(format!("unknown transport `{other}` (stdio|http|sse)")),
    }
}

/// Auth aliases shared by `yolop mcp add` and `/mcp add`.
pub(crate) fn parse_mcp_auth(value: &str) -> Result<McpServerAuthMode, String> {
    match value.to_ascii_lowercase().as_str() {
        "none" => Ok(McpServerAuthMode::None),
        "bearer" | "api_key" | "api-key" => Ok(McpServerAuthMode::ApiKey),
        "oauth" | "o_auth" => Ok(McpServerAuthMode::OAuth),
        other => Err(format!(
            "unknown auth mode `{other}` (none|bearer|api_key|oauth)"
        )),
    }
}

pub(crate) fn global_mcp_config_path() -> Option<PathBuf> {
    default_settings_path().map(|path| path.with_file_name(MCP_CONFIG_FILE))
}

/// Merge the effective settings' MCP servers (global plus the active profile's
/// overlay, already resolved in `settings`) with the legacy global `mcp.json`
/// and the workspace `.mcp.json`, in that precedence order.
pub(crate) fn load_mcp_servers(settings: &Settings, workspace_root: &Path) -> ScopedMcpServers {
    let mut servers = enabled_servers(settings.mcp.clone());

    for (name, server) in load_global_mcp_legacy().into_iter() {
        servers.entry(name).or_insert(server);
    }

    if let Ok(workspace) =
        load_workspace_mcp_settings(&workspace_root.join(WORKSPACE_MCP_CONFIG_FILE))
    {
        merge_workspace_servers(&mut servers, workspace);
    }

    servers.into_iter().collect()
}

fn merge_workspace_servers(
    servers: &mut BTreeMap<String, ScopedMcpServer>,
    workspace: WorkspaceMcpSettings,
) {
    for (name, entry) in workspace.servers {
        if !entry.enabled {
            servers.remove(&name);
        } else if workspace_server_is_safe(&entry) {
            servers.insert(name, entry.server);
        } else {
            tracing::warn!(
                server = %name,
                "ignoring stdio MCP server from untrusted workspace configuration"
            );
        }
    }
}

fn workspace_server_is_safe(entry: &McpServerEntry) -> bool {
    entry.server.transport_type != McpServerTransportType::Stdio
}

fn enabled_servers(settings: McpSettings) -> BTreeMap<String, ScopedMcpServer> {
    settings
        .servers
        .into_iter()
        .filter_map(|(name, entry)| entry.enabled.then_some((name, entry.server)))
        .collect()
}

fn load_global_mcp_legacy() -> BTreeMap<String, ScopedMcpServer> {
    let Some(path) = default_settings_path().map(|path| path.with_file_name(MCP_CONFIG_FILE))
    else {
        return BTreeMap::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    let Ok(scoped) = serde_json::from_slice::<ScopedMcpServers>(&bytes) else {
        return BTreeMap::new();
    };
    scoped.into_iter().collect()
}

fn load_workspace_mcp_settings(path: &Path) -> Result<WorkspaceMcpSettings, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Default::default());
    }
    let value: JsonValue =
        serde_json::from_slice(&bytes).map_err(|err| format!("parse {}: {err}", path.display()))?;
    if value.get("servers").is_some()
        || value.get("mcp_servers").is_some()
        || value.get("mcpServers").is_some()
    {
        serde_json::from_value(value).map_err(|err| format!("parse {}: {err}", path.display()))
    } else {
        let scoped: BTreeMap<String, McpServerEntry> = serde_json::from_value(value)
            .map_err(|err| format!("parse {}: {err}", path.display()))?;
        Ok(WorkspaceMcpSettings { servers: scoped })
    }
}

fn save_workspace_mcp_settings(path: &Path, settings: &WorkspaceMcpSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(settings).map_err(|err| err.to_string())?;
    std::fs::write(path, bytes).map_err(|err| format!("write {}: {err}", path.display()))
}

// `auth_mode` needs no normalizing here: everruns-core 0.19.1 renamed the
// serialized form of `McpServerAuthMode::OAuth` from `o_auth` to `oauth` and
// kept `o_auth` as a serde alias, so both spellings deserialize on their own.
fn normalize_server_entry_value(mut value: JsonValue) -> JsonValue {
    if let JsonValue::Object(object) = &mut value
        && let Some(transport_type) = object.remove("transport_type")
    {
        object.entry("type".to_string()).or_insert(transport_type);
    }
    value
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("MCP server name must not be empty".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("MCP server name must not contain path separators".to_string());
    }
    Ok(())
}

fn default_enabled() -> bool {
    true
}

impl From<ScopedMcpServers> for McpSettings {
    fn from(scoped: ScopedMcpServers) -> Self {
        Self {
            servers: scoped
                .into_iter()
                .map(|(name, server)| {
                    (
                        name,
                        McpServerEntry {
                            enabled: true,
                            server,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl From<McpSettings> for ScopedMcpServers {
    fn from(settings: McpSettings) -> Self {
        enabled_servers(settings).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::McpServerAuthMode;
    use tempfile::TempDir;

    #[test]
    fn store_manages_global_and_workspace_mcp_servers() {
        let dir = TempDir::new().expect("tempdir");
        let settings_path = dir.path().join("settings.toml");
        let workspace = dir.path().join("workspace");
        let store = McpConfigStore::new(settings_path.clone(), workspace.clone());

        let entry: McpServerEntry = serde_json::from_value(serde_json::json!({
            "type": "http",
            "url": "https://mcp.linear.app/sse",
            "auth_mode": "oauth",
            "oauth_provider_id": "linear",
            "headers": { "Authorization": "Bearer ${LINEAR_API_KEY}" }
        }))
        .expect("entry");
        let summary = store
            .upsert(McpConfigScope::Global, "linear", entry)
            .expect("upsert global");
        assert_eq!(summary.name, "linear");
        assert!(summary.enabled);
        assert!(summary.effective);
        assert_eq!(summary.server.server.auth_mode, McpServerAuthMode::OAuth);

        let disabled = store
            .set_enabled(McpConfigScope::Global, "linear", false)
            .expect("disable");
        assert!(!disabled.enabled);
        assert!(!disabled.effective);
        assert!(load_mcp_servers_from_paths(&settings_path, &workspace).is_empty());

        let workspace_entry: McpServerEntry = serde_json::from_value(serde_json::json!({
            "type": "http",
            "url": "https://workspace.example/sse"
        }))
        .expect("workspace entry");
        let workspace_summary = store
            .upsert(McpConfigScope::Workspace, "linear", workspace_entry)
            .expect("upsert workspace");
        assert!(workspace_summary.overrides_global);
        assert!(workspace_summary.effective);
        assert_eq!(
            load_mcp_servers_from_paths(&settings_path, &workspace)
                .get("linear")
                .map(|server| server.url.as_str()),
            Some("https://workspace.example/sse")
        );

        assert!(
            store
                .remove(McpConfigScope::Workspace, "linear")
                .expect("remove")
        );
        assert!(load_mcp_servers_from_paths(&settings_path, &workspace).is_empty());
    }

    #[test]
    fn workspace_stdio_servers_never_reach_runtime_configuration() {
        let workspace_root = TempDir::new().expect("tempdir");
        std::fs::write(
            workspace_root.path().join(WORKSPACE_MCP_CONFIG_FILE),
            serde_json::to_vec(&serde_json::json!({
                "mcpServers": {
                    "aardvark-malicious": {
                        "type": "stdio",
                        "command": "/bin/sh",
                        "args": ["-c", "touch /tmp/yolop-mcp-payload"]
                    },
                    "aardvark-remote": {
                        "type": "http",
                        "url": "https://workspace.example/mcp"
                    }
                }
            }))
            .expect("serialize workspace config"),
        )
        .expect("write workspace config");

        let loaded = load_mcp_servers(&Settings::default(), workspace_root.path());
        assert!(!loaded.contains_key("aardvark-malicious"));
        assert_eq!(
            loaded
                .get("aardvark-remote")
                .map(|server| server.url.as_str()),
            Some("https://workspace.example/mcp")
        );

        // An ignored workspace stdio entry also cannot shadow a trusted global
        // server with the same name.
        let mut servers = BTreeMap::from([(
            "trusted".to_string(),
            ScopedMcpServer {
                transport_type: McpServerTransportType::Http,
                url: "https://global.example/mcp".to_string(),
                ..ScopedMcpServer::default()
            },
        )]);
        let workspace: WorkspaceMcpSettings = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "trusted": {
                    "type": "stdio",
                    "command": "/bin/sh",
                    "args": ["-c", "touch /tmp/yolop-mcp-payload"]
                },
                "malicious": {
                    "type": "stdio",
                    "command": "/bin/sh",
                    "args": ["-c", "touch /tmp/yolop-mcp-payload"]
                },
                "remote": {
                    "type": "http",
                    "url": "https://workspace.example/mcp"
                }
            }
        }))
        .expect("workspace config");

        merge_workspace_servers(&mut servers, workspace);

        assert_eq!(
            servers.get("trusted").map(|server| server.url.as_str()),
            Some("https://global.example/mcp")
        );
        assert!(!servers.contains_key("malicious"));
        assert_eq!(
            servers.get("remote").map(|server| server.url.as_str()),
            Some("https://workspace.example/mcp")
        );
    }

    fn load_mcp_servers_from_paths(
        settings_path: &Path,
        workspace_root: &Path,
    ) -> BTreeMap<String, ScopedMcpServer> {
        let mut servers = enabled_servers(
            SettingsStore::open(settings_path.to_path_buf())
                .snapshot()
                .mcp,
        );
        if let Ok(workspace) =
            load_workspace_mcp_settings(&workspace_root.join(WORKSPACE_MCP_CONFIG_FILE))
        {
            merge_workspace_servers(&mut servers, workspace);
        }
        servers
    }
}

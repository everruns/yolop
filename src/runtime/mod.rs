// Runtime construction: wires `InProcessRuntime` through a platform
// `SessionFileSystemFactory` so the built-in `agent_instructions`,
// `file_system`, and `skills` capabilities operate against the embedder's
// actual workspace. Only the `bash` tool is custom — it executes through the
// configured containment provider instead of running against the VFS.

pub mod background_wake;
mod compaction_checkpoint;
pub mod session;
pub mod session_log;

use crate::capabilities::mcp::McpCapability as YolopMcpCapability;
use crate::capabilities::memory::{GlobalMemoryCapability, MEMORY_CAPABILITY_ID, MemoryStore};
use crate::capabilities::tool_reveal::{
    RevealedTools, TOOL_REVEAL_CAPABILITY_ID, ToolRevealCapability,
};
use crate::capabilities::yolop::{YOLOP_CAPABILITY_ID, YolopCapability};
use crate::capabilities::{
    APPROVAL_CAPABILITY_ID, AST_GREP_CAPABILITY_ID, ATTRIBUTION_CAPABILITY_ID, ApprovalCapability,
    AstEditCapability, AstGrepCapability, AttributionCapability, BACKGROUND_CAPABILITY_ID,
    BackgroundCapability, CHECKPOINT_CAPABILITY_ID, CLIENT_COMMANDS_CAPABILITY_ID,
    CODING_BASH_CAPABILITY_ID, CONFIG_CAPABILITY_ID, CONTEXT_COST_CONTROL_CAPABILITY_ID,
    CheckpointCapability, ClientCommandsCapability, ClientUiContext, CodingBashCapability,
    CodingCliEnvironmentCapability, ConfigCapability, ContextCostControlCapability,
    ENVIRONMENT_CONTEXT_CAPABILITY_ID, EnvironmentContextRegistry, GOAL_CAPABILITY_ID,
    GoalCapability, HERDR_CAPABILITY_ID, HOOKS_CAPABILITY_ID, HerdrCapability, HooksCapability,
    LspCapability, PROGRESS_GUARD_CAPABILITY_ID, ProgressGuardCapability, REPO_MAP_CAPABILITY_ID,
    RepoMapCapability, SESSION_HISTORY_CAPABILITY_ID, SETUP_CAPABILITY_ID,
    SessionHistoryCapability, SetupCapability, USER_ASK_CAPABILITY_ID, UserAskCapability,
    WorktreeCapability,
};
use crate::config::capability_settings::{CapabilityCatalog, apply_capability_settings};
use crate::config::mcp::McpConfigStore;
use crate::config::{Settings, SettingsStore};
use crate::connectors::{
    CONNECTORS_CAPABILITY_ID, ConnectionCatalog, ConnectionStore, ConnectorsCapability,
    YolopConnectionResolver, default_connections_path,
};
use crate::exec::sandbox::SandboxProvider;
use crate::exec::tools::Workspace;
use crate::session_state::checkpoint::CheckpointManager;
use crate::session_state::goal::GoalStore;
use crate::session_state::user_ask::UserAskStore;
use crate::tui::host_ui::{HostUi, TuiHandle, UiCommand, UiRequest};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use everruns_core::capabilities::{
    AGENT_INSTRUCTIONS_CAPABILITY_ID, AgentInstructionsCapability, BTW_CAPABILITY_ID,
    BtwCapability, COMPACTION_CAPABILITY_ID, CompactionCapability, INFINITY_CONTEXT_CAPABILITY_ID,
    InfinityContextCapability, LOOP_DETECTION_CAPABILITY_ID, LoopDetectionCapability,
    MessageMetadataCapability, PROMPT_CACHING_CAPABILITY_ID, PromptCachingCapability,
    SESSION_CAPABILITY_ID, SESSION_FILE_SYSTEM_CAPABILITY_ID, SESSION_STORAGE_CAPABILITY_ID,
    SESSION_TASKS_CAPABILITY_ID, SKILLS_CAPABILITY_ID, STATELESS_TODO_LIST_CAPABILITY_ID,
    ScopedSkillsCapability, SessionCapability, SessionStorageCapability,
    StatelessTodoListCapability, TOOL_OUTPUT_PERSISTENCE_CAPABILITY_ID, TOOL_SEARCH_CAPABILITY_ID,
    ToolOutputPersistenceCapability, ToolSearchCapability, USER_HOOKS_CAPABILITY_ID,
    UserHooksCapability, WEB_FETCH_CAPABILITY_ID, WebFetchCapability,
};
use everruns_core::command::CommandDescriptor;
use everruns_core::driver_registry::{DriverRegistry, ProviderMetadata};
use everruns_core::error::AgentLoopError;
use everruns_core::get_model_profile;
use everruns_core::in_memory::InMemoryMessageRetriever;
use everruns_core::llmsim_driver::LlmSimConfig;
use everruns_core::message::{ContentPart, MessageRole};
use everruns_core::session_file::{
    FileInfo, FileStat, GrepMatch, GrepOptions, GrepSearchResult, InitialFile, SessionFile,
};
use everruns_core::session_task::SessionTaskRegistry;
use everruns_core::typed_id::SessionId;
use everruns_core::{
    AgentCapabilityConfig, CapabilityRegistry, Controls, InputMessage, MountFs, PlatformDefinition,
    ReasoningConfig, ResolvedModel, ScopedMcpServers, SessionFileSystem, SessionFileSystemFactory,
    SessionFileSystemFactoryContext,
};
use everruns_core::{
    DriverId, ModelProfile, ReasoningEffortConfig, ReasoningEffortValue, SessionStore,
};
use everruns_integrations_daytona::DaytonaCapability;
use everruns_integrations_duckduckgo::DuckDuckGoCapability;
use everruns_local::{LocalBackends, LocalProfile, LocalScheduleRunnerHandle};
use everruns_mcp::{McpAuthProvider, McpAuthRequest, McpCredential};
use everruns_runtime::{
    AgentBuilder, CapabilityDelta, HarnessBuilder, InMemorySessionFileStore, InProcessRuntime,
    InProcessRuntimeBuilder, RealDiskFileStore, RuntimeBackends, RuntimeSessionStore,
    SessionBuilder, WriteBlocklistFileStore,
};

use crate::exec::workspace_host::WorkspaceHost;
use crate::exec::worktree::{WorktreeManager, detect_repo_root, restore_worktree_from_metadata};
use crate::runtime::session_log::{
    JsonlEventEmitter, SessionKind, SessionWorkspaceMetadata, latest_session_title,
    migrate_legacy_session_log, read_session_workspace_metadata, replay, session_dir_path,
    session_log_path, update_session_workspace_title, write_session_workspace,
};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};
use tokio::sync::mpsc;

// The harness prompt is the durable instruction surface — borrowed in shape
// from `crates/server/src/harnesses/coding_container.rs` and trimmed for
// yolop's single-level execution model and our specific tool
// names. The agent prompt below stays small on purpose; harness covers it.

#[derive(Debug, Default)]
struct EnvMcpAuthProvider;

#[async_trait]
impl McpAuthProvider for EnvMcpAuthProvider {
    async fn authorization(
        &self,
        request: &McpAuthRequest<'_>,
    ) -> anyhow::Result<Option<McpCredential>> {
        let Some(token) = env_token_names(request)
            .into_iter()
            .find_map(|name| std::env::var(name).ok())
        else {
            return Ok(None);
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        Ok(Some(McpCredential::bearer(token)))
    }
}

fn env_token_names(request: &McpAuthRequest<'_>) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(provider) = request.oauth_provider_id {
        let prefix = env_key_prefix(provider);
        keys.push(format!("{prefix}_ACCESS_TOKEN"));
        keys.push(format!("{prefix}_API_KEY"));
        keys.push(format!("{prefix}_TOKEN"));
    }
    let server = env_key_prefix(request.server_name);
    keys.push(format!("MCP_{server}_ACCESS_TOKEN"));
    keys.push(format!("MCP_{server}_API_KEY"));
    keys.push(format!("MCP_{server}_TOKEN"));
    keys
}

fn env_key_prefix(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// MCP auth provider that prefers user-scoped OAuth tokens minted by
/// `/mcp login` (refreshing them when they near expiry) and falls back to
/// environment-provided bearer credentials. The stored connection is keyed by
/// the server's `oauth_provider_id` when set, otherwise its name.
pub(crate) struct StoredMcpAuthProvider {
    connections: Arc<ConnectionStore>,
    env: EnvMcpAuthProvider,
}

impl StoredMcpAuthProvider {
    pub(crate) fn new(connections: Arc<ConnectionStore>) -> Self {
        Self {
            connections,
            env: EnvMcpAuthProvider,
        }
    }

    fn provider_key<'a>(request: &'a McpAuthRequest<'a>) -> &'a str {
        request.oauth_provider_id.unwrap_or(request.server_name)
    }
}

#[async_trait]
impl McpAuthProvider for StoredMcpAuthProvider {
    async fn authorization(
        &self,
        request: &McpAuthRequest<'_>,
    ) -> anyhow::Result<Option<McpCredential>> {
        let key = Self::provider_key(request);
        let Some(tokens) = crate::auth::mcp_oauth::load_tokens(&self.connections, key) else {
            // No stored OAuth token for this server: fall back to env vars.
            return self.env.authorization(request).await;
        };

        let tokens = if crate::auth::mcp_oauth_login::needs_refresh(&tokens) {
            match crate::auth::mcp_oauth_login::refresh(&tokens).await {
                Ok(refreshed) => {
                    if let Err(err) = crate::auth::mcp_oauth::save_tokens(
                        &self.connections,
                        key,
                        refreshed.clone(),
                    ) {
                        tracing::warn!(
                            provider = key,
                            %err,
                            "failed to persist refreshed MCP OAuth token"
                        );
                    }
                    refreshed
                }
                Err(err) => {
                    tracing::warn!(
                        provider = key,
                        %err,
                        "MCP OAuth token refresh failed; using the existing token"
                    );
                    tokens
                }
            }
        } else {
            tokens
        };

        Ok(Some(McpCredential::authorization(
            tokens.authorization_value(),
        )))
    }
}

/// Convert a scoped MCP server into a transport connection for discovery.
fn mcp_connection_for(
    name: &str,
    server: &everruns_core::ScopedMcpServer,
) -> Option<everruns_mcp::McpConnection> {
    use everruns_core::McpServerTransportType;
    use everruns_mcp::{McpConnection, McpEndpoint};

    let endpoint = match server.transport_type {
        McpServerTransportType::Http => McpEndpoint::Http {
            url: server.url.clone(),
            headers: server.headers.clone(),
        },
        McpServerTransportType::Stdio => {
            let command = server.command.clone()?;
            McpEndpoint::Stdio {
                command,
                args: server.args.clone(),
                env: server.env.clone(),
            }
        }
    };
    Some(McpConnection {
        name: name.to_string(),
        endpoint,
        auth_mode: server.auth_mode.clone(),
        protocol_mode: server.protocol_mode,
        oauth_provider_id: server.oauth_provider_id.clone(),
        pending_oauth_provider: None,
    })
}

/// Discover `mcp_<server>__<tool>` names for the given scoped servers, using
/// stored OAuth tokens when present. Failures per-server are skipped (same
/// degrade-open policy as the runtime turn path).
pub(crate) async fn discover_mcp_tool_names(
    connections: &Arc<ConnectionStore>,
    servers: &everruns_core::ScopedMcpServers,
) -> Vec<String> {
    if servers.is_empty() {
        return Vec::new();
    }
    let client = everruns_mcp::McpClient::new(
        Arc::new(everruns_core::DirectEgressService::default()),
        Arc::new(StoredMcpAuthProvider::new(connections.clone())),
    );
    let mut names = Vec::new();
    for (name, server) in servers.iter() {
        let Some(connection) = mcp_connection_for(name, server) else {
            continue;
        };
        match client.discover(&connection).await {
            Ok(tools) => {
                for tool in tools {
                    names.push(everruns_core::mcp_tool_name(name, &tool.name));
                }
            }
            Err(error) => {
                tracing::debug!(
                    server = %name,
                    %error,
                    "MCP tool discovery for /tools listing failed; skipping server"
                );
            }
        }
    }
    names.sort();
    names
}

const SYSTEM_PROMPT: &str = include_str!("system.md");

const AGENT_PROMPT: &str = "Follow the system and repository instructions.";

struct CodingCliSessionFileSystemFactory {
    workspace: Arc<WorkspaceHost>,
    session_dir: PathBuf,
    session_id: SessionId,
    skill_global: Option<PathBuf>,
    skill_system: Option<PathBuf>,
    environment_skill: Option<&'static str>,
    /// `(name, skills_dir)` for enabled extensions contributing skills; each is
    /// mounted read-only at its `extension_skills_vfs(name)` root.
    extension_skills: Vec<(String, PathBuf)>,
}

#[async_trait]
impl SessionFileSystemFactory for CodingCliSessionFileSystemFactory {
    fn name(&self) -> &'static str {
        "CodingCliSessionFileSystemFactory"
    }

    async fn create_session_file_system(
        &self,
        _context: SessionFileSystemFactoryContext,
    ) -> everruns_core::Result<Arc<dyn SessionFileSystem>> {
        std::fs::create_dir_all(&self.session_dir).map_err(|e| {
            AgentLoopError::config(format!(
                "create session dir {}: {e}",
                self.session_dir.display()
            ))
        })?;
        // The writable global skills dir may not exist yet; create it so a skill
        // installed mid-session is discoverable. (System skills are already
        // materialized by `SkillDirs::resolve`.)
        for dir in [self.skill_global.as_ref(), self.skill_system.as_ref()]
            .into_iter()
            .flatten()
        {
            std::fs::create_dir_all(dir).map_err(|e| {
                AgentLoopError::config(format!("create skills dir {}: {e}", dir.display()))
            })?;
        }
        let composite: Arc<dyn SessionFileSystem> = Arc::new(CodingCliSessionFileStore::new(
            self.workspace.clone(),
            self.session_dir.clone(),
            self.skill_global.clone(),
            self.skill_system.clone(),
        )?);
        // Present the real host checkout path to the model, not the `/workspace`
        // alias (#258): yolop runs on the user's own machine, so the shell, file
        // tools, and narration must all name the repo the same way. everruns
        // 0.17.12's `scoped_prompt_file_store` preserves this backend-native
        // policy into the system prompt too (via `wrap_if_needed`).
        let mut mounted = MountFs::new(composite).with_backend_display();
        if let Some(skill) = self.environment_skill {
            let environment = Arc::new(InMemorySessionFileStore::new());
            environment
                .seed_initial_file(
                    self.session_id,
                    &InitialFile {
                        path: "/herdr/SKILL.md".to_string(),
                        content: skill.to_string(),
                        encoding: "text".to_string(),
                        is_readonly: true,
                    },
                )
                .await?;
            mounted = mounted.with_mount(
                crate::capabilities::skills::ENVIRONMENT_SKILLS_VFS,
                environment,
                "/",
            );
        }
        // Mount each contributing extension's `skills/` dir read-only. Seeded
        // into an in-memory store (which honors `is_readonly`) rather than a
        // live-disk mount, so an installed extension's skills can't be mutated
        // through the VFS — the same read-only guarantee as environment skills.
        for (name, dir) in &self.extension_skills {
            let store = InMemorySessionFileStore::new();
            if let Err(err) = seed_readonly_dir(&store, self.session_id, dir).await {
                tracing::warn!(
                    target: "yolop::ext",
                    "skipping skills for extension `{name}`: {err}"
                );
                continue;
            }
            mounted = mounted.with_mount(
                crate::capabilities::skills::extension_skills_vfs(name),
                Arc::new(store),
                "/",
            );
        }
        let mounted: Arc<dyn SessionFileSystem> = Arc::new(mounted);
        let write_blocklist: Arc<dyn SessionFileSystem> =
            Arc::new(WriteBlocklistFileStore::new(mounted.clone()));
        Ok(Arc::new(GrepOptionsForwardingFileStore::new(
            write_blocklist,
            mounted,
        )))
    }
}

// TODO(everruns#2830): Remove this adapter after upgrading to the first
// everruns-runtime release containing https://github.com/everruns/everruns/pull/2830.
// Version 0.17.12's write-blocklist decorator drops `grep_files_with_options`;
// all mutations must still pass through that decorator while contextual grep
// can safely use the same mounted read backend directly.
struct GrepOptionsForwardingFileStore {
    policy: Arc<dyn SessionFileSystem>,
    grep_backend: Arc<dyn SessionFileSystem>,
}

impl GrepOptionsForwardingFileStore {
    fn new(policy: Arc<dyn SessionFileSystem>, grep_backend: Arc<dyn SessionFileSystem>) -> Self {
        Self {
            policy,
            grep_backend,
        }
    }
}

#[async_trait]
impl SessionFileSystem for GrepOptionsForwardingFileStore {
    fn display_root(&self) -> String {
        self.policy.display_root()
    }

    fn display_path(&self, path: &str) -> String {
        self.policy.display_path(path)
    }

    fn resolve_path(&self, input: &str) -> String {
        self.policy.resolve_path(input)
    }

    fn is_mount_resolver(&self) -> bool {
        self.policy.is_mount_resolver()
    }

    async fn read_file(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<Option<SessionFile>> {
        self.policy.read_file(session_id, path).await
    }

    async fn write_file(
        &self,
        session_id: SessionId,
        path: &str,
        content: &str,
        encoding: &str,
    ) -> everruns_core::Result<SessionFile> {
        self.policy
            .write_file(session_id, path, content, encoding)
            .await
    }

    async fn write_file_if_content_matches(
        &self,
        session_id: SessionId,
        path: &str,
        expected_content: &str,
        expected_encoding: &str,
        content: &str,
        encoding: &str,
    ) -> everruns_core::Result<Option<SessionFile>> {
        self.policy
            .write_file_if_content_matches(
                session_id,
                path,
                expected_content,
                expected_encoding,
                content,
                encoding,
            )
            .await
    }

    async fn delete_file(
        &self,
        session_id: SessionId,
        path: &str,
        recursive: bool,
    ) -> everruns_core::Result<bool> {
        self.policy.delete_file(session_id, path, recursive).await
    }

    async fn list_directory(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<Vec<FileInfo>> {
        self.policy.list_directory(session_id, path).await
    }

    async fn stat_file(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<Option<FileStat>> {
        self.policy.stat_file(session_id, path).await
    }

    async fn grep_files(
        &self,
        session_id: SessionId,
        pattern: &str,
        path_pattern: Option<&str>,
    ) -> everruns_core::Result<Vec<GrepMatch>> {
        self.policy
            .grep_files(session_id, pattern, path_pattern)
            .await
    }

    async fn grep_files_with_options(
        &self,
        session_id: SessionId,
        pattern: &str,
        options: &GrepOptions,
    ) -> everruns_core::Result<GrepSearchResult> {
        self.grep_backend
            .grep_files_with_options(session_id, pattern, options)
            .await
    }

    async fn create_directory(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<FileInfo> {
        self.policy.create_directory(session_id, path).await
    }

    async fn seed_initial_file(
        &self,
        session_id: SessionId,
        file: &InitialFile,
    ) -> everruns_core::Result<()> {
        self.policy.seed_initial_file(session_id, file).await
    }
}

/// Seed every UTF-8 file under `root` into `store` read-only, keyed by its path
/// relative to `root`. Used to mount an extension's on-disk `skills/` dir as a
/// read-only VFS source. Non-UTF-8 files (rare in skills) are skipped.
async fn seed_readonly_dir(
    store: &InMemorySessionFileStore,
    session_id: SessionId,
    root: &std::path::Path,
) -> everruns_core::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| AgentLoopError::config(format!("read {}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(rel) = path.strip_prefix(root)
            {
                let vfs = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
                store
                    .seed_initial_file(
                        session_id,
                        &InitialFile {
                            path: vfs,
                            content: text,
                            encoding: "text".to_string(),
                            is_readonly: true,
                        },
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

struct CodingCliSessionFileStore {
    workspace: Arc<WorkspaceHost>,
    workspace_disk: RealDiskFileStore,
    session: RealDiskFileStore,
    // Backing stores for the global/system skill scope VFS roots, served from
    // real directories outside the workspace (see `capabilities::skills`).
    skill_global: Option<RealDiskFileStore>,
    skill_system: Option<RealDiskFileStore>,
    session_dir: PathBuf,
}

impl CodingCliSessionFileStore {
    fn new(
        workspace: Arc<WorkspaceHost>,
        session_dir: PathBuf,
        skill_global: Option<PathBuf>,
        skill_system: Option<PathBuf>,
    ) -> everruns_core::Result<Self> {
        let skill_store =
            |dir: Option<PathBuf>| -> everruns_core::Result<Option<RealDiskFileStore>> {
                dir.map(RealDiskFileStore::new).transpose()
            };
        Ok(Self {
            workspace: workspace.clone(),
            workspace_disk: workspace.disk().as_ref().clone(),
            session: RealDiskFileStore::new(session_dir.clone())?,
            skill_global: skill_store(skill_global)?,
            skill_system: skill_store(skill_system)?,
            session_dir,
        })
    }

    /// The shared workspace disk, repointed via `set_host_root` (EVE-660) when
    /// the active worktree changes.
    fn workspace_store(&self) -> everruns_core::Result<&RealDiskFileStore> {
        self.workspace.sync()?;
        Ok(&self.workspace_disk)
    }

    fn skill_or_session_route(&self, path: &str) -> Option<(&RealDiskFileStore, String)> {
        use crate::capabilities::skills::{GLOBAL_SKILLS_VFS, SYSTEM_SKILLS_VFS, relative_under};
        if let Some(store) = &self.skill_global
            && let Some(rest) = relative_under(path, GLOBAL_SKILLS_VFS)
        {
            return Some((store, rest));
        }
        if let Some(store) = &self.skill_system
            && let Some(rest) = relative_under(path, SYSTEM_SKILLS_VFS)
        {
            return Some((store, rest));
        }
        Self::session_artifact_path(path).map(|path| (&self.session, path))
    }

    // Keep project files rooted at the user's workspace, but route generated
    // tool artifacts into yolop's durable per-session folder.
    fn session_artifact_path(path: &str) -> Option<String> {
        let normalized = everruns_core::session_path::to_session_path(path);
        if normalized == "/outputs"
            || normalized.starts_with("/outputs/")
            || normalized == "/.background"
            || normalized.starts_with("/.background/")
        {
            Some(normalized)
        } else {
            None
        }
    }

    #[cfg(unix)]
    fn secure_session_artifact_path(&self, path: &str) -> everruns_core::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let absolute = self.session_dir.join(path.trim_start_matches('/'));

        // Harden every artifact ancestor without crossing above the routed
        // top-level directory into the session root or its parents.
        let artifact_root = self.session_dir.join(
            path.trim_start_matches('/')
                .split('/')
                .next()
                .expect("routed artifact path has a top-level directory"),
        );
        let mut current = absolute.parent();
        while let Some(dir) = current {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
                AgentLoopError::config(format!(
                    "set private permissions on session output dir {}: {e}",
                    dir.display()
                ))
            })?;
            if dir == artifact_root {
                break;
            }
            current = dir.parent();
        }

        std::fs::set_permissions(&absolute, std::fs::Permissions::from_mode(0o600)).map_err(
            |e| {
                AgentLoopError::config(format!(
                    "set private permissions on session output file {}: {e}",
                    absolute.display()
                ))
            },
        )?;

        Ok(())
    }

    #[cfg(not(unix))]
    fn secure_session_artifact_path(&self, _path: &str) -> everruns_core::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl SessionFileSystem for CodingCliSessionFileStore {
    fn is_mount_resolver(&self) -> bool {
        false
    }

    fn display_root(&self) -> String {
        self.workspace_store()
            .map(|store| store.display_root())
            .unwrap_or_else(|_| self.workspace_disk.display_root())
    }

    fn display_path(&self, path: &str) -> String {
        if self.skill_or_session_route(path).is_some() {
            return everruns_core::session_path::to_session_path(path);
        }
        self.workspace_store()
            .map(|store| store.display_path(path))
            .unwrap_or_else(|_| path.to_string())
    }

    async fn read_file(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<Option<SessionFile>> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            return store.read_file(session_id, &path).await;
        }
        self.workspace_store()?.read_file(session_id, path).await
    }

    async fn write_file(
        &self,
        session_id: SessionId,
        path: &str,
        content: &str,
        encoding: &str,
    ) -> everruns_core::Result<SessionFile> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            let file = store
                .write_file(session_id, &path, content, encoding)
                .await?;
            if Self::session_artifact_path(&path).is_some() {
                self.secure_session_artifact_path(&path)?;
            }
            return Ok(file);
        }
        let file = self
            .workspace_store()?
            .write_file(session_id, path, content, encoding)
            .await?;
        Ok(file)
    }

    async fn write_file_if_content_matches(
        &self,
        session_id: SessionId,
        path: &str,
        expected_content: &str,
        expected_encoding: &str,
        content: &str,
        encoding: &str,
    ) -> everruns_core::Result<Option<SessionFile>> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            return store
                .write_file_if_content_matches(
                    session_id,
                    &path,
                    expected_content,
                    expected_encoding,
                    content,
                    encoding,
                )
                .await;
        }
        self.workspace_store()?
            .write_file_if_content_matches(
                session_id,
                path,
                expected_content,
                expected_encoding,
                content,
                encoding,
            )
            .await
    }

    async fn delete_file(
        &self,
        session_id: SessionId,
        path: &str,
        recursive: bool,
    ) -> everruns_core::Result<bool> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            return store.delete_file(session_id, &path, recursive).await;
        }
        self.workspace_store()?
            .delete_file(session_id, path, recursive)
            .await
    }

    async fn list_directory(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<Vec<FileInfo>> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            return store.list_directory(session_id, &path).await;
        }
        self.workspace_store()?
            .list_directory(session_id, path)
            .await
    }

    async fn stat_file(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<Option<FileStat>> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            return store.stat_file(session_id, &path).await;
        }
        self.workspace_store()?.stat_file(session_id, path).await
    }

    async fn grep_files(
        &self,
        session_id: SessionId,
        pattern: &str,
        path_pattern: Option<&str>,
    ) -> everruns_core::Result<Vec<GrepMatch>> {
        if let Some(path) = path_pattern
            && let Some((store, path)) = self.skill_or_session_route(path)
        {
            return store.grep_files(session_id, pattern, Some(&path)).await;
        }
        match path_pattern.and_then(Self::session_artifact_path) {
            Some(path) => {
                self.session
                    .grep_files(session_id, pattern, Some(path.trim_start_matches('/')))
                    .await
            }
            None => {
                let store = self.workspace_store()?;
                store.grep_files(session_id, pattern, path_pattern).await
            }
        }
    }

    async fn grep_files_with_options(
        &self,
        session_id: SessionId,
        pattern: &str,
        options: &GrepOptions,
    ) -> everruns_core::Result<GrepSearchResult> {
        if let Some(path) = options.path_pattern.as_deref()
            && let Some((store, path)) = self.skill_or_session_route(path)
        {
            let mut routed = options.clone();
            routed.path_pattern = Some(path);
            return store
                .grep_files_with_options(session_id, pattern, &routed)
                .await;
        }
        let store = self.workspace_store()?;
        store
            .grep_files_with_options(session_id, pattern, options)
            .await
    }

    async fn create_directory(
        &self,
        session_id: SessionId,
        path: &str,
    ) -> everruns_core::Result<FileInfo> {
        if let Some((store, path)) = self.skill_or_session_route(path) {
            return store.create_directory(session_id, &path).await;
        }
        self.workspace_store()?
            .create_directory(session_id, path)
            .await
    }

    async fn seed_initial_file(
        &self,
        session_id: SessionId,
        file: &InitialFile,
    ) -> everruns_core::Result<()> {
        if let Some((store, path)) = self.skill_or_session_route(&file.path) {
            let mut routed = file.clone();
            routed.path = path;
            return store.seed_initial_file(session_id, &routed).await;
        }
        self.workspace_store()?
            .seed_initial_file(session_id, file)
            .await
    }
}

// ---------- provider selection ----------

const DEFAULT_OPENAI_MODEL: &str = "gpt-5.6-sol";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-4-8";
const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-flash";
// Gemini exposes an OpenAI-compatible surface at this base URL, driven through
// `everruns_openai`. (OpenRouter has its own first-class driver since
// everruns 0.10 — see `model_with_provider`.)
const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-5.6-sol";
const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";
const DEFAULT_OLLAMA_API_KEY: &str = "ollama";
// Generic OpenAI-compatible servers usually ignore the bearer token, but the
// OpenAI client requires one — same trick as Ollama's placeholder key.
const DEFAULT_CUSTOM_API_KEY: &str = "unused";
const YOLOP_NEVER_DEFER_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_directory",
    "grep_files",
    "search_sessions",
    "ast_grep",
    "ast_edit",
    "bash",
    "spawn_background",
    "write_todos",
    "write_session_title",
    // Skill activation is a routing tool with required arguments; hiding its
    // tiny schema makes malformed calls like activate_skill({}) more likely.
    "activate_skill",
    "run_yolop_command",
    // LSP tools exist only when the optional `lsp` capability is enabled
    // (absent names are ignored by the allowlist). When they exist, stub
    // schemas behind `tool_search` add enough friction that models fall back
    // to grep instead of adopting them (measured in evals/lsp_integration),
    // so keep their real schemas loaded.
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_diagnostics",
    "lsp_rename",
    "lsp_symbols",
    "lsp_code_actions",
];
#[derive(Clone, Debug)]
pub enum ProviderChoice {
    Anthropic {
        model: String,
        reasoning_effort: Option<String>,
    },
    OpenAi {
        model: String,
        reasoning_effort: Option<String>,
    },
    Codex {
        model: String,
        reasoning_effort: Option<String>,
    },
    Google {
        model: String,
        base_url: String,
        reasoning_effort: Option<String>,
    },
    OpenRouter {
        model: String,
        base_url: String,
        reasoning_effort: Option<String>,
    },
    Ollama {
        model: String,
        base_url: String,
        reasoning_effort: Option<String>,
    },
    /// Generic OpenAI-compatible endpoint (vLLM, llama.cpp, LM Studio,
    /// hosted gateways, …). Unlike the other variants the base URL is not
    /// carried here: it is user configuration, resolved from
    /// `CUSTOM_BASE_URL` or the settings file at request-build time in
    /// [`Self::model_with_provider`], so a bare `custom/model` spec can be
    /// parsed without access to settings.
    Custom {
        model: String,
        reasoning_effort: Option<String>,
    },
    Sim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReasoningEffortOption {
    pub value: String,
    pub label: String,
}

/// Canonical provider identity: the single source of truth for the set of
/// providers and the name/driver mapping. `ProviderChoice` carries the live
/// model/effort/base-url for a chosen provider; `Provider` is just the identity
/// (one per [`ProviderChoice`] variant). Settings TOML and env vars still use
/// the string form at their boundary — `as_str`/`from_name` are the only place
/// that conversion happens, instead of string matches scattered across the
/// resolver, driver lookup, and a separate hardcoded name list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Codex,
    Anthropic,
    Google,
    OpenRouter,
    Ollama,
    Custom,
    Sim,
}

impl Provider {
    /// Every provider, in the user-visible suggestion order.
    pub const ALL: [Provider; 8] = [
        Provider::OpenAi,
        Provider::Codex,
        Provider::Anthropic,
        Provider::Google,
        Provider::OpenRouter,
        Provider::Ollama,
        Provider::Custom,
        Provider::Sim,
    ];

    /// Short name used in settings, env, and command suggestions.
    pub const fn as_str(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Codex => "codex",
            Provider::Anthropic => "anthropic",
            Provider::Google => "google",
            Provider::OpenRouter => "openrouter",
            Provider::Ollama => "ollama",
            Provider::Custom => "custom",
            Provider::Sim => "llmsim",
        }
    }

    /// Parse a provider name (case-insensitive, trimmed). `None` for any name
    /// outside [`Provider::ALL`].
    pub fn from_name(name: &str) -> Option<Provider> {
        let name = name.trim();
        Provider::ALL
            .into_iter()
            .find(|p| p.as_str().eq_ignore_ascii_case(name))
    }

    /// The driver that serves this provider, when it maps to one of the
    /// registered first-class drivers. Providers without a dedicated driver
    /// (codex, ollama, custom, llmsim) return `None`.
    pub fn driver_id(self) -> Option<DriverId> {
        match self {
            Provider::Anthropic => Some(DriverId::Anthropic),
            Provider::OpenAi | Provider::Google => Some(DriverId::OpenAI),
            Provider::OpenRouter => Some(DriverId::OpenRouter),
            Provider::Codex | Provider::Ollama | Provider::Custom | Provider::Sim => None,
        }
    }
}

/// Provider names recognized by `/setup` and persisted settings, in the
/// user-visible suggestion order. Spelled in terms of [`Provider`] so the set
/// has one source of truth (a round-trip test locks the two together).
pub const SUPPORTED_PROVIDERS: &[&str] = &[
    Provider::OpenAi.as_str(),
    Provider::Codex.as_str(),
    Provider::Anthropic.as_str(),
    Provider::Google.as_str(),
    Provider::OpenRouter.as_str(),
    Provider::Ollama.as_str(),
    Provider::Custom.as_str(),
    Provider::Sim.as_str(),
];

impl ProviderChoice {
    /// Pick a default from env vars or settings-stored tokens. CLI flags
    /// override this in `main`. OpenAI is preferred when both an OpenAI
    /// and Anthropic credential are present, and it is also the no-credential
    /// first-run default so llmsim is only selected explicitly.
    pub fn from_env_or_settings(settings: &Settings) -> Self {
        if env_non_empty("OPENAI_API_KEY").is_some() || settings.has_token("openai") {
            return Self::default_openai();
        }
        if env_non_empty("CODEX_ACCESS_TOKEN").is_some() || settings.has_codex_auth() {
            return Self::default_codex();
        }
        if env_non_empty("ANTHROPIC_API_KEY").is_some() || settings.has_token("anthropic") {
            return Self::default_anthropic();
        }
        if env_non_empty("OPENROUTER_API_KEY").is_some() || settings.has_token("openrouter") {
            return Self::default_openrouter();
        }
        if google_api_key().is_some() || settings.has_token("google") {
            return Self::default_google();
        }
        if env_non_empty("OLLAMA_BASE_URL").is_some()
            || env_non_empty("OLLAMA_API_KEY").is_some()
            || settings.has_token("ollama")
        {
            return Self::default_ollama();
        }
        // The custom endpoint has no default model, so it is auto-selected
        // only when a model is also known (env override or a persisted
        // `[models].custom` pick — applied by the caller's
        // `resolve_for_settings`). Otherwise a non-interactive run would send a
        // Chat Completions request with an empty model id.
        if (env_non_empty("CUSTOM_BASE_URL").is_some() || settings.base_url_for("custom").is_some())
            && (env_non_empty("EVERRUNS_CLI_MODEL").is_some()
                || settings.model_for("custom").is_some())
        {
            return Self::default_custom();
        }
        Self::default_openai()
    }

    fn default_openai() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", DEFAULT_OPENAI_MODEL);
        Self::OpenAi {
            reasoning_effort: default_reasoning_effort(&DriverId::OpenAI, &model),
            model,
        }
    }

    fn default_codex() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", DEFAULT_CODEX_MODEL);
        Self::Codex {
            reasoning_effort: normalize_reasoning_effort(env_non_empty(
                "EVERRUNS_CLI_REASONING_EFFORT",
            ))
            .or_else(|| {
                crate::drivers::codex::model_profile(&model)
                    .and_then(|profile| profile.reasoning_effort)
                    .and_then(|config| reasoning_effort_value(&config.default))
            }),
            model,
        }
    }

    fn default_anthropic() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", DEFAULT_ANTHROPIC_MODEL);
        Self::Anthropic {
            reasoning_effort: default_reasoning_effort(&DriverId::Anthropic, &model),
            model,
        }
    }

    fn default_google() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", DEFAULT_GOOGLE_MODEL);
        Self::Google {
            base_url: env_or_default("GOOGLE_BASE_URL", DEFAULT_GOOGLE_BASE_URL),
            reasoning_effort: default_reasoning_effort(&DriverId::OpenAI, &model),
            model,
        }
    }

    fn default_openrouter() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", DEFAULT_OPENROUTER_MODEL);
        Self::OpenRouter {
            base_url: env_or_default("OPENROUTER_BASE_URL", DEFAULT_OPENROUTER_BASE_URL),
            reasoning_effort: default_reasoning_effort(&DriverId::OpenRouter, &model),
            model,
        }
    }

    fn default_ollama() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", DEFAULT_OLLAMA_MODEL);
        Self::Ollama {
            base_url: env_or_default("OLLAMA_BASE_URL", DEFAULT_OLLAMA_BASE_URL),
            reasoning_effort: default_reasoning_effort(&DriverId::OpenAI, &model),
            model,
        }
    }

    /// The custom endpoint has no default model; callers gate on a model being
    /// known before selecting it, and an empty model is rejected downstream.
    fn default_custom() -> Self {
        let model = env_or_default("EVERRUNS_CLI_MODEL", "");
        Self::Custom {
            reasoning_effort: default_reasoning_effort(&DriverId::OpenAICompletions, &model),
            model,
        }
    }

    pub fn label(&self) -> String {
        let mut label = format!("{}/{}", self.provider_name(), self.model_id());
        if let Some(effort) = self.reasoning_effort() {
            label.push(' ');
            label.push_str(effort);
        }
        label
    }

    /// The canonical provider identity for this choice.
    pub fn provider(&self) -> Provider {
        match self {
            Self::Anthropic { .. } => Provider::Anthropic,
            Self::OpenAi { .. } => Provider::OpenAi,
            Self::Codex { .. } => Provider::Codex,
            Self::Google { .. } => Provider::Google,
            Self::OpenRouter { .. } => Provider::OpenRouter,
            Self::Ollama { .. } => Provider::Ollama,
            Self::Custom { .. } => Provider::Custom,
            Self::Sim => Provider::Sim,
        }
    }

    /// Short name used in settings and command suggestions.
    pub fn provider_name(&self) -> &'static str {
        self.provider().as_str()
    }

    /// Build a ProviderChoice from a bare provider name, picking the
    /// provider's default model. Used by `/setup` and by startup when
    /// rehydrating the persisted preference.
    pub fn default_for_provider_name(name: &str) -> Result<Self> {
        let provider = Provider::from_name(name).ok_or_else(|| {
            anyhow!(
                "unknown provider {name}; expected one of {}",
                SUPPORTED_PROVIDERS.join(", ")
            )
        })?;
        match provider {
            Provider::OpenAi => Ok(Self::default_openai()),
            Provider::Codex => Ok(Self::default_codex()),
            Provider::Anthropic => Ok(Self::default_anthropic()),
            Provider::Google => Ok(Self::default_google()),
            Provider::OpenRouter => Ok(Self::default_openrouter()),
            Provider::Ollama => Ok(Self::default_ollama()),
            // No sensible default model exists for an arbitrary endpoint; an
            // empty model is rejected later by `model_with_provider` so the
            // setup wizard (or a saved model from settings) must fill it in.
            Provider::Custom => Ok(Self::default_custom()),
            Provider::Sim => Ok(Self::Sim),
        }
    }

    /// Provider name used when previewing config changes before the next run.
    pub fn preview_provider_name(settings: &Settings) -> String {
        settings.default_provider.clone().unwrap_or_else(|| {
            Self::from_env_or_settings(settings)
                .provider_name()
                .to_string()
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelResolutionSource {
    ProviderDefault,
    PerProviderModel,
    DefaultModel,
    EnvOverride,
}

#[derive(Clone, Debug)]
pub struct ResolvedProviderChoice {
    pub choice: ProviderChoice,
    pub source: ModelResolutionSource,
    pub notes: Vec<String>,
}

impl ResolvedProviderChoice {
    pub fn next_run_preview(&self) -> String {
        let label = self.choice.label();
        let mut preview = match self.source {
            ModelResolutionSource::PerProviderModel => {
                let provider = self.choice.provider_name();
                format!("→ next run: {label} (from models.{provider})")
            }
            ModelResolutionSource::DefaultModel => {
                format!("→ next run: {label} (from default_model)")
            }
            ModelResolutionSource::EnvOverride => {
                format!("→ next run: {label} (EVERRUNS_CLI_MODEL env)")
            }
            ModelResolutionSource::ProviderDefault => {
                format!("→ next run: {label} (provider default)")
            }
        };
        for note in &self.notes {
            preview.push('\n');
            preview.push_str("→ ");
            preview.push_str(note);
        }
        preview
    }
}

fn bare_model_id_from_spec(spec: &str) -> &str {
    spec.split_whitespace().next().unwrap_or(spec)
}

fn driver_id_for_provider_name(provider: &str) -> Option<DriverId> {
    Provider::from_name(provider).and_then(Provider::driver_id)
}

/// Whether a bare model id plausibly belongs to the given provider. Used to
/// gate the cross-provider `default_model` fallback so an OpenAI pick is not
/// silently applied after switching to Anthropic.
pub fn model_compatible_with_provider(model_id: &str, provider: &str) -> bool {
    match provider {
        "openrouter" | "ollama" | "custom" => true,
        "llmsim" => model_id == "llmsim-yolop",
        "anthropic" | "openai" | "google" => {
            let bare = model_id.strip_suffix("[1m]").unwrap_or(model_id);
            if ProviderChoice::model_suggestions_for_provider(provider)
                .iter()
                .any(|s| bare_model_id_from_spec(s) == bare)
            {
                return true;
            }
            if let Some(driver) = driver_id_for_provider_name(provider)
                && get_model_profile(&driver, bare).is_some()
            {
                return true;
            }
            match provider {
                "anthropic" => bare.starts_with("claude-"),
                "openai" => {
                    bare.starts_with("gpt-")
                        || bare.starts_with("o1")
                        || bare.starts_with("o3")
                        || bare.starts_with("o4")
                        || bare.starts_with("codex")
                }
                "google" => bare.starts_with("gemini-"),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Resolve a provider plus its model from persisted settings.
///
/// Priority: `EVERRUNS_CLI_MODEL` env → `models.<provider>` → compatible
/// `default_model` → the provider's built-in default.
pub fn resolve_for_settings(provider: &str, settings: &Settings) -> Result<ResolvedProviderChoice> {
    let base = ProviderChoice::default_for_provider_name(provider)?;
    let base_label = base.label();
    if env_non_empty("EVERRUNS_CLI_MODEL").is_some() {
        return Ok(ResolvedProviderChoice {
            choice: base,
            source: ModelResolutionSource::EnvOverride,
            notes: vec![],
        });
    }

    let provider_name = base.provider_name();

    if let Some(spec) = settings.model_for(provider_name) {
        return match base.resolve_model_spec(spec) {
            Ok(choice) => Ok(ResolvedProviderChoice {
                choice,
                source: ModelResolutionSource::PerProviderModel,
                notes: vec![],
            }),
            Err(err) => Ok(ResolvedProviderChoice {
                choice: base,
                source: ModelResolutionSource::ProviderDefault,
                notes: vec![format!(
                    "ignored models.{provider_name} \"{spec}\": {err}; using {base_label}"
                )],
            }),
        };
    }

    if let Some(spec) = settings.default_model() {
        let model_id = bare_model_id_from_spec(spec);
        if !model_compatible_with_provider(model_id, provider_name) {
            return Ok(ResolvedProviderChoice {
                choice: base,
                source: ModelResolutionSource::ProviderDefault,
                notes: vec![format!(
                    "default_model \"{spec}\" ignored for {provider_name} (not a recognized \
                     {provider_name} model); using {base_label}"
                )],
            });
        }
        return match base.resolve_model_spec(spec) {
            Ok(choice) => Ok(ResolvedProviderChoice {
                choice,
                source: ModelResolutionSource::DefaultModel,
                notes: vec![],
            }),
            Err(err) => Ok(ResolvedProviderChoice {
                choice: base,
                source: ModelResolutionSource::ProviderDefault,
                notes: vec![format!(
                    "default_model \"{spec}\" ignored for {provider_name}: {err}; using \
                     {base_label}"
                )],
            }),
        };
    }

    Ok(ResolvedProviderChoice {
        choice: base,
        source: ModelResolutionSource::ProviderDefault,
        notes: vec![],
    })
}

impl ProviderChoice {
    pub fn model_id(&self) -> &str {
        match self {
            Self::Anthropic { model, .. }
            | Self::OpenAi { model, .. }
            | Self::Codex { model, .. }
            | Self::Google { model, .. }
            | Self::OpenRouter { model, .. }
            | Self::Ollama { model, .. }
            | Self::Custom { model, .. } => model,
            Self::Sim => "llmsim-yolop",
        }
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort_value().and_then(Option::as_deref)
    }

    pub(crate) fn reasoning_effort_options(&self) -> Vec<ReasoningEffortOption> {
        self.reasoning_effort_config()
            .map(|config| config.values.iter().map(reasoning_effort_option).collect())
            .unwrap_or_default()
    }

    pub(crate) fn default_reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort_config()
            .and_then(|config| reasoning_effort_value(&config.default))
    }

    fn reasoning_effort_config(&self) -> Option<ReasoningEffortConfig> {
        self.model_profile()?.reasoning_effort
    }

    fn model_profile(&self) -> Option<ModelProfile> {
        match self {
            Self::Codex { model, .. } => crate::drivers::codex::model_profile(model),
            _ => {
                let resolved = self.model_without_stored_key();
                local_model_profile(&resolved.provider_type, &resolved.model)
                    .or_else(|| get_model_profile(&resolved.provider_type, &resolved.model))
            }
        }
    }

    fn reasoning_effort_value(&self) -> Option<&Option<String>> {
        match self {
            Self::OpenAi {
                reasoning_effort, ..
            }
            | Self::Anthropic {
                reasoning_effort, ..
            }
            | Self::Codex {
                reasoning_effort, ..
            }
            | Self::Google {
                reasoning_effort, ..
            }
            | Self::OpenRouter {
                reasoning_effort, ..
            }
            | Self::Ollama {
                reasoning_effort, ..
            }
            | Self::Custom {
                reasoning_effort, ..
            } => Some(reasoning_effort),
            _ => None,
        }
    }

    /// Provider-relative model spec (`<model> [effort]`) — the label without
    /// the `provider/` prefix. This is the form `/setup model` accepts and
    /// the form persisted under `[models]` in settings.
    pub fn model_spec(&self) -> String {
        self.label()
            .strip_prefix(&format!("{}/", self.provider_name()))
            .map(str::to_string)
            .unwrap_or_else(|| self.model_id().to_string())
    }

    pub fn model_suggestions_for_provider(provider: &str) -> &'static [&'static str] {
        match provider {
            "openai" => &[
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
                "gpt-5.2",
            ],
            "codex" => &[
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
                "gpt-5.3-codex-spark",
            ],
            "anthropic" => &[
                "claude-sonnet-4-5",
                "claude-opus-4-5",
                "claude-haiku-4-5",
                "claude-sonnet-4-6",
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-fable-5",
                // `[1m]` ids are the 1M-context twins of the 200K base models;
                // the everruns-anthropic driver strips the suffix on the wire
                // and requests the window via the `context-1m` beta header.
                "claude-fable-5[1m]",
                "claude-opus-4-8[1m]",
            ],
            "google" => &["gemini-2.5-flash", "gemini-2.5-pro"],
            "openrouter" => &[
                "openai/gpt-5.6-sol",
                "openai/gpt-5.6-terra",
                "openai/gpt-5.6-luna",
                "openai/gpt-5.5",
                "anthropic/claude-opus-4-8",
                "nvidia/nemotron-3-super-120b-a12b high",
            ],
            "ollama" => &["llama3.2"],
            "llmsim" => &["llmsim-yolop"],
            _ => &[],
        }
    }

    pub(crate) fn resolve_model_spec(&self, spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let mut parts = spec.split_whitespace();
        let model_spec = parts.next().unwrap_or_default();
        let reasoning_effort = parts.next().map(str::to_string);
        if parts.next().is_some() {
            return Err(anyhow!("too many model arguments; use `gpt-5.5 medium`"));
        }
        self.with_current_provider_model(model_spec.to_string(), reasoning_effort)
    }

    fn with_current_provider_model(
        &self,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<Self> {
        if model.trim().is_empty() {
            return Err(anyhow!("model id is required"));
        }
        match self {
            Self::Anthropic { .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::Anthropic {
                    model,
                    reasoning_effort,
                })
            }
            Self::OpenAi { .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::OpenAi {
                    model,
                    reasoning_effort,
                })
            }
            Self::Codex { .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::Codex {
                    model,
                    reasoning_effort,
                })
            }
            Self::Google { base_url, .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::Google {
                    model,
                    base_url: base_url.clone(),
                    reasoning_effort,
                })
            }
            Self::OpenRouter { base_url, .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::OpenRouter {
                    model,
                    base_url: base_url.clone(),
                    reasoning_effort,
                })
            }
            Self::Ollama { base_url, .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::Ollama {
                    model,
                    base_url: base_url.clone(),
                    reasoning_effort,
                })
            }
            Self::Custom { .. } => {
                let reasoning_effort =
                    self.resolve_model_reasoning_effort(&model, reasoning_effort)?;
                Ok(Self::Custom {
                    model,
                    reasoning_effort,
                })
            }
            Self::Sim => {
                if reasoning_effort.is_some() {
                    return Err(anyhow!("offline llmsim does not support reasoning effort"));
                }
                if model == "llmsim-yolop" {
                    Ok(Self::Sim)
                } else {
                    Err(anyhow!("offline llmsim only supports llmsim-yolop"))
                }
            }
        }
    }

    fn resolve_model_reasoning_effort(
        &self,
        model: &str,
        reasoning_effort: Option<String>,
    ) -> Result<Option<String>> {
        let requested = normalize_reasoning_effort(reasoning_effort);
        let Some(config) = self.reasoning_effort_config_for_model(model) else {
            return Ok(requested);
        };
        let allowed = config
            .values
            .iter()
            .filter_map(|option| reasoning_effort_value(&option.value))
            .collect::<Vec<_>>();
        if let Some(effort) = requested {
            if allowed.iter().any(|allowed| allowed == &effort) {
                return Ok(Some(effort));
            }
            return Err(anyhow!(
                "model {} supports reasoning efforts: {}",
                self.model_label_for(model),
                allowed.join(", ")
            ));
        }
        reasoning_effort_value(&config.default)
            .ok_or_else(|| {
                anyhow!(
                    "model {} has an invalid profile default",
                    self.model_label_for(model)
                )
            })
            .map(Some)
    }

    fn reasoning_effort_config_for_model(&self, model: &str) -> Option<ReasoningEffortConfig> {
        match self {
            Self::Codex { .. } => crate::drivers::codex::model_profile(model),
            _ => {
                let resolved = self.model_without_stored_key_for_model(model);
                get_model_profile(&resolved.provider_type, &resolved.model)
            }
        }
        .and_then(|profile| profile.reasoning_effort)
    }

    fn model_label_for(&self, model: &str) -> String {
        format!("{}/{}", self.provider_name(), model)
    }

    pub(crate) fn resolve_reasoning_effort(&self, raw: &str) -> Result<Self> {
        let mut parts = raw.split_whitespace();
        let effort = parts.next().unwrap_or_default();
        if effort.is_empty() || parts.next().is_some() {
            let suggestions = self
                .reasoning_effort_options()
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "expected one reasoning effort (suggestions: {})",
                if suggestions.is_empty() {
                    "none".to_string()
                } else {
                    suggestions
                }
            ));
        }
        self.with_current_provider_model(self.model_id().to_string(), Some(effort.to_string()))
    }

    pub(crate) fn model_with_provider(&self, settings: &Settings) -> Result<ResolvedModel> {
        match self {
            ProviderChoice::Anthropic { model, .. } => {
                let key = resolve_token(settings, "anthropic", &["ANTHROPIC_API_KEY"])
                    .ok_or_else(|| anyhow!("ANTHROPIC_API_KEY not set (and no token stored)"))?;
                Ok(ResolvedModel {
                    model: model.clone(),
                    provider_type: DriverId::Anthropic,
                    provider_metadata: None,
                    api_key: Some(key),
                    base_url: None,
                })
            }
            ProviderChoice::OpenAi { model, .. } => {
                let key = resolve_token(settings, "openai", &["OPENAI_API_KEY"])
                    .ok_or_else(|| anyhow!("OPENAI_API_KEY not set (and no token stored)"))?;
                Ok(ResolvedModel {
                    model: model.clone(),
                    provider_type: DriverId::OpenAI,
                    provider_metadata: None,
                    api_key: Some(key),
                    base_url: None,
                })
            }
            ProviderChoice::Codex { model, .. } => {
                let auth_from_settings = settings.codex_auth();
                let access_token = env_non_empty("CODEX_ACCESS_TOKEN")
                    .or_else(|| auth_from_settings.map(|auth| auth.access_token.clone()))
                    .ok_or_else(|| {
                        anyhow!("CODEX_ACCESS_TOKEN not set and no Codex login stored")
                    })?;
                let account_id = auth_from_settings
                    .and_then(|auth| auth.account_id.clone())
                    .or_else(|| crate::auth::codex::extract_account_id(&access_token));
                let refresh_token = auth_from_settings.and_then(|auth| auth.refresh_token.clone());
                let expires_at = auth_from_settings.and_then(|auth| auth.expires_at);
                Ok(ResolvedModel {
                    model: model.clone(),
                    provider_type: DriverId::external(crate::drivers::codex::CODEX_DRIVER_ID),
                    provider_metadata: Some(ProviderMetadata {
                        refresh_token,
                        account_id,
                        extra: Some(serde_json::json!({
                            "expires_at": expires_at,
                        })),
                    }),
                    api_key: Some(access_token),
                    base_url: None,
                })
            }
            ProviderChoice::Google {
                model, base_url, ..
            } => {
                let key = resolve_token(settings, "google", &["GEMINI_API_KEY", "GOOGLE_API_KEY"])
                    .ok_or_else(|| {
                        anyhow!("GEMINI_API_KEY (or GOOGLE_API_KEY) not set (and no token stored)")
                    })?;
                Ok(ResolvedModel {
                    model: model.clone(),
                    provider_type: DriverId::OpenAI,
                    provider_metadata: None,
                    api_key: Some(key),
                    base_url: Some(base_url.clone()),
                })
            }
            ProviderChoice::OpenRouter {
                model, base_url, ..
            } => {
                let key = resolve_token(settings, "openrouter", &["OPENROUTER_API_KEY"])
                    .ok_or_else(|| anyhow!("OPENROUTER_API_KEY not set (and no token stored)"))?;
                Ok(ResolvedModel {
                    model: model.clone(),
                    // First-class OpenRouter driver (everruns 0.10+). It speaks
                    // OpenRouter's OpenAI-compatible Responses API but knows the
                    // endpoint is stateless (`previous_response_id` is silently
                    // ignored), so it replays the full transcript each turn
                    // instead of chaining by response id, and it looks up model
                    // profiles under the OpenRouter provider so OpenAI-only
                    // extensions (native phases, hosted tool_search) are never
                    // sent to the gateway. This replaces the earlier Chat
                    // Completions workaround for the stateless endpoint.
                    provider_type: DriverId::OpenRouter,
                    provider_metadata: None,
                    api_key: Some(key),
                    base_url: Some(base_url.clone()),
                })
            }
            ProviderChoice::Ollama {
                model, base_url, ..
            } => {
                let key = resolve_token(settings, "ollama", &["OLLAMA_API_KEY"])
                    .unwrap_or_else(|| DEFAULT_OLLAMA_API_KEY.to_string());
                Ok(ResolvedModel {
                    model: model.clone(),
                    provider_type: DriverId::OpenAI,
                    provider_metadata: None,
                    api_key: Some(key),
                    base_url: Some(base_url.clone()),
                })
            }
            ProviderChoice::Custom { model, .. } => {
                let base_url = custom_base_url(settings).ok_or_else(|| {
                    anyhow!("custom endpoint base URL not set (set CUSTOM_BASE_URL or run /setup)")
                })?;
                // An empty model is deliberately not rejected here: model
                // discovery builds this config before a model is chosen.
                // `/setup` validates the model separately on switch.
                // Chat Completions is the lowest common denominator that
                // virtually every OpenAI-compatible server implements; the
                // Responses driver would break on most of them.
                let key = resolve_token(settings, "custom", &["CUSTOM_API_KEY"])
                    .unwrap_or_else(|| DEFAULT_CUSTOM_API_KEY.to_string());
                Ok(ResolvedModel {
                    model: model.clone(),
                    provider_type: DriverId::OpenAICompletions,
                    provider_metadata: None,
                    api_key: Some(key),
                    base_url: Some(base_url),
                })
            }
            ProviderChoice::Sim => Ok(ResolvedModel {
                model: "llmsim-yolop".into(),
                provider_type: DriverId::LlmSim,
                provider_metadata: None,
                api_key: Some("fake-key".into()),
                base_url: None,
            }),
        }
    }

    fn model_without_stored_key(&self) -> ResolvedModel {
        match self {
            ProviderChoice::Anthropic { model, .. } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::Anthropic,
                provider_metadata: None,
                api_key: None,
                base_url: None,
            },
            ProviderChoice::OpenAi { model, .. } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::OpenAI,
                provider_metadata: None,
                api_key: None,
                base_url: None,
            },
            ProviderChoice::Codex { model, .. } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::external(crate::drivers::codex::CODEX_DRIVER_ID),
                provider_metadata: None,
                api_key: None,
                base_url: None,
            },
            ProviderChoice::Google {
                model, base_url, ..
            } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::OpenAI,
                provider_metadata: None,
                api_key: None,
                base_url: Some(base_url.clone()),
            },
            // First-class OpenRouter driver — see the keyed path in
            // `model_with_provider` for the full rationale.
            ProviderChoice::OpenRouter {
                model, base_url, ..
            } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::OpenRouter,
                provider_metadata: None,
                api_key: None,
                base_url: Some(base_url.clone()),
            },
            ProviderChoice::Ollama {
                model, base_url, ..
            } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::OpenAI,
                provider_metadata: None,
                api_key: Some(DEFAULT_OLLAMA_API_KEY.to_string()),
                base_url: Some(base_url.clone()),
            },
            ProviderChoice::Custom { model, .. } => ResolvedModel {
                model: model.clone(),
                provider_type: DriverId::OpenAICompletions,
                provider_metadata: None,
                api_key: None,
                base_url: env_non_empty("CUSTOM_BASE_URL"),
            },
            ProviderChoice::Sim => ResolvedModel {
                model: "llmsim-yolop".into(),
                provider_type: DriverId::LlmSim,
                provider_metadata: None,
                api_key: Some("fake-key".into()),
                base_url: None,
            },
        }
    }

    fn model_without_stored_key_for_model(&self, model: &str) -> ResolvedModel {
        let mut resolved = self.model_without_stored_key();
        resolved.model = model.to_string();
        resolved
    }

    fn input_message(&self, text: impl Into<String>) -> InputMessage {
        self.input_message_with_parts(vec![ContentPart::text(text)])
    }

    fn input_message_with_parts(&self, mut parts: Vec<ContentPart>) -> InputMessage {
        parts.retain(|part| match part {
            ContentPart::Text(text) => !text.text.trim().is_empty(),
            _ => true,
        });
        let mut input = InputMessage {
            role: MessageRole::User,
            content: parts,
            controls: None,
            metadata: None,
            tags: vec![],
        };
        if let Some(effort) = self.reasoning_effort() {
            input.controls = Some(Controls {
                reasoning: Some(ReasoningConfig {
                    effort: Some(effort.to_string()),
                }),
                ..Default::default()
            });
        }
        input
    }

    fn input_message_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ContentPart>,
    ) -> InputMessage {
        let images = images.into_iter();
        let mut parts = Vec::with_capacity(1 + images.size_hint().0);
        let text = text.into();
        if !text.trim().is_empty() {
            parts.push(ContentPart::text(text));
        }
        parts.extend(images);
        self.input_message_with_parts(parts)
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Gemini's OpenAI-compatible API accepts either `GEMINI_API_KEY` or
/// `GOOGLE_API_KEY`; the Google docs lean on `GEMINI_API_KEY` so it wins.
fn google_api_key() -> Option<String> {
    env_non_empty("GEMINI_API_KEY").or_else(|| env_non_empty("GOOGLE_API_KEY"))
}

/// Base URL for the generic OpenAI-compatible provider. Env beats the
/// settings file, mirroring token resolution.
pub(crate) fn custom_base_url(settings: &Settings) -> Option<String> {
    env_non_empty("CUSTOM_BASE_URL").or_else(|| settings.base_url_for("custom").map(str::to_string))
}

/// Env vars beat settings — a per-run override always wins over a saved
/// token, so a developer can point yolop at a scratch key without editing
/// the settings file.
fn resolve_token(settings: &Settings, provider: &str, env_names: &[&str]) -> Option<String> {
    for name in env_names {
        if let Some(value) = env_non_empty(name) {
            return Some(value);
        }
    }
    settings.token_for(provider).map(str::to_string)
}

fn env_or_default(name: &str, default: &str) -> String {
    env_non_empty(name).unwrap_or_else(|| default.to_string())
}

pub(crate) fn normalize_reasoning_effort(reasoning_effort: Option<String>) -> Option<String> {
    reasoning_effort
        .map(|effort| effort.trim().to_ascii_lowercase())
        .filter(|effort| !effort.is_empty())
}

fn model_profile(provider_type: &DriverId, model: &str) -> Option<ModelProfile> {
    local_model_profile(provider_type, model).or_else(|| get_model_profile(provider_type, model))
}

fn local_model_profile(provider_type: &DriverId, model: &str) -> Option<ModelProfile> {
    if *provider_type == DriverId::OpenAI
        && matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna")
    {
        return Some(GPT_5_6_PROFILE.clone());
    }

    None
}

static GPT_5_6_PROFILE: LazyLock<ModelProfile> = LazyLock::new(|| {
    let mut profile = get_model_profile(&DriverId::OpenAI, "gpt-5.5")
        .or_else(|| get_model_profile(&DriverId::OpenAI, "gpt-5.4"))
        .expect("GPT-5 profile metadata should be available")
        .clone();
    profile.name = "GPT-5.6".to_string();
    profile.family = "gpt-5.6".to_string();
    profile
});

fn profile_default_reasoning_effort(provider_type: &DriverId, model: &str) -> Option<String> {
    model_profile(provider_type, model)
        .and_then(|profile| profile.reasoning_effort.clone())
        .and_then(|config| reasoning_effort_value(&config.default))
}

/// The reasoning effort for a fresh provider default: the `EVERRUNS_CLI_REASONING_EFFORT`
/// override if set, else the model profile's default for `provider_type`.
fn default_reasoning_effort(provider_type: &DriverId, model: &str) -> Option<String> {
    normalize_reasoning_effort(env_non_empty("EVERRUNS_CLI_REASONING_EFFORT"))
        .or_else(|| profile_default_reasoning_effort(provider_type, model))
}

fn reasoning_effort_option(value: &ReasoningEffortValue) -> ReasoningEffortOption {
    ReasoningEffortOption {
        value: reasoning_effort_value(&value.value).unwrap_or_default(),
        label: value.name.clone(),
    }
}

fn reasoning_effort_value(value: &everruns_core::ReasoningEffort) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
}

// Integration capabilities whose crates do not export an id constant
// (`everruns-integrations-*` identify themselves with a bare string). Mirror
// them here so the harness wiring references a single named source of truth
// instead of scattering string literals.
pub(crate) const DUCKDUCKGO_CAPABILITY_ID: &str = "duckduckgo";
pub(crate) const DAYTONA_CAPABILITY_ID: &str = "daytona";
pub(crate) const COMPACTION_BUDGET_PERCENT: u8 = 85;

fn default_coding_harness_capabilities(client_commands: bool) -> Vec<AgentCapabilityConfig> {
    let mut caps = Vec::new();
    // Terminal-side commands lead the registry so the most-typed commands
    // (/help, !shell, /clear, /quit, …) surface first in the palette. Enabled only
    // when the host registered the capability that backs them (the TUI);
    // enabling an unregistered id would have nothing to dispatch to.
    if client_commands {
        caps.push(AgentCapabilityConfig::new(CLIENT_COMMANDS_CAPABILITY_ID));
    }
    caps.extend([
        AgentCapabilityConfig::with_config(
            SESSION_CAPABILITY_ID,
            serde_json::json!({ "auto_title": true }),
        ),
        AgentCapabilityConfig::new(SESSION_FILE_SYSTEM_CAPABILITY_ID),
        AgentCapabilityConfig::new(SKILLS_CAPABILITY_ID),
        AgentCapabilityConfig::new(HERDR_CAPABILITY_ID),
        AgentCapabilityConfig::new(REPO_MAP_CAPABILITY_ID),
        AgentCapabilityConfig::new(SESSION_HISTORY_CAPABILITY_ID),
        AgentCapabilityConfig::new(CHECKPOINT_CAPABILITY_ID),
        AgentCapabilityConfig::new(AST_GREP_CAPABILITY_ID),
        // Raw history stays searchable while the runtime persists a canonical
        // provider-native replacement plus the uncompacted suffix. Auto keeps
        // provider-neutral fallbacks for drivers without native compaction.
        AgentCapabilityConfig::new(INFINITY_CONTEXT_CAPABILITY_ID),
        AgentCapabilityConfig::with_config(
            COMPACTION_CAPABILITY_ID,
            serde_json::json!({
                "strategy": "auto",
                "proactive": true,
                "budget_percent": 0.85,
            }),
        ),
        AgentCapabilityConfig::new(CONTEXT_COST_CONTROL_CAPABILITY_ID),
        AgentCapabilityConfig::new(STATELESS_TODO_LIST_CAPABILITY_ID),
        AgentCapabilityConfig::new(LOOP_DETECTION_CAPABILITY_ID),
        AgentCapabilityConfig::new(PROGRESS_GUARD_CAPABILITY_ID),
        AgentCapabilityConfig::new(PROMPT_CACHING_CAPABILITY_ID),
        // Provider-agnostic deferred tool loading. Core tools stay fully
        // loaded; the long tail is stubbed until the model loads it via the
        // `tool_search` tool. Works on every model. Default threshold is 15
        // tools (see DEFAULT_TOOL_SEARCH_THRESHOLD).
        AgentCapabilityConfig::new(TOOL_SEARCH_CAPABILITY_ID),
        // Records what `tool_search` loaded so reveal-gated prompt blocks
        // (`config`, `memory`) can stay silent until their tools are callable.
        // Pairs with TOOL_SEARCH_CAPABILITY_ID above — gating is meaningless
        // without deferral.
        AgentCapabilityConfig::new(TOOL_REVEAL_CAPABILITY_ID),
        AgentCapabilityConfig::new(TOOL_OUTPUT_PERSISTENCE_CAPABILITY_ID),
        AgentCapabilityConfig::new(SESSION_TASKS_CAPABILITY_ID),
        AgentCapabilityConfig::new(DUCKDUCKGO_CAPABILITY_ID),
        AgentCapabilityConfig::new(ATTRIBUTION_CAPABILITY_ID),
        // enable_file_download=true: saved responses land on disk through
        // the platform filesystem stack, so the write blocklist applies.
        AgentCapabilityConfig::with_config(
            WEB_FETCH_CAPABILITY_ID,
            serde_json::json!({ "enable_file_download": true }),
        ),
        AgentCapabilityConfig::new(SETUP_CAPABILITY_ID),
        AgentCapabilityConfig::new(CONFIG_CAPABILITY_ID),
        AgentCapabilityConfig::new(CONNECTORS_CAPABILITY_ID),
        AgentCapabilityConfig::new(crate::extensions::manage::EXTENSIONS_CAPABILITY_ID),
        AgentCapabilityConfig::new(MEMORY_CAPABILITY_ID),
        AgentCapabilityConfig::new(HOOKS_CAPABILITY_ID),
        AgentCapabilityConfig::new(YOLOP_CAPABILITY_ID),
        // `/btw` — ephemeral side question, answered out-of-band with the
        // session's context (upstream `BtwCapability`).
        AgentCapabilityConfig::new(BTW_CAPABILITY_ID),
        // `/goal` — keep working across turns until a model-evaluated condition holds.
        AgentCapabilityConfig::new(GOAL_CAPABILITY_ID),
        // Soft approval: injects spoken-consent guidance for critical actions,
        // tuned by the central `approval_mode` setting (off contributes nothing).
        AgentCapabilityConfig::new(APPROVAL_CAPABILITY_ID),
        AgentCapabilityConfig::new(CODING_BASH_CAPABILITY_ID),
        AgentCapabilityConfig::new(BACKGROUND_CAPABILITY_ID),
        // Project policy changes more often than tool-use guidance, so keep it
        // late in the prompt prefix for better cache reuse.
        AgentCapabilityConfig::with_config(
            AGENT_INSTRUCTIONS_CAPABILITY_ID,
            serde_json::json!({ "files": ["AGENTS.md"] }),
        ),
        // Per-turn facts are the most volatile prompt contribution.
        AgentCapabilityConfig::new(ENVIRONMENT_CONTEXT_CAPABILITY_ID),
    ]);
    caps
}

pub(crate) fn coding_harness_defaults(client_commands: bool) -> Vec<AgentCapabilityConfig> {
    default_coding_harness_capabilities(client_commands)
}

/// Capability dependency edges: enabling the first id requires the listed ids
/// to also be present on the harness. Declared as data — each new dependency is
/// one row, resolved transitively below — rather than hand-written per-pair
/// checks. IDs are constants so a typo or an upstream rename is a compile error,
/// not a silently dropped dependency.
const HARNESS_CAPABILITY_DEPENDENCIES: &[(&str, &[&str])] =
    &[(DAYTONA_CAPABILITY_ID, &[SESSION_STORAGE_CAPABILITY_ID])];

/// Append any capabilities that enabled ones depend on (e.g. `daytona` pulls in
/// `session_storage`), closing the set against the production
/// [`HARNESS_CAPABILITY_DEPENDENCIES`] table.
fn ensure_harness_capability_dependencies(caps: &mut Vec<AgentCapabilityConfig>) {
    resolve_capability_dependencies(caps, HARNESS_CAPABILITY_DEPENDENCIES);
}

/// Close `caps` under the dependency `edges`: for every present capability,
/// append any missing ids it depends on, repeating until no new id is added so
/// transitive chains are fully resolved. Already-present ids are never
/// duplicated. Bounded by the number of edges — each pass adds at most the
/// missing dependencies once, so the set stabilizes.
fn resolve_capability_dependencies(
    caps: &mut Vec<AgentCapabilityConfig>,
    edges: &[(&str, &[&str])],
) {
    loop {
        let mut added = false;
        for (capability, dependencies) in edges {
            if !caps.iter().any(|c| c.capability_id() == *capability) {
                continue;
            }
            for dependency in *dependencies {
                if !caps.iter().any(|c| c.capability_id() == *dependency) {
                    caps.push(AgentCapabilityConfig::new(*dependency));
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
    }
}

fn push_before_environment_context(
    caps: &mut Vec<AgentCapabilityConfig>,
    cap: AgentCapabilityConfig,
) {
    if let Some(index) = caps
        .iter()
        .position(|c| c.capability_id() == ENVIRONMENT_CONTEXT_CAPABILITY_ID)
    {
        caps.insert(index, cap);
    } else {
        caps.push(cap);
    }
}

fn coding_harness_capabilities(
    client_commands: bool,
    hook_config: Option<serde_json::Value>,
    settings: &Settings,
) -> Vec<AgentCapabilityConfig> {
    let mut caps = apply_capability_settings(
        default_coding_harness_capabilities(client_commands),
        &settings.capabilities,
    );
    ensure_harness_capability_dependencies(&mut caps);
    if let Some(config) = hook_config {
        push_before_environment_context(
            &mut caps,
            AgentCapabilityConfig::with_config(USER_HOOKS_CAPABILITY_ID, config),
        );
    }
    caps
}

// ---------- runtime wiring result ----------

pub struct BuiltRuntime {
    pub handles: RuntimeHandles,
    pub startup: StartupInfo,
    pub model: ModelState,
    pub goal_store: Arc<GoalStore>,
    pub user_ask_store: Arc<UserAskStore>,
    /// Whether `yolop_user_ask` is on the session harness (off by default).
    pub user_ask_enabled: bool,
    pub worktree: Arc<WorktreeManager>,
    /// Settings store shared with the runtime capabilities. The TUI uses it
    /// to resolve credentials when querying provider models APIs and to show
    /// per-provider connection status in the setup overlay.
    pub settings: Arc<SettingsStore>,
    /// CLI-only sandbox override for this process, if present. Hosts use this
    /// to display the runtime's effective mode without persisting it.
    pub sandbox_mode_override: Option<crate::config::SandboxMode>,
    /// Receiver for terminal-side commands emitted by
    /// [`ClientCommandsCapability`]. The TUI drains it in its event loop;
    /// other hosts ignore it. Empty/never-written when
    /// [`BuildOptions::client_commands`] is `false`.
    pub ui_rx: mpsc::UnboundedReceiver<UiRequest>,
    /// Receiver for extension `ui/ask` requests; the TUI prompts the user and
    /// answers each via its oneshot. Empty/never-written outside the TUI.
    pub ask_rx: mpsc::UnboundedReceiver<crate::tui::host_ui::AskRequest>,
    /// Receiver for hard shell escalation approvals. Only the TUI services it.
    pub sandbox_approval_rx: crate::sandbox_approval::ApprovalReceiver,
    /// Receiver for everruns background-task completion signals, delivered via
    /// the wake seam (`background_wake`). The host (TUI/ACP) drains it and runs
    /// a streamed turn so the agent reacts to finished `spawn_background` work.
    pub background_wake: crate::runtime::background_wake::WakeReceiver,
    /// Owns the local schedule poller for this host session. Dropping the host
    /// stops new schedule claims; due prompts share `background_wake` with
    /// ordinary background-task completion signals.
    pub schedule_runner: LocalScheduleRunnerHandle,
    /// Shared repointable workspace disk for host-path tools (`bash`, `!shell`).
    pub workspace_host: Arc<WorkspaceHost>,
    /// Shared Everruns session task registry, backed by `everruns-local`. The
    /// TUI reads this to show `spawn_background` tasks through the generic
    /// runtime task model.
    pub task_registry: Arc<dyn SessionTaskRegistry>,
    /// Schedule store paired with the task registry. The TUI uses it to make
    /// monitor cancellation terminal by disarming the durable schedule.
    pub task_schedule_store: Arc<dyn everruns_core::traits::SessionScheduleStore>,
}

#[derive(Clone)]
pub struct RuntimeHandles {
    pub runtime: Arc<InProcessRuntime>,
    pub session_id: SessionId,
    /// Typed handle to the JSONL event emitter. The runtime sees it
    /// through the `EventBus` trait object; we keep a direct reference
    /// so the TUI can subscribe to the live broadcast for streaming.
    pub events: Arc<JsonlEventEmitter>,
    /// Durable conversation/workspace checkpoint controller shared by every
    /// host path that can start a turn or restore a branch.
    pub checkpoints: Arc<CheckpointManager>,
    /// The runtime's session store, retained so the host can mutate the
    /// live session's scoped MCP servers without rebuilding the runtime.
    /// See [`RuntimeHandles::reload_mcp_servers`].
    pub session_store: Arc<dyn RuntimeSessionStore>,
    /// Workspace root the session's MCP servers were loaded from. Reused
    /// verbatim on reload so `.mcp.json` resolution stays identical to
    /// startup.
    pub workspace_root: PathBuf,
    /// Shared connection store backing MCP OAuth tokens. Saving through this
    /// same instance is what lets the runtime's [`StoredMcpAuthProvider`] see a
    /// freshly minted token (the store caches connections in memory per
    /// handle), so `/mcp login` uses it rather than a separate handle.
    pub connections: Arc<ConnectionStore>,
    /// Shared containment provider for host-triggered shell commands.
    pub(crate) sandbox: Arc<dyn SandboxProvider>,
    pub(crate) sandbox_approval_gate: Arc<crate::sandbox_approval::ApprovalGate>,
    pub(crate) approval_policy: crate::config::ApprovalPolicy,
    /// Best-effort local lifecycle bridge when this process runs in Herdr.
    pub(crate) herdr: crate::capabilities::herdr::HerdrReporter,
}

impl RuntimeHandles {
    pub(crate) fn report_herdr_state(&self, state: crate::capabilities::herdr::HerdrState) {
        self.herdr.report_background(state);
    }

    pub async fn run_checkpointed_turn(
        &self,
        prompt: &str,
        input: InputMessage,
    ) -> anyhow::Result<everruns_runtime::TurnResult> {
        let checkpoint = self.checkpoints.start_turn(prompt)?;
        let result = self.runtime.run_turn(self.session_id, input).await;
        let success = result.as_ref().is_ok_and(|turn| turn.success);
        checkpoint.finish(success)?;
        self.checkpoints.apply_queued_confirmation().await;
        Ok(result?)
    }

    /// Re-read the merged MCP server config (global settings + workspace
    /// `.mcp.json`) and swap it into the live session, so add / remove /
    /// enable / disable take effect on the next turn without a restart.
    ///
    /// The runtime resolves a session's scoped MCP servers per turn from
    /// `session.mcp_servers` and never negatively caches a failed or empty
    /// discovery, so upserting the session here is enough: newly enabled
    /// servers are discovered cold on the next turn and removed ones simply
    /// drop out of the tool set. Returns the sorted names now active.
    pub async fn reload_mcp_servers(&self) -> anyhow::Result<Vec<String>> {
        let servers = crate::config::mcp::load_mcp_servers(&self.workspace_root);
        let mut session = self
            .session_store
            .get_session(self.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session {} not found", self.session_id))?;
        let mut names: Vec<String> = servers.keys().cloned().collect();
        names.sort();
        session.mcp_servers = servers;
        self.session_store.add_session(session).await?;
        Ok(names)
    }

    /// Discover tool names currently offered by the session's scoped MCP
    /// servers (`mcp_<server>__<tool>`). Used by `/tools` so the listing
    /// matches what the next turn can call after login/reload — not the frozen
    /// startup capability list.
    pub async fn list_mcp_tool_names(&self) -> Vec<String> {
        let Ok(Some(session)) = self.session_store.get_session(self.session_id).await else {
            return Vec::new();
        };
        discover_mcp_tool_names(&self.connections, &session.mcp_servers).await
    }

    /// Activate a registered `ext:<name>` capability on the live session so its
    /// tools, prompt, hooks, commands, and contributed MCP servers appear on
    /// the next turn — the hot-enable seam (EVE-795). The capability lands on
    /// the session overlay (distinct from the harness layer a startup-enabled
    /// extension rides), so it can also be live-deactivated.
    pub async fn activate_capability(
        &self,
        capability_id: &str,
    ) -> anyhow::Result<CapabilityDelta> {
        Ok(self
            .runtime
            .activate_capability(self.session_id, AgentCapabilityConfig::new(capability_id))
            .await?)
    }

    /// Deactivate a capability from the live session overlay. Succeeds only for
    /// one activated *this session*; an extension enabled at startup rides the
    /// harness layer and cannot be removed by a session-scoped op (its disable
    /// takes effect next session via settings).
    pub async fn deactivate_capability(
        &self,
        capability_id: &str,
    ) -> anyhow::Result<CapabilityDelta> {
        Ok(self
            .runtime
            .deactivate_capability(self.session_id, capability_id)
            .await?)
    }
}

pub struct StartupInfo {
    pub workspace_root: PathBuf,
    pub tool_names: Vec<String>,
    /// Slash commands contributed by registered capabilities (via
    /// `Capability::commands()`). Resolved once at startup against this
    /// session's harness/agent chain; this is the single source of truth for
    /// the command palette, `/help`, and completion. For the TUI host it
    /// includes the terminal-side commands (`/help`, `/tools`, `/mcp`,
    /// `/cwd`, `/model`, `/effort`, `/clear`, `/shell`, `/quit`) contributed by
    /// `ClientCommandsCapability`; the TUI also accepts `!shell` as the local
    /// shell alias for `/shell`.
    pub capability_commands: Vec<CommandDescriptor>,
    /// On-disk JSONL log for this session. Populated even for fresh ids
    /// so the startup banner can show where new events are being written.
    pub session_log_path: PathBuf,
    /// On-disk folder containing this session's durable local artifacts.
    pub session_dir: PathBuf,
    /// How many events were replayed from disk into the new session.
    /// Zero for fresh sessions; used by the startup banner.
    pub replayed_events: usize,
    /// True when neither env vars nor saved settings provide a credential
    /// for any real provider, or when the preferred provider cannot
    /// authenticate. The TUI auto-opens its setup wizard in this case;
    /// `--print` mode ignores the flag and surfaces a clear error instead.
    pub setup_recommended: bool,
    /// Names of MCP servers configured for this session from `.mcp.json`
    /// (global + workspace, merged). Source for the `/mcp` command and the
    /// startup banner. Empty when no servers are configured.
    pub mcp_server_names: Vec<String>,
    /// Effective user hooks loaded from global/workspace config.
    pub hook_count: usize,
    pub hook_scope_counts: std::collections::BTreeMap<String, usize>,
    pub disabled_hook_contribution_count: usize,
    pub hook_configured: bool,
}

impl StartupInfo {
    pub fn hook_summary(&self) -> String {
        if !self.hook_configured {
            return "none".to_string();
        }
        let scopes = self
            .hook_scope_counts
            .iter()
            .map(|(scope, count)| format!("{scope}:{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let hooks = if scopes.is_empty() {
            self.hook_count.to_string()
        } else {
            format!("{} ({scopes})", self.hook_count)
        };
        if self.disabled_hook_contribution_count == 0 {
            hooks
        } else {
            format!(
                "{hooks}, {} disabled contribution(s)",
                self.disabled_hook_contribution_count
            )
        }
    }
}

#[derive(Clone)]
pub struct ModelState {
    /// Shared with [`crate::capabilities::SetupCapability`] so a successful `/setup`
    /// invocation through `runtime.execute_command` immediately updates the
    /// banner label.
    provider: Arc<RwLock<ProviderChoice>>,
}

impl ModelState {
    fn new(provider: Arc<RwLock<ProviderChoice>>) -> Self {
        Self { provider }
    }

    pub fn provider_label(&self) -> String {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .label()
    }

    pub fn provider_name(&self) -> String {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .provider_name()
            .to_string()
    }

    pub fn model_id(&self) -> String {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .model_id()
            .to_string()
    }

    pub fn reasoning_effort(&self) -> Option<String> {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .reasoning_effort()
            .map(str::to_string)
    }

    pub(crate) fn reasoning_effort_options(&self) -> Vec<ReasoningEffortOption> {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .reasoning_effort_options()
    }

    pub(crate) fn default_reasoning_effort(&self) -> Option<String> {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .default_reasoning_effort()
    }

    /// Snapshot of the current provider choice (including any custom base
    /// URL), e.g. for model discovery against the live configuration.
    pub fn provider_choice(&self) -> ProviderChoice {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .clone()
    }

    pub fn input_message(&self, text: impl Into<String>) -> InputMessage {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .input_message(text)
    }

    pub fn input_message_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ContentPart>,
    ) -> InputMessage {
        self.provider
            .read()
            .expect("provider lock poisoned")
            .input_message_with_images(text, images)
    }
}

/// Optional knobs for [`build`]. Lets the streaming integration tests
/// replace the bundled llmsim config (which is sized for offline demos
/// — too short and too fast to ever cross the runtime's 100ms delta
/// batch window) with one that produces real multi-delta streams. All
/// fields default to "no override" so callers that don't care keep the
/// existing behavior.
pub struct BuildOptions {
    pub llmsim_override: Option<LlmSimConfig>,
    pub(crate) provider_model: Option<ProviderChoice>,
    pub session_kind: SessionKind,
    pub initial_prompt: Option<String>,
    /// Per-process sandbox override. CLI flags use this instead of persisting
    /// a settings change, so the next invocation returns to configured mode.
    pub sandbox_mode_override: Option<crate::config::SandboxMode>,
    /// Register [`ClientCommandsCapability`], which contributes the
    /// terminal-side commands (help/tools/mcp/cwd/model/effort/clear/shell/quit)
    /// and drives them through the host UI channel. Only a host that can apply
    /// the effects sets this: the interactive TUI (and the `app` unit tests
    /// that exercise it). ACP and `--print` leave it `false`.
    pub client_commands: bool,
    pub client_ui: ClientUiContext,
    /// MCP servers supplied by the client for this session (ACP `session/new`
    /// `mcpServers`). Merged over the file-based `.mcp.json`/global config, so a
    /// client-configured server wins on a name collision. Empty for hosts that
    /// do not carry per-session MCP config (the TUI, `--print`).
    pub client_mcp_servers: ScopedMcpServers,
    /// Host that can interactively approve tool calls (ACP
    /// `session/request_permission`). When set, the tool-approval gate is
    /// registered and enforces the current approval level; hosts without an
    /// interactive prompt (the TUI, `--print`) leave it `None`.
    pub tool_approver: Option<Arc<dyn crate::capabilities::ToolApprover>>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            llmsim_override: None,
            provider_model: None,
            session_kind: SessionKind::Interactive,
            initial_prompt: None,
            sandbox_mode_override: None,
            client_commands: false,
            client_ui: ClientUiContext::None,
            client_mcp_servers: ScopedMcpServers::new(),
            tool_approver: None,
        }
    }
}

pub async fn build_with_options(
    workspace_root: PathBuf,
    provider: ProviderChoice,
    resume_session_id: Option<SessionId>,
    sessions_dir: PathBuf,
    settings: Arc<SettingsStore>,
    options: BuildOptions,
) -> Result<BuiltRuntime> {
    let provider = options.provider_model.clone().unwrap_or(provider);
    let canonical_root = std::fs::canonicalize(&workspace_root)
        .with_context(|| format!("canonicalize workspace: {}", workspace_root.display()))?;
    // Unit-test binaries are libtest harnesses and cannot service the Linux
    // worker subcommand. Real-binary coverage lives in tests/integration.rs.
    #[cfg(all(test, target_os = "linux"))]
    let sandbox_mode = options
        .sandbox_mode_override
        .unwrap_or(crate::config::SandboxMode::DangerFullAccess);
    #[cfg(not(all(test, target_os = "linux")))]
    let sandbox_mode = options
        .sandbox_mode_override
        .unwrap_or_else(|| settings.snapshot().sandbox_mode());
    let sandbox = crate::exec::sandbox::provider(sandbox_mode);
    let approval_policy = settings.snapshot().approval_policy();
    let (sandbox_approval_gate, sandbox_approval_rx) = if options.client_ui == ClientUiContext::Tui
    {
        crate::sandbox_approval::ApprovalGate::channel()
    } else {
        (
            crate::sandbox_approval::ApprovalGate::deny(),
            crate::sandbox_approval::denied_receiver(),
        )
    };

    // Pin the SessionId so resume can re-attach to the same session folder
    // (directory name is the session id).
    let session_id = resume_session_id.unwrap_or_default();
    let session_dir = session_dir_path(&sessions_dir, session_id);
    let log_path = session_log_path(&session_dir);
    let _legacy_log = migrate_legacy_session_log(&sessions_dir, &session_dir, session_id)?;

    let saved_metadata = read_session_workspace_metadata(&session_dir)?;
    let repo_root = detect_repo_root(&canonical_root);
    let restored_worktree = saved_metadata
        .as_ref()
        .and_then(restore_worktree_from_metadata);
    let initial_active = restored_worktree
        .as_ref()
        .map(|w| w.path.clone())
        .or_else(|| saved_metadata.as_ref().map(|m| m.active_root.clone()))
        .unwrap_or_else(|| canonical_root.clone());
    let active_root = std::fs::canonicalize(&initial_active).unwrap_or(initial_active);

    let worktrees_mode = settings.snapshot().worktrees_mode();
    let worktree = Arc::new(WorktreeManager::new(
        worktrees_mode,
        repo_root.clone(),
        active_root.clone(),
        session_id,
        session_dir.clone(),
        restored_worktree,
    ));
    if let Some(metadata) = saved_metadata.as_ref() {
        let _ = worktree.restore_from_metadata(metadata);
    } else {
        let mut metadata = SessionWorkspaceMetadata::new(worktree.active_root(), repo_root.clone());
        metadata.session_kind = options.session_kind;
        if let Some(prompt) = options.initial_prompt.as_deref() {
            metadata.apply_initial_prompt(prompt);
        }
        metadata.worktree =
            worktree
                .worktree_info()
                .map(|info| crate::runtime::session_log::WorktreeMetadata {
                    path: info.path,
                    branch: info.branch,
                    base_ref: info.base_ref,
                    slug: info.slug,
                });
        write_session_workspace(&session_dir, &metadata)?;
    }
    if worktrees_mode == crate::config::WorktreesMode::Always {
        let _ = worktree.ensure_always();
    }

    let shared_workspace_root = worktree.shared_active_root();
    let workspace_host = Arc::new(WorkspaceHost::new(
        shared_workspace_root.clone(),
        worktree.active_root(),
    )?);
    let workspace = Workspace::new(workspace_host.clone());
    let effective_root = worktree.active_root();

    let herdr = crate::capabilities::herdr::HerdrReporter::from_env(session_id.to_string());

    // Resolve the disk-backed workspace/global/system skill directories once
    // (this also
    // materializes the embedded system skills). Shared by the skills capability
    // config and the file-store factory's scope routing.
    let skill_dirs = crate::capabilities::skills::SkillDirs::resolve(&effective_root);

    // Read-only skills contributed by enabled extensions (D4: only a declaring,
    // enabled extension's `skills/` loads). Computed here so it feeds both the
    // skills capability config and the file-store mounts below.
    let extension_skill_scopes = crate::extensions::extensions_dir()
        .map(|ext_dir| {
            let snapshot = settings.snapshot();
            let packages = crate::extensions::discover_extensions(&ext_dir);
            crate::extensions::extension_skill_scopes(&packages, |name| {
                snapshot
                    .capability_overrides_for(&crate::extensions::extension_capability_id(name))
                    .iter()
                    .any(|(_, entry)| !entry.is_remove())
            })
        })
        .unwrap_or_default();

    // MCP servers from global settings and workspace `.mcp.json`, merged. Loading is
    // best-effort per scope: a malformed file is warned about and skipped, so
    // it never sinks the session or masks the other scope.
    let mut mcp_servers: ScopedMcpServers = crate::config::mcp::load_mcp_servers(&canonical_root);
    // Client-supplied servers (ACP `session/new` `mcpServers`) overlay the
    // file-based config: a name present in both resolves to the client's entry,
    // matching the "the editor configured this for the agent" expectation.
    if !options.client_mcp_servers.is_empty() {
        mcp_servers = everruns_core::mcp_server::merge_scoped_mcp_servers(
            &mcp_servers,
            &options.client_mcp_servers,
        );
    }
    let hooks_store = Arc::new(crate::config::hooks::HooksStore::beside_settings(
        &settings,
        canonical_root.clone(),
    ));
    let effective_hooks = hooks_store.effective();
    let hook_count = effective_hooks.hooks.len();
    let hook_scope_counts = effective_hooks.scope_counts();
    let disabled_hook_contribution_count = effective_hooks.disabled_contributions.len();
    let hook_configured = !effective_hooks.is_empty();
    let hook_capability_config = hook_configured.then(|| effective_hooks.capability_config());
    let connections_path =
        default_connections_path().unwrap_or_else(|| PathBuf::from("connections.toml"));
    let connections = Arc::new(ConnectionStore::open(connections_path));
    let connection_catalog = Arc::new(ConnectionCatalog::with_defaults());
    let connection_resolver = Arc::new(YolopConnectionResolver::new(connections.clone()));

    // Replay anything already on disk for this id. Missing file → empty.
    // Pass `session_id` so events for any other session get skipped
    // rather than seeded — defends against mixed/copied logs.
    let replayed = replay(&log_path, session_id)?;
    let next_sequence = replayed.max_sequence.map(|m| m + 1).unwrap_or(1);

    // JsonlEventEmitter is the EventBus: emits to memory + appends
    // replay-relevant lines to the per-session JSONL file. `next_sequence`
    // carries the sequence counter across resumes so `Event.sequence`
    // stays monotonic within a session.
    let event_bus_typed = Arc::new(JsonlEventEmitter::open(&log_path, next_sequence)?);
    let message_store = Arc::new(InMemoryMessageRetriever::new());
    let checkpoints = Arc::new(CheckpointManager::open(
        session_id,
        session_dir.clone(),
        log_path.clone(),
        worktree.clone(),
        event_bus_typed.clone(),
        message_store.clone(),
        replayed.max_sequence.unwrap_or(0),
    )?);
    let compaction_checkpoints =
        Arc::new(compaction_checkpoint::JsonlCompactionCheckpointStore::open(
            &session_dir,
            session_id,
            checkpoints.clone(),
        )?);
    let active_events = checkpoints.filter_active_events(replayed.events);
    let replayed_events_count = active_events.len();
    let active_messages = crate::runtime::session_log::messages_from_events(&active_events);
    if let Some(title) = latest_session_title(&active_events) {
        update_session_workspace_title(&session_dir, &title)?;
    }

    // Only the selected timeline branch enters the live stores. Abandoned
    // suffixes remain in events.jsonl for redo and local history inspection.
    event_bus_typed.seed_replayed(active_events).await;
    if !active_messages.is_empty() {
        message_store.seed(session_id, active_messages).await;
    }
    let event_bus: Arc<dyn everruns_runtime::EventBus> = event_bus_typed.clone();

    // Start from the in-memory backend bundle, preserving Yolop's durable
    // event bus and replay-seeded message store, then let everruns-local attach
    // the SQLite-backed task registry and schedule store.
    let base_backends = RuntimeBackends::in_memory()
        .with_event_bus(event_bus)
        .with_message_store(message_store)
        .with_compaction_checkpoint_store(compaction_checkpoints)
        .with_connection_resolver(connection_resolver);
    let local_profile = LocalProfile::new(sessions_dir.join("everruns-local"))
        .with_workspace_root(effective_root.clone());
    let local_backends = LocalBackends::new(local_profile, base_backends)
        .context("initialize everruns-local backend stores")?;
    let task_registry: Arc<dyn SessionTaskRegistry> = local_backends.task_registry.clone();
    let task_schedule_store: Arc<dyn everruns_core::traits::SessionScheduleStore> = Arc::new(
        local_backends
            .schedule_store()
            .context("open local schedule store for task controls")?,
    );
    checkpoints.attach_task_registry(task_registry.clone());
    // Install the wake seam: a `LocalPlatformStore` whose `send_message`
    // enqueues everruns background-task completion signals onto `background_wake`
    // for the host to run as a streamed turn (see `background_wake` and
    // knowledge/specs/background.md). Without a platform store, `spawn_background`'s
    // completion signal is a silent no-op and finished background work never
    // reaches the agent.
    let (background_wake_tx, background_wake_rx) = mpsc::unbounded_channel::<String>();
    let wake_runner = Arc::new(crate::runtime::background_wake::WakeRunner::new(
        session_id,
        background_wake_tx,
    ));
    let schedule_runner = local_backends
        .start_schedule_runner(wake_runner.clone())
        .context("start everruns-local schedule runner")?;
    let local_backends = local_backends.with_platform_runner(wake_runner);
    let backends = local_backends.runtime_backends;
    // Shared between `ModelState` (for banner labels) and
    // `SetupCapability` (which mutates it on a successful `/setup`).
    let provider_state = Arc::new(RwLock::new(provider.clone()));
    let provider_store = backends.provider_store.clone();

    // Register a curated set of built-in capabilities (no opinionated bundle
    // — we want a tight, predictable surface for the coding-CLI) plus our
    // bash capability.
    //
    // Filesystem-anchored (all read via the platform filesystem factory, so
    // they target the real workspace transparently):
    //   * agent_instructions   — re-reads AGENTS.md every turn
    //   * session_file_system  — read/write/edit/list/grep/delete/stat tools
    //
    // Skills (upstream `ScopedSkillsCapability`, wired in `crate::capabilities::skills`):
    //   * skills               — discovers SKILL.md across workspace / global /
    //                            environment / system scopes via the session file store;
    //                            list_skills + activate_skill + read/write_skill
    //
    // Non-filesystem, but useful for a coding agent:
    //   * repo_map            - on-demand multi-language symbol map for broad codebase orientation
    //   * ast_grep            - read-only structural code search
    //   * ast_edit            - previewed ast-grep rewrites (opt-in)
    //   * infinity_context     — bounds the live context and adds query_history
    //   * compaction           — durable native replacement with safe fallbacks
    //   * stateless_todo_list  — write_todos tool for multi-step tasks
    //   * loop_detection       — safety net against repeated identical tool calls
    //   * prompt_caching       — Anthropic prompt caching; free token savings
    //   * duckduckgo           — DuckDuckGo Instant Answer lookup (`duckduckgo_instant_answer`); no API key
    //   * free_search          — best-effort free SERP/dev search (`free_web_search`); no API key
    //   * session_storage      — session kv/secret store (Daytona dependency)
    //   * daytona              — remote cloud sandboxes (`daytona_*` tools)
    //   * connectors           — connect/disconnect sandbox backends
    //   * user_hooks           — executes user-authored hook specs loaded from
    //                            global/workspace hook config
    let mut capabilities = CapabilityRegistry::new();
    // Shared across every reveal-gated capability: `tool_reveal` writes it from
    // `tool_search` results, `config` and `memory` read it when deciding whether
    // their how-to prose has earned its place this turn.
    let tool_reveals = Arc::new(RevealedTools::new());
    let environment_context = EnvironmentContextRegistry::default();
    environment_context.set("sandbox_mode", sandbox_mode.as_str());
    capabilities.register(ToolRevealCapability::new(tool_reveals.clone()));
    capabilities.register(SessionCapability);
    capabilities.register(AgentInstructionsCapability);
    // TODO(EVE-620): revert to `capabilities.register(FileSystemCapability)` once
    // everruns ships an edits[]-only edit_file. This wrapper drops the ambiguous
    // top-level old_text/new_text from edit_file's schema (gpt-5.5 populates both
    // and eats a rejection). https://linear.app/everruns/issue/EVE-620
    capabilities
        .register(crate::capabilities::edit_file_override::EditsOnlyFileSystemCapability::new());
    // Upstream multi-scope skills capability (everruns-core 0.12.0+),
    // configured with yolop's workspace/global/environment/system scopes and a host-path
    // resolver so `${SKILL_DIR}` reaches real files. The file store maps the
    // scope VFS roots onto disk (see `capabilities::skills`).
    capabilities.register(ScopedSkillsCapability::new(
        crate::capabilities::skills::skills_config(
            &skill_dirs,
            herdr.is_active(),
            &extension_skill_scopes,
        ),
    ));
    // Herdr contributes a conditional, read-only skill mount. Outside a Herdr
    // pane it has no mounts and the reporter is inert.
    capabilities.register(HerdrCapability::new(herdr.is_active()));
    // yolop-owned skill uninstall (`delete_skill`); the upstream capability has
    // no removal. Shares the same resolved scope directories.
    capabilities.register(crate::capabilities::skills::SkillManagementCapability::new(
        skill_dirs.clone(),
    ));
    capabilities.register(RepoMapCapability::new(workspace_host.clone()));
    capabilities.register(SessionHistoryCapability::new(
        sessions_dir.clone(),
        session_id,
    ));
    capabilities.register(AstGrepCapability::new(workspace_host.clone()));
    // `ast_edit` — structural rewrites with preview-first `dry_run`. Registered
    // for the catalog but intentionally NOT part of the default harness; enable
    // with `[[capabilities]] ref = "ast_edit"` in settings.toml.
    capabilities.register(AstEditCapability::new(workspace_host.clone()));
    // `lsp` — real language servers (diagnostics, go-to-def, references,
    // rename, symbols, code actions). Registered so it appears in the catalog
    // and can be switched on, but intentionally NOT part of the default
    // harness: it spawns external server processes, so it is opt-in via
    // `[[capabilities]] ref = "lsp"` in settings.toml. See knowledge/specs/lsp.md.
    capabilities.register(LspCapability::new(workspace_host.clone()));
    // Installed extension packages (`<config_dir>/yolop/extensions/<name>/`,
    // `YOLOP_EXTENSIONS_DIR` override) — each becomes an `ext:<name>`
    // capability proxying a YEP capability server. Registered for the catalog
    // but never on the default harness: enable with
    // `[[capabilities]] ref = "ext:<name>"` in settings.toml, exactly like
    // `lsp`. See knowledge/specs/extensions.md.
    // Terminal-side command channel. Created here (before extensions register)
    // so extension `status/changed` pushes and `ClientCommandsCapability` share
    // one `UiRequest` stream that the `App` event loop drains.
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiRequest>();
    // Status-bar sink for extensions, only when the host has a status bar (the
    // TUI). Maps a server's `status/changed` into a `SetExtensionStatus`
    // command; `None` in `--print`/ACP, where it logs instead.
    let status_sink: Option<crate::extensions::StatusSink> =
        matches!(options.client_ui, ClientUiContext::Tui).then(|| {
            let tx = ui_tx.clone();
            Arc::new(
                move |ext: &str, params: crate::extensions::protocol::StatusChangedParams| {
                    let _ = tx.send(UiRequest::fire(UiCommand::SetExtensionStatus {
                        ext: ext.to_string(),
                        status: params.status,
                    }));
                },
            ) as crate::extensions::StatusSink
        });
    // `ui/ask` handler channel: an extension's request rides `ask_tx` to the
    // App, which prompts the user and answers via a per-request oneshot. Only
    // wired for the TUI; `None` elsewhere refuses `ui/ask`.
    let (ask_tx, ask_rx) = mpsc::unbounded_channel::<crate::tui::host_ui::AskRequest>();
    let ask_sink: Option<crate::extensions::AskSink> =
        matches!(options.client_ui, ClientUiContext::Tui).then(|| {
            let ask_tx = ask_tx.clone();
            Arc::new(move |params: crate::extensions::protocol::UiAskParams| {
                let ask_tx = ask_tx.clone();
                Box::pin(async move {
                    let (reply, answer) = tokio::sync::oneshot::channel();
                    let _ = ask_tx.send(crate::tui::host_ui::AskRequest {
                        prompt: params.prompt,
                        placeholder: params.placeholder,
                        secret: params.secret,
                        options: params.options,
                        reply,
                    });
                    match answer.await {
                        Ok(a) => crate::extensions::protocol::UiAskResult {
                            answer: a.answer,
                            cancelled: a.cancelled,
                        },
                        Err(_) => crate::extensions::protocol::UiAskResult {
                            answer: String::new(),
                            cancelled: true,
                        },
                    }
                })
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = crate::extensions::protocol::UiAskResult,
                                > + Send,
                        >,
                    >
            }) as crate::extensions::AskSink
        });

    // Shared handle to enabled extensions' live server processes, so
    // `reload_extension` can restart one in place mid-session (self-writing
    // iteration) without a yolop restart.
    let live_processes = crate::extensions::LiveProcessRegistry::default();
    // Per-extension secrets ride the shared credential store (`connections.toml`,
    // 0600), keyed `ext:<name>` — never `settings.toml`. Injected as env at
    // spawn; never surfaced to the agent.
    let extension_secrets = crate::extensions::ExtensionSecrets::new(connections.clone());
    let mut extension_never_defer: Vec<String> = Vec::new();
    // Trace-facet extensions, captured here and started once `session_id` and
    // the event broadcast exist (below). Observe-only agentic-trace export.
    let mut trace_forwarders: Vec<crate::extensions::trace::TraceForwarder> = Vec::new();
    if let Some(ext_dir) = crate::extensions::extensions_dir() {
        let settings_snapshot = settings.snapshot();
        for package in crate::extensions::discover_extensions(&ext_dir) {
            // An extension's contributed MCP servers (D1) apply only when the
            // extension is enabled in the harness — merged into scoped MCP
            // config so the runtime's own client discovers them, exactly like
            // `.mcp.json`. Workspace `.mcp.json` still overrides by name.
            let overrides = settings_snapshot.capability_overrides_for(
                &crate::extensions::extension_capability_id(&package.manifest.name),
            );
            let enabled = overrides.iter().any(|(_, entry)| !entry.is_remove());
            // The effective harness config: the last non-remove override's
            // inline config (empty when the extension is enabled without one).
            let ext_config = overrides
                .iter()
                .rev()
                .find(|(_, entry)| !entry.is_remove())
                .map(|(_, entry)| entry.config.clone())
                .unwrap_or(serde_json::Value::Null);
            let capability =
                crate::extensions::ExtensionCapability::new(package, effective_root.clone())
                    .with_status_sink(status_sink.clone())
                    .with_ask_sink(ask_sink.clone())
                    .with_process_registry(live_processes.clone())
                    .with_secrets(extension_secrets.clone())
                    .with_environment_context(environment_context.clone());
            if enabled {
                let contributed = capability.contributed_mcp_servers();
                if !contributed.is_empty() {
                    mcp_servers = everruns_core::mcp_server::merge_scoped_mcp_servers(
                        &contributed,
                        &mcp_servers,
                    );
                }
                // Forward the session event stream to an enabled `trace`
                // extension (the process is shared with its other facets via
                // `ext_config`); started below once the session id exists.
                if let Some(fwd) = crate::extensions::trace::TraceForwarder::for_capability(
                    &capability,
                    &ext_config,
                ) {
                    trace_forwarders.push(fwd);
                }
            }
            extension_never_defer.extend(capability.never_defer_tools());
            capabilities.register(capability);
        }
        // The always-on management surface (install/list/enable/remove/reload).
        // Hand it the UI-command sink so enable/disable can activate the
        // capability on the live session (TUI only); `None` elsewhere.
        let manage_ui_tx = matches!(options.client_ui, ClientUiContext::Tui).then(|| ui_tx.clone());
        capabilities.register(
            crate::extensions::ExtensionsCapability::new(
                ext_dir,
                effective_root.clone(),
                settings.clone(),
                live_processes.clone(),
                manage_ui_tx,
            )
            .with_secrets(extension_secrets.clone())
            // The `set_extension_secret` prompt reuses the extension `ui/ask`
            // surface (TUI only); `None` elsewhere refuses interactive setup.
            .with_ask_sink(ask_sink.clone()),
        );
    }
    // Server name list for `/mcp` and StartupInfo, computed after extension
    // contributions are merged so provider-provenance entries show up too.
    let mut mcp_server_names: Vec<String> = mcp_servers.keys().cloned().collect();
    mcp_server_names.sort();
    capabilities.register(InfinityContextCapability);
    capabilities.register(CompactionCapability);
    capabilities.register(ContextCostControlCapability);
    capabilities.register(StatelessTodoListCapability);
    capabilities.register(LoopDetectionCapability);
    capabilities.register(PromptCachingCapability::new());
    // Provider-agnostic deferred tool loading (upstream `everruns-core`, 0.11.0+).
    // Defers the long tail behind a `tool_search` tool and restores real schemas
    // progressively (per-session reveal set). The `never_defer` allowlist keeps
    // hot-path file/shell/planning tools fully loaded so the agent never needs a
    // `tool_search` round-trip before its first read/edit/run/todo call — yolop does not
    // own those tool definitions, so it sets the policy by name here. Works on
    // every provider/model, unlike the native `openai_tool_search` (EVE-521).
    // Progressive disclosure + this allowlist landed upstream in EVE-527 (#2130),
    // which retired the previously vendored copy.
    capabilities.register(
        ToolSearchCapability::new().with_never_defer(
            YOLOP_NEVER_DEFER_TOOLS
                .iter()
                .map(|name| name.to_string())
                // Extension tools flagged `never_defer` in their manifest keep
                // real schemas loaded (budgeted per extension) — the LSP eval
                // showed deferred stubs get ~zero adoption.
                .chain(extension_never_defer.iter().cloned()),
        ),
    );
    capabilities.register(ToolOutputPersistenceCapability);
    capabilities.register(
        crate::capabilities::session_tasks_override::TruthfulSessionTasksCapability::new(),
    );
    capabilities.register(crate::capabilities::NarratedBackgroundExecutionCapability::new());
    capabilities.register(SessionStorageCapability);
    capabilities.register(DaytonaCapability);
    capabilities.register(UserHooksCapability);
    capabilities.register(DuckDuckGoCapability);
    capabilities.register(crate::capabilities::FreeSearchCapability::new());
    capabilities.register(WebFetchCapability::from_env());
    capabilities.register(MessageMetadataCapability);
    capabilities.register(CodingCliEnvironmentCapability::new(
        repo_root.clone().unwrap_or_else(|| canonical_root.clone()),
        shared_workspace_root.clone(),
        options.client_ui.clone(),
        environment_context,
    ));
    // Read-only consumer of the shared config service. `SettingsStore`
    // implements `ConfigService`, so the same handle that backs writes also
    // serves reads to capabilities that don't need the concrete store.
    capabilities.register(AttributionCapability {
        config: settings.clone(),
    });
    // `/btw` — ephemeral side question. As of everruns 0.11.0 the upstream
    // `BtwCapability` implements `execute_command` end to end through the
    // runtime's `CommandHost` facilities (turn context + a session-scoped,
    // tool-less completion that persists nothing), so the embedded runtime
    // dispatches it like any other capability command — no bespoke executor
    // needed. yolop owns no `/btw` logic; it only registers and enables it.
    capabilities.register(BtwCapability);
    let goal_store = Arc::new(GoalStore::open(session_dir.clone()));
    goal_store.load_session(session_id)?;
    capabilities.register(GoalCapability {
        store: goal_store.clone(),
    });
    let user_ask_store = Arc::new(UserAskStore::open(session_dir.clone()));
    user_ask_store.load_session(session_id)?;
    capabilities.register(UserAskCapability {
        store: user_ask_store.clone(),
        session_id,
    });
    capabilities.register(WorktreeCapability {
        manager: worktree.clone(),
    });
    capabilities.register(CheckpointCapability {
        manager: checkpoints.clone(),
    });
    // `/setup` (below) is the capability-sourced slash command. It implements
    // `Capability::execute_command` end to end.
    capabilities.register(SetupCapability {
        provider: provider_state.clone(),
        provider_store: provider_store.clone(),
        config: settings.clone(),
        settings: settings.clone(),
    });
    // Schema-described, human-friendly config editing (`get_config` /
    // `set_config`, including `key=capabilities`) plus an always-on pointer
    // into the system prompt. Persists to the same `settings.toml`; provider/
    // model edits take effect next run. Registered after the catalog is built
    // (see below).
    capabilities.register(ConnectorsCapability {
        catalog: connection_catalog,
        store: connections.clone(),
    });
    // `memory` — global, durable, structured user memory. Its MEMORY.md lives
    // beside settings.toml in the yolop config dir, so a tempdir settings path
    // in tests isolates memory automatically. Only titles are disclosed each
    // turn; bodies are recalled on demand. Tuning (disclosed_titles,
    // recall_limit, soft_cap) flows through the generic capability-config
    // system — see its `config_schema()` and the `AgentCapabilityConfig` for
    // MEMORY_CAPABILITY_ID below.
    capabilities.register(GlobalMemoryCapability {
        memory: Arc::new(MemoryStore::beside_settings(&settings)),
        reveals: tool_reveals.clone(),
    });
    // `hooks` — global/workspace hook self-configuration tools. Runtime
    // execution is still upstream `user_hooks`, registered above.
    capabilities.register(HooksCapability { hooks: hooks_store });
    // `yolop` — framing when the user addresses yolop itself, not the project.
    capabilities.register(YolopCapability);
    capabilities.register(YolopMcpCapability {
        store: Arc::new(McpConfigStore::default_for_workspace(&canonical_root)),
    });
    // `progress_guard` — runtime-visible warnings when tool use stops making
    // observable progress.
    capabilities.register(ProgressGuardCapability::new());
    // Soft approval — spoken-consent guidance + audit tool, gated by the
    // central `approval_mode` setting (read live each turn).
    capabilities.register(ApprovalCapability {
        config: settings.clone(),
        settings: settings.clone(),
    });
    // Hard approval gate — the enforcement half of the same `approval_mode`.
    // Only registered when the host can service an interactive prompt (ACP);
    // it blocks risky tools behind that approval instead of trusting the model
    // to pause.
    if let Some(approver) = options.tool_approver.clone() {
        capabilities.register(crate::capabilities::ToolApprovalCapability::new(
            approver,
            settings.clone(),
        ));
    }
    capabilities.register(CodingBashCapability {
        workspace: workspace.clone(),
        sandbox: sandbox.clone(),
        expose_command: !options.client_commands,
        approval_policy,
        approval_gate: sandbox_approval_gate.clone(),
    });
    // `background` — the `/background` command listing this session's everruns
    // tasks. Detached work runs through everruns `spawn_background` (which wraps
    // the background-capable `bash` tool) and completions wake the agent via the
    // platform-store wake seam (`crate::runtime::background_wake`). See knowledge/specs/background.md.
    capabilities.register(BackgroundCapability {
        session_id,
        task_registry: task_registry.clone(),
        session_store: backends.session_store.clone(),
    });
    // Terminal-side commands. Registered only when the host can apply their
    // effects (the TUI). The capability declares help/tools/mcp/cwd/model/
    // effort/clear/shell/quit and forwards each invocation as a `UiCommand` down
    // `ui_tx` (created above, shared with extension status); the `App` event
    // loop drains `ui_rx` and performs the effect.
    if options.client_commands {
        let ui: Arc<dyn HostUi> = Arc::new(TuiHandle::new(ui_tx));
        capabilities.register(ClientCommandsCapability::new(ui));
    }

    let mut catalog = CapabilityCatalog::new();
    for cap in capabilities.list() {
        catalog.register_arc(cap.clone());
    }

    capabilities.register(ConfigCapability {
        settings: settings.clone(),
        catalog: Arc::new(catalog),
        reveals: tool_reveals.clone(),
    });

    let mut driver_registry = DriverRegistry::new();
    everruns_anthropic::register_driver(&mut driver_registry);
    everruns_openai::register_driver(&mut driver_registry);
    // OpenRouter moved to its own crate in everruns 0.13.0; register its
    // first-class DriverId::OpenRouter driver here (was bundled with openai).
    everruns_openrouter::register_driver(&mut driver_registry);
    crate::drivers::codex::register_driver(&mut driver_registry, settings.clone());
    let settings_snapshot = settings.snapshot();
    let mut setup_recommended = SetupCapability::needs_onboarding(&settings_snapshot);
    let default_model = match &provider {
        ProviderChoice::Anthropic { .. }
        | ProviderChoice::OpenAi { .. }
        | ProviderChoice::Codex { .. }
        | ProviderChoice::Google { .. }
        | ProviderChoice::OpenRouter { .. }
        | ProviderChoice::Ollama { .. }
        | ProviderChoice::Custom { .. } => match provider.model_with_provider(&settings_snapshot) {
            Ok(model) => model,
            Err(err) if matches!(options.client_ui, ClientUiContext::Tui) => {
                // Preferred provider is set but credentials are missing
                // (e.g. Codex login cleared after refresh_token_reused).
                // Interactive sessions open `/setup` instead of exiting.
                tracing::warn!(
                    error = %err,
                    provider = provider.provider_name(),
                    "provider credentials missing; opening setup"
                );
                setup_recommended = true;
                provider.model_without_stored_key()
            }
            Err(_) if setup_recommended => provider.model_without_stored_key(),
            Err(err) => {
                return Err(err.context(
                    "provider credentials missing; run `yolop` interactively and complete /setup",
                ));
            }
        },
        ProviderChoice::Sim => ResolvedModel {
            model: "llmsim-yolop".into(),
            provider_type: DriverId::LlmSim,
            provider_metadata: None,
            api_key: Some("fake-key".into()),
            base_url: None,
        },
    };

    let platform = PlatformDefinition::builder()
        .capability_registry(capabilities)
        .driver_registry(driver_registry)
        .connector(everruns_integrations_daytona::connection::DaytonaConnector)
        .session_file_system_factory(Arc::new(CodingCliSessionFileSystemFactory {
            workspace: workspace_host.clone(),
            session_dir: session_dir.clone(),
            session_id,
            skill_global: skill_dirs.global.clone(),
            skill_system: skill_dirs.system.clone(),
            environment_skill: HerdrCapability::skill_content(herdr.is_active()),
            extension_skills: extension_skill_scopes.clone(),
        }))
        .build();

    // Seed harness/agent/session explicitly so Yolop can attach harness
    // metadata that Everruns forwards to LLM calls and observability.
    let session_title = read_session_workspace_metadata(&session_dir)?
        .and_then(|metadata| metadata.title)
        .unwrap_or_else(|| format!("yolop @ {}", effective_root.display()));
    let mut harness_capabilities = coding_harness_capabilities(
        options.client_commands,
        hook_capability_config,
        &settings_snapshot,
    );
    // Resolve the hard approval gate only when a host supplied an approver
    // (ACP): registering it above is not enough — its pre-tool hook is collected
    // only from the *resolved* capability set.
    if options.tool_approver.is_some() {
        harness_capabilities.push(AgentCapabilityConfig::new(
            crate::capabilities::tool_approval::TOOL_APPROVAL_CAPABILITY_ID,
        ));
    }
    let user_ask_enabled = harness_capabilities
        .iter()
        .any(|cap| cap.capability_id() == USER_ASK_CAPABILITY_ID);
    let session_mcp_servers = mcp_servers.clone();

    let mut harness_builder = HarnessBuilder::new("yolop", SYSTEM_PROMPT)
        .metadata_entry("app", "yolop")
        .metadata_entry("yolop_version", env!("CARGO_PKG_VERSION"))
        .metadata_entry(
            "everruns_runtime_version",
            env!("YOLOP_EVERRUNS_RUNTIME_VERSION"),
        )
        .display_name("Coding CLI")
        .description("Embedded terminal coding agent.")
        // Attribute LLM calls routed through OpenRouter so they show up under
        // Yolop on OpenRouter's app dashboards. The driver forwards these as
        // the `HTTP-Referer` and `X-Title` headers (everruns 0.14+).
        .openrouter_attribution("https://github.com/everruns/yolop", "Yolop")
        .tag("example")
        .tag("coding");
    for cap in harness_capabilities {
        harness_builder = harness_builder.capability(cap);
    }
    let harness_id = harness_builder.harness_id();

    let agent_builder = AgentBuilder::new("coding-agent", AGENT_PROMPT)
        .display_name("Coding Agent")
        .description("Reads, edits, and runs commands inside a project workspace.")
        .tag("example")
        .tag("coding");
    let agent_id = agent_builder.agent_id();

    let session_builder = SessionBuilder::new(harness_id)
        .agent(agent_id)
        .id(session_id)
        .title(session_title)
        .mcp_servers(session_mcp_servers)
        .tag("example")
        .tag("coding");

    // Retain the session store so the host can hot-swap the live session's
    // scoped MCP servers (`RuntimeHandles::reload_mcp_servers`) without
    // rebuilding the runtime. Cloned before `backends` is moved into the
    // builder below.
    let session_store = backends.session_store.clone();

    let mut builder = InProcessRuntimeBuilder::new()
        .mcp_auth_provider(Arc::new(StoredMcpAuthProvider::new(connections.clone())))
        .platform_definition(platform)
        .default_model(default_model)
        .backends(backends)
        .harness(harness_builder.build())
        .agent(agent_builder.build())
        .session(session_builder.build());
    // Always register the llmsim driver so `/setup` can switch to offline mode.
    // mid-session, even if the user started with anthropic or openai.
    let llmsim_config = options.llmsim_override.unwrap_or_else(|| {
        LlmSimConfig::fixed(
            "I'm running in offline mode (llmsim — no API key set). \
             Set ANTHROPIC_API_KEY or OPENAI_API_KEY for real responses.",
        )
        .with_model("llmsim-yolop")
    });
    builder = builder.llm_sim(llmsim_config);
    let runtime = builder.build().await?;

    let context = runtime.load_context(session_id).await?;
    let tool_names = context
        .runtime_agent
        .tools
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let capability_commands = runtime.list_commands(session_id).await?;

    herdr.start_monitor(session_id, event_bus_typed.subscribe());

    // Start agentic-trace forwarding for each enabled `trace` extension: one
    // task per extension consuming its own subscription, filtered to this
    // session. Observe-only — a slow exporter never stalls the run.
    for forwarder in trace_forwarders {
        forwarder.start(session_id, event_bus_typed.subscribe());
    }

    Ok(BuiltRuntime {
        handles: RuntimeHandles {
            runtime: Arc::new(runtime),
            session_id,
            events: event_bus_typed,
            checkpoints,
            session_store,
            workspace_root: canonical_root.clone(),
            connections,
            sandbox: sandbox.clone(),
            sandbox_approval_gate,
            approval_policy,
            herdr,
        },
        startup: StartupInfo {
            workspace_root: effective_root,
            tool_names,
            capability_commands,
            session_log_path: log_path,
            session_dir,
            replayed_events: replayed_events_count,
            setup_recommended,
            mcp_server_names,
            hook_count,
            hook_scope_counts,
            disabled_hook_contribution_count,
            hook_configured,
        },
        model: ModelState::new(provider_state),
        settings,
        sandbox_mode_override: options.sandbox_mode_override,
        ui_rx,
        ask_rx,
        sandbox_approval_rx,
        background_wake: background_wake_rx,
        schedule_runner,
        workspace_host,
        task_registry,
        task_schedule_store,
        goal_store,
        user_ask_store,
        user_ask_enabled,
        worktree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::REPO_MAP_CAPABILITY_ID;
    use everruns_core::McpServerAuthMode;
    use everruns_core::command::ExecuteCommandRequest;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn env_token_names_prefers_oauth_provider_then_server_specific_keys() {
        let request = McpAuthRequest {
            server_name: "linear-prod",
            auth_mode: McpServerAuthMode::OAuth,
            oauth_provider_id: Some("linear"),
        };

        assert_eq!(
            env_token_names(&request),
            vec![
                "LINEAR_ACCESS_TOKEN",
                "LINEAR_API_KEY",
                "LINEAR_TOKEN",
                "MCP_LINEAR_PROD_ACCESS_TOKEN",
                "MCP_LINEAR_PROD_API_KEY",
                "MCP_LINEAR_PROD_TOKEN",
            ]
        );
    }

    #[test]
    fn env_key_prefix_normalizes_separators_and_case() {
        assert_eq!(env_key_prefix("Acme Linear/OAuth"), "ACME_LINEAR_OAUTH");
        assert_eq!(env_key_prefix("linear"), "LINEAR");
    }

    #[test]
    fn env_mcp_auth_provider_returns_bearer_credential_from_provider_env() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::set_var("LINEAR_ACCESS_TOKEN", "linear-test-token");
        }
        let request = McpAuthRequest {
            server_name: "linear",
            auth_mode: McpServerAuthMode::OAuth,
            oauth_provider_id: Some("linear"),
        };

        let credential = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(EnvMcpAuthProvider.authorization(&request))
            .expect("auth provider result")
            .expect("credential from env");

        assert_eq!(
            credential.authorization.as_deref(),
            Some("Bearer linear-test-token")
        );
        assert!(credential.headers.is_empty());
        unsafe {
            std::env::remove_var("LINEAR_ACCESS_TOKEN");
        }
    }

    fn oauth_request(server_name: &str) -> McpAuthRequest<'_> {
        McpAuthRequest {
            server_name,
            auth_mode: McpServerAuthMode::OAuth,
            oauth_provider_id: None,
        }
    }

    fn stored_token(
        token_endpoint: String,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> crate::auth::mcp_oauth::McpOAuthTokenSet {
        crate::auth::mcp_oauth::McpOAuthTokenSet {
            access_token: "access-1".to_string(),
            refresh_token: Some("refresh-1".to_string()),
            token_type: "Bearer".to_string(),
            expires_at: Some(expires_at),
            scope: Some("read".to_string()),
            token_endpoint: Some(token_endpoint),
            client_id: Some("dcr-client-1".to_string()),
            client_secret: None,
        }
    }

    #[tokio::test]
    async fn stored_provider_refreshes_an_expired_token_and_persists_it() {
        let server = crate::auth::mcp_oauth_login::test_support::MockOAuthServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        crate::auth::mcp_oauth::save_tokens(
            &store,
            "docs",
            stored_token(
                format!("{}/token", server.base),
                chrono::Utc::now() - chrono::Duration::seconds(1),
            ),
        )
        .expect("seed token");

        let provider = StoredMcpAuthProvider::new(store.clone());
        let credential = provider
            .authorization(&oauth_request("docs"))
            .await
            .expect("provider result")
            .expect("credential");
        assert_eq!(credential.authorization.as_deref(), Some("Bearer access-2"));

        // The refreshed token is written back through the shared store, so the
        // next request skips the refresh round-trip.
        let saved = crate::auth::mcp_oauth::load_tokens(&store, "docs").expect("persisted token");
        assert_eq!(saved.access_token, "access-2");
    }

    #[tokio::test]
    async fn stored_provider_uses_a_fresh_token_without_a_network_call() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        // Token endpoint points at a closed port: any refresh attempt would
        // error, so a successful bearer proves no refresh was made.
        crate::auth::mcp_oauth::save_tokens(
            &store,
            "docs",
            stored_token(
                "http://127.0.0.1:1/token".to_string(),
                chrono::Utc::now() + chrono::Duration::seconds(3600),
            ),
        )
        .expect("seed token");

        let provider = StoredMcpAuthProvider::new(store);
        let credential = provider
            .authorization(&oauth_request("docs"))
            .await
            .expect("provider result")
            .expect("credential");
        assert_eq!(credential.authorization.as_deref(), Some("Bearer access-1"));
    }

    #[tokio::test]
    async fn stored_provider_without_token_or_env_returns_none() {
        // Uses a server name no env credential can match, so it needs no
        // ENV_LOCK (and holding a std mutex across await would be a lint error).
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Arc::new(ConnectionStore::open(tmp.path().join("connections.toml")));
        let provider = StoredMcpAuthProvider::new(store);
        let credential = provider
            .authorization(&oauth_request("yolop-test-unconfigured-server"))
            .await
            .expect("provider result");
        assert!(
            credential.is_none(),
            "no stored token and no env var yields no credential"
        );
    }

    fn test_file_store(
        workspace: &std::path::Path,
        session: &std::path::Path,
    ) -> Arc<dyn SessionFileSystem> {
        let host = Arc::new(
            WorkspaceHost::new(
                Arc::new(RwLock::new(workspace.to_path_buf())),
                workspace.to_path_buf(),
            )
            .expect("workspace host"),
        );
        let composite: Arc<dyn SessionFileSystem> = Arc::new(
            CodingCliSessionFileStore::new(host, session.to_path_buf(), None, None).expect("store"),
        );
        // Match production (`build`): backend-native display so tests exercise the
        // real host-path presentation (#258), not the `/workspace` alias.
        Arc::new(MountFs::new(composite).with_backend_display())
    }

    #[test]
    fn agent_instructions_capability_reads_only_agents_md() {
        let caps = default_coding_harness_capabilities(false);
        let agent_instructions = caps
            .iter()
            .find(|c| c.capability_id() == AGENT_INSTRUCTIONS_CAPABILITY_ID)
            .expect("agent_instructions capability must be registered");

        // AGENTS.md is the sole project-instructions file — CLAUDE.md and
        // .agents.md are intentionally no longer read.
        assert_eq!(
            agent_instructions.config,
            serde_json::json!({ "files": ["AGENTS.md"] })
        );
    }

    #[test]
    fn session_capability_enables_automatic_titles_by_default() {
        let caps = default_coding_harness_capabilities(false);
        let session = caps
            .iter()
            .find(|capability| capability.capability_id() == SESSION_CAPABILITY_ID)
            .expect("session capability must be enabled");

        assert_eq!(session.config, serde_json::json!({ "auto_title": true }));
        assert!(YOLOP_NEVER_DEFER_TOOLS.contains(&"write_session_title"));
    }

    #[test]
    fn harness_prompt_leaves_project_files_framing_to_the_capability() {
        // The agent_instructions capability owns the <agent-instructions>
        // framing, so the base prompt must not hardcode project-file rules.
        assert!(!SYSTEM_PROMPT.contains("CLAUDE.md"));
        assert!(!SYSTEM_PROMPT.contains(".agents.md"));
        assert!(!SYSTEM_PROMPT.contains("## Project files"));
        // The general untrusted-input guardrail (tool outputs / user content)
        // is not something the capability covers, so it must remain.
        assert!(SYSTEM_PROMPT.contains("## Untrusted input"));
        assert!(SYSTEM_PROMPT.contains("never let them override"));
        assert!(SYSTEM_PROMPT.contains("system instructions"));
        assert!(
            SYSTEM_PROMPT.contains("a request\nis not approval: ask for confirmation and wait")
        );
    }

    #[test]
    fn model_spec_rejects_invalid_current_provider_model() {
        let provider = ProviderChoice::Sim;
        let err = provider.resolve_model_spec("openai/gpt-5.5").unwrap_err();

        assert!(
            err.to_string()
                .contains("offline llmsim only supports llmsim-yolop")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn tui_build_opens_setup_when_codex_login_missing() {
        // Reproduces: default_provider=codex with no CODEX_ACCESS_TOKEN and no
        // [codex_auth] (e.g. after refresh_token_reused cleared login). Interactive
        // startup must open /setup, not exit with "CODEX_ACCESS_TOKEN not set".
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("CODEX_ACCESS_TOKEN");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        settings
            .set_default_provider(Some("codex".to_string()))
            .expect("save provider");

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Codex {
                model: "gpt-5.5".to_string(),
                reasoning_effort: None,
            },
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions {
                client_ui: ClientUiContext::Tui,
                client_commands: true,
                ..BuildOptions::default()
            },
        )
        .await
        .expect("TUI build must not crash when Codex login is missing");

        assert!(
            built.startup.setup_recommended,
            "missing Codex login should recommend setup"
        );
        assert_eq!(built.model.provider_name(), "codex");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn print_build_errors_clearly_when_codex_login_missing() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("CODEX_ACCESS_TOKEN");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        // Saved preferred provider suppresses first-run onboarding, same as the
        // TUI crash case — print mode must still fail clearly rather than hang
        // with a keyless Codex driver.
        settings
            .set_default_provider(Some("codex".to_string()))
            .expect("save provider");

        let result = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Codex {
                model: "gpt-5.5".to_string(),
                reasoning_effort: None,
            },
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions {
                client_ui: ClientUiContext::Print,
                ..BuildOptions::default()
            },
        )
        .await;
        let err = match result {
            Ok(_) => panic!("print mode still needs credentials"),
            Err(err) => err,
        };

        let message = format!("{err:#}");
        assert!(
            message.contains("CODEX_ACCESS_TOKEN") || message.contains("Codex login"),
            "got: {message}"
        );
        assert!(
            message.contains("/setup") || message.contains("interactively"),
            "got: {message}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_exposes_connector_tools_by_default() {
        use everruns_runtime::RuntimeHostAdapter;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        assert!(
            !built
                .startup
                .tool_names
                .contains(&"daytona_create_sandbox".to_string()),
            "daytona is opt-in via [[capabilities]]: {:?}",
            built.startup.tool_names
        );
        for connector_tool in ["list_connectors", "connect", "disconnect", "get_connector"] {
            assert!(
                built
                    .startup
                    .tool_names
                    .contains(&connector_tool.to_string()),
                "connector tools: {:?}",
                built.startup.tool_names
            );
        }
        assert!(
            built
                .startup
                .tool_names
                .contains(&"duckduckgo_instant_answer".to_string()),
            "DuckDuckGo Instant Answer tool: {:?}",
            built.startup.tool_names
        );
        assert!(
            !built
                .startup
                .tool_names
                .contains(&"duckduckgo_search".to_string()),
            "retired DuckDuckGo tool name must not remain: {:?}",
            built.startup.tool_names
        );
        assert!(
            built
                .handles
                .runtime
                .connection_resolver()
                .expect("connection resolver")
                .get_connection_token(built.handles.session_id, "daytona")
                .await
                .expect("resolve")
                .is_none(),
            "no credential configured yet"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scripted_title_tool_updates_runtime_event_log_and_metadata() {
        use everruns_core::events::EventData;
        use everruns_core::llmsim_driver::{SimToolCall, SimTurn};

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let options = BuildOptions {
            llmsim_override: Some(LlmSimConfig::scripted(vec![
                SimTurn::ToolCalls(vec![SimToolCall {
                    name: "write_session_title".to_string(),
                    arguments: serde_json::json!({ "title": "Automatic session titles" }),
                    id: None,
                }]),
                SimTurn::Assistant("Title set.".to_string()),
            ])),
            ..BuildOptions::default()
        };
        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            options,
        )
        .await
        .expect("build runtime");
        let session_id = built.handles.session_id;
        let result = built
            .handles
            .run_checkpointed_turn(
                "Implement automatic session titles",
                built
                    .model
                    .input_message("Implement automatic session titles"),
            )
            .await
            .expect("run turn");

        assert!(result.success, "scripted title turn: {result:?}");
        assert_eq!(result.tool_calls_count, 1);
        let session = built
            .handles
            .session_store
            .get_session(session_id)
            .await
            .expect("get runtime session")
            .expect("runtime session present");
        assert_eq!(session.title.as_deref(), Some("Automatic session titles"));
        let events = built
            .handles
            .runtime
            .events()
            .await
            .expect("runtime events");
        assert!(events.iter().any(|event| matches!(
            &event.data,
            EventData::SessionTitleUpdated(data)
                if data.title == "Automatic session titles"
        )));
        let metadata =
            read_session_workspace_metadata(&session_dir_path(sessions.path(), session_id))
                .expect("read workspace metadata")
                .expect("workspace metadata present");
        assert_eq!(metadata.title.as_deref(), Some("Automatic session titles"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_uses_everruns_local_backend_stores() {
        use everruns_runtime::RuntimeHostAdapter;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        assert!(
            sessions.path().join("everruns-local/local.db").exists(),
            "local backend database should be created under the Yolop sessions dir"
        );
        assert!(
            built.handles.runtime.session_task_registry().is_some(),
            "everruns-local should install a session task registry"
        );
        assert!(
            built
                .handles
                .runtime
                .schedule_store(everruns_runtime::in_process_internal_org_id(
                    everruns_core::DEFAULT_ORG_PUBLIC_ID
                ))
                .is_some(),
            "everruns-local should install a schedule store factory"
        );
        for task_tool in [
            "list_tasks",
            "get_task",
            "message_task",
            "cancel_task",
            "wait_task",
        ] {
            assert!(
                built.startup.tool_names.contains(&task_tool.to_string()),
                "session task tools: {:?}",
                built.startup.tool_names
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_attaches_yolop_embedder_metadata() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        let context = built
            .handles
            .runtime
            .load_context(built.handles.session_id)
            .await
            .expect("load context");
        assert_eq!(
            context.embedder_metadata.get("app").map(String::as_str),
            Some("yolop")
        );
        assert_eq!(
            context
                .embedder_metadata
                .get("yolop_version")
                .map(String::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            context
                .embedder_metadata
                .get("everruns_runtime_version")
                .map(String::as_str),
            Some(env!("YOLOP_EVERRUNS_RUNTIME_VERSION"))
        );
        // OpenRouter attribution headers flow through embedder metadata.
        use everruns_core::driver_registry::{
            OPENROUTER_HTTP_REFERER_METADATA_KEY, OPENROUTER_X_TITLE_METADATA_KEY,
        };
        assert_eq!(
            context
                .embedder_metadata
                .get(OPENROUTER_HTTP_REFERER_METADATA_KEY)
                .map(String::as_str),
            Some("https://github.com/everruns/yolop")
        );
        assert_eq!(
            context
                .embedder_metadata
                .get(OPENROUTER_X_TITLE_METADATA_KEY)
                .map(String::as_str),
            Some("Yolop")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_persists_workspace_root_for_session_resume() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        let saved = crate::runtime::session_log::read_session_workspace(&built.startup.session_dir)
            .expect("read workspace metadata")
            .expect("workspace metadata");
        assert_eq!(saved, built.startup.workspace_root);
    }

    #[test]
    fn harness_applies_daytona_from_settings() {
        use crate::config::capability_settings::CapabilityOverride;

        let mut settings = Settings::default();
        settings.capabilities.push(CapabilityOverride {
            capability_ref: DAYTONA_CAPABILITY_ID.to_string(),
            enabled: Some(true),
            append: false,
            config: serde_json::json!({}),
        });
        let ids = coding_harness_capabilities(false, None, &settings);
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == DAYTONA_CAPABILITY_ID)
        );
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == SESSION_STORAGE_CAPABILITY_ID)
        );
    }

    #[test]
    fn coding_harness_enables_connectors_by_default() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == CONNECTORS_CAPABILITY_ID)
        );
        assert!(
            !ids.iter()
                .any(|cap| cap.capability_id() == DAYTONA_CAPABILITY_ID)
        );
    }

    #[test]
    fn dependency_resolver_pulls_transitive_dependencies() {
        // Drive the production resolver with a synthetic edge set: a capability
        // that only declares a direct dependency must still get the
        // dependency's own dependencies (a -> b -> c).
        let edges: &[(&str, &[&str])] = &[("a", &["b"]), ("b", &["c"])];
        let mut caps = vec![AgentCapabilityConfig::new("a")];
        resolve_capability_dependencies(&mut caps, edges);
        let ids: Vec<&str> = caps.iter().map(|c| c.capability_id()).collect();
        assert!(ids.contains(&"b"), "direct dependency missing: {ids:?}");
        assert!(ids.contains(&"c"), "transitive dependency missing: {ids:?}");
    }

    #[test]
    fn dependency_resolver_is_idempotent() {
        // Running the real resolver twice must not duplicate the injected
        // dependency, and an already-satisfied dependency is left untouched.
        let mut caps = vec![
            AgentCapabilityConfig::new(DAYTONA_CAPABILITY_ID),
            AgentCapabilityConfig::new(SESSION_STORAGE_CAPABILITY_ID),
        ];
        ensure_harness_capability_dependencies(&mut caps);
        ensure_harness_capability_dependencies(&mut caps);
        let storage = caps
            .iter()
            .filter(|c| c.capability_id() == SESSION_STORAGE_CAPABILITY_ID)
            .count();
        assert_eq!(storage, 1, "dependency injected more than once");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_wires_mcp_servers_from_dot_mcp_json() {
        // A workspace `.mcp.json` should flow through build() into the session
        // and surface in startup info (the source for `/mcp`). build() does not
        // contact the server, so this stays offline.
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        std::fs::write(
            workspace.path().join(".mcp.json"),
            r#"{ "mcpServers": { "docs": { "type": "http", "url": "https://example.com/mcp" } } }"#,
        )
        .expect("write .mcp.json");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        assert!(
            built.startup.mcp_server_names.contains(&"docs".to_string()),
            "mcp servers: {:?}",
            built.startup.mcp_server_names
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_merges_client_mcp_servers_over_dot_mcp_json() {
        // Client-supplied servers (ACP `session/new` `mcpServers`) join the
        // file-based config, and on a name collision the client's entry wins.
        use everruns_core::mcp_server::{McpServerTransportType, ScopedMcpServer};
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        std::fs::write(
            workspace.path().join(".mcp.json"),
            r#"{ "mcpServers": {
                "docs": { "type": "http", "url": "https://file.example.com/mcp" }
            } }"#,
        )
        .expect("write .mcp.json");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));

        let mut client_mcp_servers = ScopedMcpServers::new();
        // Collides with the file entry by name — the client URL must win.
        client_mcp_servers.insert(
            "docs".to_string(),
            ScopedMcpServer {
                transport_type: McpServerTransportType::Http,
                url: "https://client.example.com/mcp".to_string(),
                ..ScopedMcpServer::default()
            },
        );
        // A client-only server also flows through.
        client_mcp_servers.insert(
            "issues".to_string(),
            ScopedMcpServer {
                transport_type: McpServerTransportType::Http,
                url: "https://client.example.com/issues".to_string(),
                ..ScopedMcpServer::default()
            },
        );

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions {
                client_mcp_servers,
                ..BuildOptions::default()
            },
        )
        .await
        .expect("build runtime");

        assert!(
            built.startup.mcp_server_names.contains(&"docs".to_string())
                && built
                    .startup
                    .mcp_server_names
                    .contains(&"issues".to_string()),
            "mcp servers: {:?}",
            built.startup.mcp_server_names
        );
        let docs = {
            use everruns_core::SessionStore;
            built
                .handles
                .session_store
                .get_session(built.handles.session_id)
                .await
                .expect("get session")
                .expect("session exists")
                .mcp_servers
                .get("docs")
                .cloned()
                .expect("docs server present")
        };
        assert_eq!(
            docs.url, "https://client.example.com/mcp",
            "client entry must override the file entry on name collision"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn btw_answers_a_side_question_without_persisting_it() {
        // End-to-end check that the upstream `/btw` capability is enabled and
        // dispatches through the embedded runtime's `CommandHost`: it must be
        // listed, answer offline via llmsim, and leave history untouched.
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        let commands = built
            .handles
            .runtime
            .list_commands(built.handles.session_id)
            .await
            .expect("commands");
        let btw = commands
            .iter()
            .find(|c| c.name == "btw")
            .expect("/btw surfaced in the command registry");
        assert!(btw.args.iter().any(|a| a.name == "question" && a.required));

        let result = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "btw".to_string(),
                    arguments: Some("what model are you?".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("execute /btw");
        assert!(result.success, "result: {}", result.message);
        // Offline build → the llmsim fixed response answers the side question.
        assert!(
            result.message.contains("offline mode"),
            "unexpected /btw answer: {}",
            result.message
        );

        // Ephemeral: neither the question nor the answer lands in history.
        let messages = built
            .handles
            .runtime
            .messages(built.handles.session_id)
            .await
            .expect("messages");
        assert!(
            messages.is_empty(),
            "history grew by {} message(s)",
            messages.len()
        );

        // A missing question is rejected (not silently answered). The exact
        // wording lives upstream, so assert only that the call fails.
        built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "btw".to_string(),
                    arguments: None,
                    controls: None,
                },
            )
            .await
            .expect_err("missing question is rejected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn goal_command_is_registered_and_sets_active_condition() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        let commands = built
            .handles
            .runtime
            .list_commands(built.handles.session_id)
            .await
            .expect("commands");
        let goal = commands
            .iter()
            .find(|c| c.name == "goal")
            .expect("/goal surfaced in the command registry");

        let result = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "goal".to_string(),
                    arguments: Some("cargo test exits 0".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("execute /goal");
        assert!(result.success, "result: {}", result.message);
        assert!(built.goal_store.is_active(built.handles.session_id));
        assert!(built.goal_store.take_pending_turn(built.handles.session_id));
        assert_eq!(
            built
                .goal_store
                .active_condition(built.handles.session_id)
                .as_deref(),
            Some("cargo test exits 0")
        );
        assert!(
            goal.description.contains("completion"),
            "descriptor: {}",
            goal.description
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_ask_command_is_registered_when_enabled_in_settings() {
        use crate::config::capability_settings::CapabilityOverride;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        settings
            .append_capability_override(CapabilityOverride {
                capability_ref: USER_ASK_CAPABILITY_ID.to_string(),
                enabled: Some(true),
                append: false,
                config: serde_json::json!({}),
            })
            .expect("append user ask override");

        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");
        assert!(built.user_ask_enabled);

        let commands = built
            .handles
            .runtime
            .list_commands(built.handles.session_id)
            .await
            .expect("commands");
        let ask = commands
            .iter()
            .find(|c| c.name == "ask")
            .expect("/ask surfaced when capability is enabled");

        let result = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "ask".to_string(),
                    arguments: Some("upgrade dependencies".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("execute /ask");
        assert!(result.success, "result: {}", result.message);
        assert!(built.user_ask_store.is_active(built.handles.session_id));
        assert_eq!(
            built
                .user_ask_store
                .active_text(built.handles.session_id)
                .as_deref(),
            Some("upgrade dependencies")
        );
        assert!(
            ask.description.contains("tracked"),
            "descriptor: {}",
            ask.description
        );
    }

    #[test]
    fn coding_harness_excludes_user_ask_by_default() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());
        assert!(
            !ids.iter()
                .any(|cap| cap.capability_id() == USER_ASK_CAPABILITY_ID),
            "user ask should be opt-in via settings"
        );
    }

    #[test]
    fn harness_applies_user_ask_from_settings() {
        use crate::config::capability_settings::CapabilityOverride;

        let mut settings = Settings::default();
        settings.capabilities.push(CapabilityOverride {
            capability_ref: USER_ASK_CAPABILITY_ID.to_string(),
            enabled: Some(true),
            append: false,
            config: serde_json::json!({}),
        });
        let ids = coding_harness_capabilities(false, None, &settings);
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == USER_ASK_CAPABILITY_ID)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_is_the_only_provider_configuration_command() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let settings_for_assert = settings.clone();
        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        let commands = built
            .handles
            .runtime
            .list_commands(built.handles.session_id)
            .await
            .expect("commands");
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();

        assert!(names.contains(&"setup"), "commands: {names:?}");
        for removed in ["provider", "token", "model", "onboard"] {
            assert!(
                !names.contains(&removed),
                "/{removed} should not be a visible setup command: {names:?}"
            );
        }

        let status = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("status".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("setup status");
        assert!(status.success);
        assert!(status.message.starts_with("setup:"));
        assert!(
            status.message.contains("attribution=on"),
            "status: {}",
            status.message
        );
        assert!(
            status.message.contains("approval=normal"),
            "status should report the default approval level: {}",
            status.message
        );

        let disable_attribution = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("attribution off".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("disable setup attribution");
        assert!(disable_attribution.success);
        assert!(!settings_for_assert.snapshot().attribution_enabled());

        let enable_attribution = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("attribution on".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("enable setup attribution");
        assert!(enable_attribution.success);
        assert!(settings_for_assert.snapshot().attribution_enabled());

        // `/setup approval <level>` drives the soft-approval level through the
        // same command entry point and persists it.
        let set_approval = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("approval protective".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("set setup approval");
        assert!(set_approval.success);
        assert_eq!(
            settings_for_assert.snapshot().approval_mode(),
            crate::config::ApprovalMode::Protective
        );

        let bad_approval = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("approval whenever".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("reject bad approval level");
        assert!(!bad_approval.success);
        // An invalid level leaves the prior selection untouched.
        assert_eq!(
            settings_for_assert.snapshot().approval_mode(),
            crate::config::ApprovalMode::Protective
        );

        let store_token = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("token openai sk-test".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("store setup token");
        assert!(store_token.success);
        assert!(settings_for_assert.snapshot().has_token("openai"));

        let set_provider = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("provider openai".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("setup openai provider");
        assert!(set_provider.success);

        let model_effort_base = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("model gpt-5.4".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("setup openai model");
        assert!(model_effort_base.success);

        let effort = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("effort high".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("setup effort");
        assert!(effort.success);
        assert_eq!(built.model.provider_label(), "openai/gpt-5.4 high");

        let clear_token = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("token openai clear".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("clear setup token");
        assert!(clear_token.success);
        assert!(!settings_for_assert.snapshot().has_token("openai"));

        let provider = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("provider llmsim".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("setup provider");
        assert!(provider.success);

        let model = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("model llmsim-yolop".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("setup model");
        assert!(model.success);

        let unknown = built
            .handles
            .runtime
            .execute_command(
                built.handles.session_id,
                ExecuteCommandRequest {
                    name: "setup".to_string(),
                    arguments: Some("wat".to_string()),
                    controls: None,
                },
            )
            .await
            .expect("unknown setup action");
        assert!(!unknown.success);
        assert!(unknown.message.contains("model <id>"));
    }

    // The live-config tools (`set_provider` / `set_model` / `set_reasoning_effort`)
    // and `delete_skill` are registered via SetupCapability / SkillManagementCapability
    // in `build_with_options`. Because ToolSearchCapability defers the long tail
    // behind `tool_search`, they are not in the immediate `runtime_agent.tools`
    // snapshot, so their presence is asserted at the capability level
    // (`capabilities::host::tests::setup_capability_exposes_live_config_tools`,
    // `capabilities::skills::tests::skill_management_capability_exposes_delete_skill`)
    // and their behavior by the SetupController / DeleteSkillTool unit tests.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_url_and_custom_model_persist_through_settings() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let settings_for_assert = settings.clone();
        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");
        let run = |arg: &str| {
            let runtime = built.handles.runtime.clone();
            let session_id = built.handles.session_id;
            let arg = arg.to_string();
            async move {
                runtime
                    .execute_command(
                        session_id,
                        ExecuteCommandRequest {
                            name: "setup".to_string(),
                            arguments: Some(arg),
                            controls: None,
                        },
                    )
                    .await
                    .expect("execute setup")
            }
        };

        let bad_provider = run("url ollama http://localhost:1234/v1").await;
        assert!(!bad_provider.success, "{}", bad_provider.message);

        let bad_scheme = run("url custom ftp://example.com").await;
        assert!(!bad_scheme.success, "{}", bad_scheme.message);

        let stored = run("url custom http://localhost:8000/v1").await;
        assert!(stored.success, "{}", stored.message);
        assert_eq!(
            settings_for_assert.snapshot().base_url_for("custom"),
            Some("http://localhost:8000/v1")
        );

        // First-time custom setup has no model yet, so a bare provider
        // switch must fail with a pointer to /setup …
        let no_model = run("provider custom").await;
        assert!(!no_model.success, "{}", no_model.message);
        assert!(
            no_model.message.contains("no model configured"),
            "{}",
            no_model.message
        );

        // … and the wizard's atomic `provider custom <model>` form succeeds.
        let model = run("provider custom qwen3-coder").await;
        assert!(model.success, "{}", model.message);
        assert_eq!(built.model.provider_label(), "custom/qwen3-coder");
        // Model switches persist so the choice survives a restart.
        let snapshot = settings_for_assert.snapshot();
        assert_eq!(snapshot.default_provider.as_deref(), Some("custom"));
        assert_eq!(snapshot.model_for("custom"), Some("qwen3-coder"));

        // With a model saved, the bare switch now works too.
        let bare = run("provider custom").await;
        assert!(bare.success, "{}", bare.message);
        assert_eq!(built.model.provider_label(), "custom/qwen3-coder");

        let cleared = run("url custom clear").await;
        assert!(cleared.success, "{}", cleared.message);
        assert!(
            settings_for_assert
                .snapshot()
                .base_url_for("custom")
                .is_none()
        );
    }

    #[test]
    fn model_spec_treats_slashes_as_current_provider_model_id() {
        let provider = ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
        };
        let next = provider
            .resolve_model_spec("anthropic/claude-sonnet-4-5")
            .unwrap();

        assert_eq!(next.label(), "openai/anthropic/claude-sonnet-4-5");
    }

    #[test]
    fn model_suggestions_include_claude_fable_5() {
        // Fable 5 rejects budget thinking and sampling params; yolop sends
        // neither for Anthropic, so the published driver works as-is.
        assert!(
            ProviderChoice::model_suggestions_for_provider("anthropic").contains(&"claude-fable-5")
        );

        let provider = ProviderChoice::Anthropic {
            model: "claude-sonnet-4-5".to_string(),
            reasoning_effort: None,
        };
        let next = provider.resolve_model_spec("claude-fable-5").unwrap();
        assert_eq!(next.label(), "anthropic/claude-fable-5 high");
    }

    #[test]
    fn model_suggestions_include_gpt_5_6_variants() {
        let suggestions = ProviderChoice::model_suggestions_for_provider("openai");
        assert_eq!(suggestions[0], "gpt-5.6-sol");
        assert_eq!(suggestions[1], "gpt-5.6-terra");
        assert_eq!(suggestions[2], "gpt-5.6-luna");
        let codex = ProviderChoice::model_suggestions_for_provider("codex");
        assert!(codex.contains(&"gpt-5.6-sol"));
        assert!(codex.contains(&"gpt-5.6-terra"));
        assert!(codex.contains(&"gpt-5.6-luna"));
        let openrouter = ProviderChoice::model_suggestions_for_provider("openrouter");
        assert!(openrouter.contains(&"openai/gpt-5.6-sol"));
        assert!(openrouter.contains(&"openai/gpt-5.6-terra"));
        assert!(openrouter.contains(&"openai/gpt-5.6-luna"));
    }

    #[test]
    fn gpt_5_6_models_have_reasoning_effort_controls() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let provider = ProviderChoice::OpenAi {
                model: "gpt-5.5".to_string(),
                reasoning_effort: None,
            };
            let next = provider.resolve_model_spec(model).unwrap();

            let profile = next.model_profile().expect("gpt-5.6 variant profile");
            let efforts = profile.reasoning_effort.expect("reasoning effort config");
            assert_eq!(
                reasoning_effort_value(&efforts.default).as_deref(),
                Some("medium")
            );
            assert!(
                efforts
                    .values
                    .iter()
                    .any(|value| reasoning_effort_value(&value.value).as_deref() == Some("xhigh"))
            );
        }
    }

    #[test]
    fn model_suggestions_include_1m_context_variants() {
        // The `[1m]` ids resolve through the normal Anthropic model-spec path;
        // the driver handles the suffix (bare id on the wire + `context-1m`
        // beta header), so yolop only needs to offer them in the picker.
        let suggestions = ProviderChoice::model_suggestions_for_provider("anthropic");
        assert!(suggestions.contains(&"claude-fable-5[1m]"));
        assert!(suggestions.contains(&"claude-opus-4-8[1m]"));

        let provider = ProviderChoice::Anthropic {
            model: "claude-sonnet-4-5".to_string(),
            reasoning_effort: None,
        };
        let next = provider.resolve_model_spec("claude-fable-5[1m]").unwrap();
        assert_eq!(next.label(), "anthropic/claude-fable-5[1m] high");
    }

    #[test]
    fn model_spec_uses_current_provider_without_prefix() {
        let provider = ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
        };
        let next = provider.resolve_model_spec("gpt-5.4").unwrap();

        assert_eq!(next.label(), "openai/gpt-5.4 none");
    }

    #[test]
    fn model_spec_accepts_llmsim_model_id() {
        let provider = ProviderChoice::Sim;
        let next = provider.resolve_model_spec("llmsim-yolop").unwrap();

        assert_eq!(next.label(), "llmsim/llmsim-yolop");
    }

    #[test]
    fn model_spec_accepts_openrouter_model_id_with_slash() {
        let provider = ProviderChoice::OpenRouter {
            model: "openai/gpt-5.2".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: None,
        };
        let next = provider
            .resolve_model_spec("nvidia/nemotron-3-ultra-550b-a55b:free")
            .unwrap();

        assert_eq!(
            next.label(),
            "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free"
        );
    }

    #[test]
    fn model_spec_accepts_openrouter_reasoning_effort() {
        let provider = ProviderChoice::OpenRouter {
            model: "openai/gpt-5.2".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: None,
        };
        let next = provider
            .resolve_model_spec("nvidia/nemotron-3-super-120b-a12b high")
            .unwrap();

        assert_eq!(
            next.label(),
            "openrouter/nvidia/nemotron-3-super-120b-a12b high"
        );
    }

    #[test]
    fn model_spec_accepts_ollama_model_id() {
        let provider = ProviderChoice::Ollama {
            model: "llama3.2".to_string(),
            base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
            reasoning_effort: None,
        };
        let next = provider.resolve_model_spec("llama3.3").unwrap();

        assert_eq!(next.label(), "ollama/llama3.3");
    }

    #[test]
    fn model_spec_accepts_google_model_id() {
        let provider = ProviderChoice::Google {
            model: "gemini-2.5-flash".to_string(),
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
            reasoning_effort: None,
        };
        let next = provider.resolve_model_spec("gemini-2.5-pro").unwrap();

        assert_eq!(next.label(), "google/gemini-2.5-pro");
        assert_eq!(next.provider_name(), "google");
    }

    #[test]
    fn default_for_provider_name_returns_provider_default_model() {
        let openai = ProviderChoice::default_for_provider_name("openai").unwrap();
        assert!(openai.label().starts_with("openai/gpt-5.6-sol"));

        let codex = ProviderChoice::default_for_provider_name("codex").unwrap();
        assert!(codex.label().starts_with("codex/gpt-5.6-sol"));

        let anthropic = ProviderChoice::default_for_provider_name("anthropic").unwrap();
        assert_eq!(anthropic.label(), "anthropic/claude-opus-4-8 high");

        let google = ProviderChoice::default_for_provider_name("google").unwrap();
        assert_eq!(google.label(), "google/gemini-2.5-flash");

        let sim = ProviderChoice::default_for_provider_name("llmsim").unwrap();
        assert_eq!(sim.label(), "llmsim/llmsim-yolop");
    }

    #[test]
    fn provider_identity_is_single_source_of_truth() {
        // The supported-name list is exactly `Provider::ALL` mapped to names,
        // in order — adding a variant without updating the list (or vice versa)
        // fails here.
        let from_enum: Vec<&str> = Provider::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(SUPPORTED_PROVIDERS, from_enum.as_slice());

        // Names round-trip; parsing is case-insensitive and trims.
        for p in Provider::ALL {
            assert_eq!(Provider::from_name(p.as_str()), Some(p));
        }
        assert_eq!(Provider::from_name("  OpenAI "), Some(Provider::OpenAi));
        assert_eq!(Provider::from_name("nope"), None);

        // Every supported name builds a choice whose identity round-trips, so
        // `ProviderChoice` and `Provider` can't drift apart.
        for name in SUPPORTED_PROVIDERS {
            let choice = ProviderChoice::default_for_provider_name(name)
                .unwrap_or_else(|e| panic!("default_for_provider_name({name}): {e}"));
            assert_eq!(choice.provider().as_str(), *name);
        }

        // Driver mapping matches the previous hand-written table.
        assert_eq!(Provider::Anthropic.driver_id(), Some(DriverId::Anthropic));
        assert_eq!(Provider::OpenAi.driver_id(), Some(DriverId::OpenAI));
        assert_eq!(Provider::Google.driver_id(), Some(DriverId::OpenAI));
        assert_eq!(Provider::OpenRouter.driver_id(), Some(DriverId::OpenRouter));
        assert_eq!(Provider::Codex.driver_id(), None);
        assert_eq!(Provider::Ollama.driver_id(), None);
        assert_eq!(Provider::Custom.driver_id(), None);
        assert_eq!(Provider::Sim.driver_id(), None);
    }

    #[test]
    fn from_env_or_settings_defaults_to_openai_without_credentials() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("CODEX_ACCESS_TOKEN");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENROUTER_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
            std::env::remove_var("OLLAMA_BASE_URL");
            std::env::remove_var("OLLAMA_API_KEY");
            std::env::remove_var("CUSTOM_BASE_URL");
        }

        let provider = ProviderChoice::from_env_or_settings(&Settings::default());

        assert_eq!(provider.provider_name(), "openai");
    }

    #[test]
    fn from_env_or_settings_picks_custom_only_when_a_model_is_known() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENROUTER_API_KEY");
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
            std::env::remove_var("OLLAMA_BASE_URL");
            std::env::remove_var("OLLAMA_API_KEY");
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("EVERRUNS_CLI_MODEL");
        }
        let mut settings = Settings::default();
        settings
            .base_urls
            .insert("custom".to_string(), "http://localhost:8000/v1".to_string());

        // A base URL alone is not enough — with no model known, a
        // non-interactive run would send an empty model id. Fall back.
        let provider = ProviderChoice::from_env_or_settings(&settings);
        assert_eq!(provider.provider_name(), "openai");

        // With a persisted model the custom endpoint is auto-selected (the
        // caller's `resolve_for_settings` fills the model in).
        settings
            .models
            .insert("custom".to_string(), "qwen3-coder".to_string());
        let provider = ProviderChoice::from_env_or_settings(&settings);
        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(
            resolve_for_settings(provider.provider_name(), &settings)
                .expect("resolve")
                .choice
                .label(),
            "custom/qwen3-coder"
        );
    }

    #[test]
    fn model_spec_on_custom_provider_accepts_effort() {
        let provider = ProviderChoice::Custom {
            model: "old-model".to_string(),
            reasoning_effort: None,
        };
        let next = provider.resolve_model_spec("qwen3-coder high").unwrap();

        assert_eq!(next.label(), "custom/qwen3-coder high");
        assert_eq!(next.provider_name(), "custom");
    }

    #[test]
    fn custom_model_with_provider_resolves_saved_base_url_and_placeholder_key() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("CUSTOM_API_KEY");
        }
        let mut settings = Settings::default();
        settings
            .base_urls
            .insert("custom".to_string(), "http://localhost:8000/v1".to_string());

        let provider = ProviderChoice::Custom {
            model: "qwen3-coder".to_string(),
            reasoning_effort: None,
        };
        let mw = provider.model_with_provider(&settings).unwrap();

        assert_eq!(mw.model, "qwen3-coder");
        assert_eq!(mw.base_url.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(mw.api_key.as_deref(), Some(DEFAULT_CUSTOM_API_KEY));
    }

    #[test]
    fn custom_model_with_provider_requires_base_url_but_not_model() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("CUSTOM_BASE_URL");
            std::env::remove_var("CUSTOM_API_KEY");
        }
        let no_url = ProviderChoice::Custom {
            model: "qwen3-coder".to_string(),
            reasoning_effort: None,
        };
        let err = no_url
            .model_with_provider(&Settings::default())
            .unwrap_err();
        assert!(err.to_string().contains("base URL"), "got: {err}");

        // An unset model must still build a config: model discovery queries
        // the endpoint before any model has been chosen.
        let mut settings = Settings::default();
        settings
            .base_urls
            .insert("custom".to_string(), "http://localhost:8000/v1".to_string());
        let no_model = ProviderChoice::Custom {
            model: String::new(),
            reasoning_effort: None,
        };
        let mw = no_model.model_with_provider(&settings).unwrap();
        assert_eq!(mw.base_url.as_deref(), Some("http://localhost:8000/v1"));
    }

    #[test]
    fn resolve_for_settings_overlays_persisted_spec_for_same_provider() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("EVERRUNS_CLI_MODEL");
        }
        let mut settings = Settings::default();
        settings
            .models
            .insert("openai".to_string(), "gpt-5.4 high".to_string());
        // Anthropic profiles own their effort support too, so a persisted
        // model+effort spec is restored when the model profile allows it.
        settings
            .models
            .insert("anthropic".to_string(), "claude-opus-4-5 high".to_string());

        let openai = resolve_for_settings("openai", &settings)
            .expect("resolve")
            .choice;
        assert_eq!(openai.label(), "openai/gpt-5.4 high");

        let anthropic = resolve_for_settings("anthropic", &settings)
            .expect("resolve")
            .choice;
        assert_eq!(anthropic.label(), "anthropic/claude-opus-4-5 high");
    }

    #[test]
    fn resolve_for_settings_falls_back_to_global_default_model() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("EVERRUNS_CLI_MODEL");
        }
        let mut settings = Settings {
            default_model: Some("claude-opus-4-5".to_string()),
            ..Default::default()
        };

        // No per-provider entry, so the global default_model is applied.
        let anthropic = resolve_for_settings("anthropic", &settings)
            .expect("resolve")
            .choice;
        assert_eq!(anthropic.label(), "anthropic/claude-opus-4-5 medium");

        // A per-provider pick still wins over the global default.
        settings
            .models
            .insert("anthropic".to_string(), "claude-haiku-4-5".to_string());
        let anthropic = resolve_for_settings("anthropic", &settings)
            .expect("resolve")
            .choice;
        assert_eq!(anthropic.label(), "anthropic/claude-haiku-4-5 medium");
    }

    #[test]
    fn resolve_for_settings_ignores_cross_provider_default_model() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("EVERRUNS_CLI_MODEL");
        }
        let settings = Settings {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("gpt-5.5".to_string()),
            ..Default::default()
        };

        let resolved = resolve_for_settings("anthropic", &settings).expect("resolve");
        assert_eq!(resolved.choice.label(), "anthropic/claude-opus-4-8 high");
        assert_eq!(resolved.source, ModelResolutionSource::ProviderDefault);
        assert!(
            resolved.notes.iter().any(|n| n.contains("default_model")),
            "expected warning about ignored default_model, got {:?}",
            resolved.notes
        );
    }

    #[test]
    fn resolve_for_settings_uses_per_provider_model() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("EVERRUNS_CLI_MODEL");
        }
        let mut settings = Settings::default();
        settings
            .models
            .insert("openai".to_string(), "gpt-5.4 high".to_string());

        let resolved = resolve_for_settings("openai", &settings).expect("resolve");
        assert_eq!(resolved.choice.label(), "openai/gpt-5.4 high");
        assert_eq!(resolved.source, ModelResolutionSource::PerProviderModel);
    }

    #[test]
    fn model_compatible_with_provider_rejects_obvious_mismatches() {
        assert!(model_compatible_with_provider(
            "claude-opus-4-5",
            "anthropic"
        ));
        assert!(model_compatible_with_provider("gpt-5.5", "openai"));
        assert!(!model_compatible_with_provider("gpt-5.5", "anthropic"));
        assert!(model_compatible_with_provider(
            "anthropic/claude-sonnet-4-5",
            "openrouter"
        ));
    }

    #[test]
    fn next_run_preview_includes_resolution_notes() {
        let resolved = ResolvedProviderChoice {
            choice: ProviderChoice::default_for_provider_name("anthropic").unwrap(),
            source: ModelResolutionSource::ProviderDefault,
            notes: vec!["default_model \"gpt-5.5\" ignored for anthropic".to_string()],
        };
        let preview = resolved.next_run_preview();
        assert!(preview.contains("provider default"));
        assert!(preview.contains("default_model"));
    }

    #[test]
    fn model_spec_strips_provider_prefix_from_label() {
        let openai = ProviderChoice::OpenAi {
            model: "gpt-5.4".to_string(),
            reasoning_effort: Some("high".to_string()),
        };
        assert_eq!(openai.model_spec(), "gpt-5.4 high");

        // OpenRouter model ids contain `/` themselves; only the provider
        // prefix is stripped.
        let openrouter = ProviderChoice::OpenRouter {
            model: "openai/gpt-5.2".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: None,
        };
        assert_eq!(openrouter.model_spec(), "openai/gpt-5.2");
    }

    #[test]
    fn default_for_provider_name_rejects_unknown() {
        let err = ProviderChoice::default_for_provider_name("totally-bogus").unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn google_requires_api_key_to_build_model_with_provider() {
        // Drop both env vars in case the test runner exported one. The
        // shared `crate::testing::test_env::lock()` serializes against every other
        // env-mutating test in this binary; concurrent setenv/unsetenv
        // calls would otherwise race (UB on glibc).
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
        }
        let provider = ProviderChoice::Google {
            model: "gemini-2.5-flash".to_string(),
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
            reasoning_effort: None,
        };
        let err = provider
            .model_with_provider(&Settings::default())
            .unwrap_err();
        assert!(err.to_string().contains("GEMINI_API_KEY"));
    }

    #[test]
    fn openrouter_requires_api_key() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
        }
        let provider = ProviderChoice::OpenRouter {
            model: "openai/gpt-5.2".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: None,
        };

        let err = provider
            .model_with_provider(&Settings::default())
            .unwrap_err();

        assert!(err.to_string().contains("OPENROUTER_API_KEY not set"));
    }

    #[test]
    fn openrouter_uses_first_class_openrouter_driver() {
        // OpenRouter routes through the first-class OpenRouter provider type
        // (everruns 0.10+): the driver replays the full transcript each turn
        // (the /responses endpoint ignores `previous_response_id`) and resolves
        // model profiles under the OpenRouter provider, so OpenAI-only
        // extensions are never sent to the gateway.
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-or-key");
        }
        let provider = ProviderChoice::OpenRouter {
            model: "nvidia/nemotron-3-ultra-550b-a55b".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: None,
        };

        let model = provider.model_with_provider(&Settings::default()).unwrap();
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
        }

        assert_eq!(model.provider_type, DriverId::OpenRouter);
        assert_eq!(model.api_key, Some("test-or-key".to_string()));
        assert_eq!(
            model.base_url,
            Some(DEFAULT_OPENROUTER_BASE_URL.to_string())
        );

        // The keyless fallback path must agree, so /setup and startup don't
        // silently fall back to a different driver.
        assert_eq!(
            provider.model_without_stored_key().provider_type,
            DriverId::OpenRouter
        );
    }

    #[test]
    fn ollama_uses_openai_responses_driver_with_local_base_url() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let provider = ProviderChoice::Ollama {
            model: "llama3.2".to_string(),
            base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
            reasoning_effort: None,
        };

        let model = provider.model_with_provider(&Settings::default()).unwrap();

        assert_eq!(model.provider_type, DriverId::OpenAI);
        assert_eq!(model.api_key, Some(DEFAULT_OLLAMA_API_KEY.to_string()));
        assert_eq!(model.base_url, Some(DEFAULT_OLLAMA_BASE_URL.to_string()));
    }

    #[test]
    fn stored_token_falls_back_when_env_var_missing() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let mut settings = Settings::default();
        settings
            .tokens
            .insert("anthropic".to_string(), "stored-anth-key".to_string());

        let provider = ProviderChoice::Anthropic {
            model: "claude-sonnet-4-5".to_string(),
            reasoning_effort: None,
        };
        let model = provider.model_with_provider(&settings).unwrap();
        assert_eq!(model.api_key, Some("stored-anth-key".to_string()));
    }

    #[test]
    fn model_spec_accepts_openai_reasoning_effort() {
        let provider = ProviderChoice::OpenAi {
            model: "gpt-5.4".to_string(),
            reasoning_effort: Some("medium".to_string()),
        };
        let next = provider.resolve_model_spec("gpt-5.5 high").unwrap();

        assert_eq!(next.label(), "openai/gpt-5.5 high");
    }

    #[test]
    fn codex_model_with_provider_uses_external_driver_metadata() {
        let _guard = crate::testing::test_env::lock();
        unsafe {
            std::env::remove_var("CODEX_ACCESS_TOKEN");
        }
        let settings = Settings {
            codex_auth: Some(crate::config::CodexAuth {
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_at: Some(1_771_000_000_000),
                account_id: Some("acc_123".to_string()),
                email: None,
            }),
            ..Default::default()
        };
        let provider = ProviderChoice::Codex {
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("high".to_string()),
        };

        let model = provider.model_with_provider(&settings).unwrap();

        assert_eq!(
            model.provider_type,
            DriverId::external(crate::drivers::codex::CODEX_DRIVER_ID)
        );
        assert_eq!(model.api_key.as_deref(), Some("access-token"));
        let metadata = model.provider_metadata.expect("metadata");
        assert_eq!(metadata.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(metadata.account_id.as_deref(), Some("acc_123"));
        assert_eq!(
            metadata
                .extra
                .as_ref()
                .and_then(|extra| extra.get("expires_at"))
                .and_then(serde_json::Value::as_i64),
            Some(1_771_000_000_000)
        );
    }

    #[test]
    fn reasoning_effort_can_update_current_openai_model() {
        let provider = ProviderChoice::OpenAi {
            model: "gpt-5.4".to_string(),
            reasoning_effort: Some("medium".to_string()),
        };
        let next = provider.resolve_reasoning_effort("high").unwrap();

        assert_eq!(next.label(), "openai/gpt-5.4 high");
    }

    #[test]
    fn reasoning_effort_can_update_current_openrouter_model() {
        let provider = ProviderChoice::OpenRouter {
            model: "nvidia/nemotron-3-super-120b-a12b".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: Some("medium".to_string()),
        };
        let next = provider.resolve_reasoning_effort("high").unwrap();

        assert_eq!(
            next.label(),
            "openrouter/nvidia/nemotron-3-super-120b-a12b high"
        );
    }

    #[test]
    fn reasoning_effort_options_come_from_model_profile() {
        let provider = ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: None,
        };
        let options = provider.reasoning_effort_options();

        assert!(
            options.iter().any(|option| option.value == "xhigh"),
            "profile-defined xhigh option should be exposed: {options:?}"
        );
        assert_eq!(
            provider.default_reasoning_effort().as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn codex_reasoning_effort_options_come_from_driver_profile() {
        let provider = ProviderChoice::Codex {
            model: "gpt-5.5".to_string(),
            reasoning_effort: None,
        };
        let options = provider.reasoning_effort_options();

        assert!(
            options.iter().any(|option| option.value == "xhigh"),
            "Codex driver profile should expose OpenAI-family effort metadata: {options:?}"
        );
    }

    #[tokio::test]
    async fn yolop_file_store_routes_workspace_files_to_workspace_root() {
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let session_id = SessionId::from_seed(1);

        store
            .write_file(session_id, "/notes.md", "workspace note", "text")
            .await
            .expect("write workspace file");

        assert_eq!(
            std::fs::read_to_string(workspace.path().join("notes.md")).expect("workspace file"),
            "workspace note"
        );
        assert!(!session.path().join("notes.md").exists());
    }

    #[tokio::test]
    async fn yolop_file_store_repoints_workspace_on_worktree_switch() {
        // A worktree switch mutates the shared active-root lock; the workspace
        // store must follow it via `set_host_root` (EVE-660) rather than staying
        // pinned to the original checkout.
        let first = tempfile::tempdir().expect("first workspace");
        let second = tempfile::tempdir().expect("second workspace");
        let session = tempfile::tempdir().expect("session");
        let active_root = Arc::new(RwLock::new(first.path().to_path_buf()));
        let host = Arc::new(
            WorkspaceHost::new(active_root.clone(), first.path().to_path_buf()).expect("host"),
        );
        let store: Arc<dyn SessionFileSystem> = Arc::new(
            MountFs::new(Arc::new(
                CodingCliSessionFileStore::new(host, session.path().to_path_buf(), None, None)
                    .expect("store"),
            ))
            .with_backend_display(),
        );
        let session_id = SessionId::from_seed(7);
        let first_root = std::fs::canonicalize(first.path()).expect("canonical first");
        let second_root = std::fs::canonicalize(second.path()).expect("canonical second");

        store
            .write_file(
                session_id,
                &first_root.join("before.md").display().to_string(),
                "in first",
                "text",
            )
            .await
            .expect("write to first workspace");
        assert_eq!(
            std::fs::read_to_string(first.path().join("before.md")).expect("first file"),
            "in first"
        );
        assert_eq!(store.display_root(), first_root.display().to_string());

        // Simulate the worktree activating: swap the shared active root.
        *active_root.write().expect("lock") = second.path().to_path_buf();

        store
            .write_file(
                session_id,
                &second_root.join("after.md").display().to_string(),
                "in second",
                "text",
            )
            .await
            .expect("write to second workspace");
        assert_eq!(
            std::fs::read_to_string(second.path().join("after.md")).expect("second file"),
            "in second"
        );
        // The new file must land in the switched-to root, not the original.
        assert!(!first.path().join("after.md").exists());
        assert_eq!(store.display_root(), second_root.display().to_string());
    }

    #[tokio::test]
    async fn yolop_file_store_enforces_seeded_readonly_across_operations() {
        // The persistent workspace store keeps `is_readonly` marks from
        // `seed_initial_file`; recreating it per operation (the prior behavior)
        // dropped them, so a seeded read-only file was silently writable.
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let session_id = SessionId::from_seed(11);

        store
            .seed_initial_file(
                session_id,
                &InitialFile {
                    path: "/locked.md".to_string(),
                    content: "do not touch".to_string(),
                    encoding: "text".to_string(),
                    is_readonly: true,
                },
            )
            .await
            .expect("seed readonly workspace file");

        // A later write to the same path — a distinct operation — must be rejected.
        store
            .write_file(session_id, "/locked.md", "tampered", "text")
            .await
            .expect_err("write to seeded read-only file must be rejected");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("locked.md")).expect("locked file"),
            "do not touch"
        );
    }

    #[tokio::test]
    async fn yolop_file_store_routes_outputs_to_session_dir() {
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let session_id = SessionId::from_seed(2);

        store
            .write_file(
                session_id,
                "/outputs/call.stdout",
                "large command output",
                "text",
            )
            .await
            .expect("write output file");

        assert_eq!(
            std::fs::read_to_string(session.path().join("outputs/call.stdout"))
                .expect("session output"),
            "large command output"
        );
        assert!(!workspace.path().join("outputs/call.stdout").exists());

        let displayed_output = store.display_path("/outputs/call.stdout");
        let via_display_path = store
            .read_file(session_id, &displayed_output)
            .await
            .expect("read output")
            .expect("output file");
        assert_eq!(
            via_display_path.content.as_deref(),
            Some("large command output")
        );

        let direct_grep = store
            .grep_files(session_id, "large command", Some("/outputs"))
            .await
            .expect("grep outputs");
        assert_eq!(direct_grep.len(), 1);
        assert_eq!(direct_grep[0].path, "/outputs/call.stdout");

        store
            .write_file(
                session_id,
                "/outputs/context.stdout",
                "before\nError: protocol mismatch\nROOT_CAUSE=duplicate_done_sentinel\nafter\n",
                "text",
            )
            .await
            .expect("write contextual output");
        let contextual = store
            .grep_files_with_options(
                session_id,
                "Error|failed",
                &GrepOptions {
                    path_pattern: Some("/outputs/context.stdout".to_string()),
                    before_context: 1,
                    after_context: 1,
                    ..GrepOptions::default()
                },
            )
            .await
            .expect("grep output with context");
        assert_eq!(contextual.returned_matches, 1);
        assert_eq!(contextual.blocks.len(), 1);
        assert_eq!(contextual.blocks[0].path, "/outputs/context.stdout");
        assert_eq!(contextual.blocks[0].start_line, 1);
        assert_eq!(contextual.blocks[0].end_line, 3);
        assert_eq!(
            contextual.blocks[0].lines[2].line,
            "ROOT_CAUSE=duplicate_done_sentinel"
        );

        store
            .write_file(session_id, "/src/lib.rs", "workspace grep target", "text")
            .await
            .expect("write workspace file");
        let workspace_grep = store
            .grep_files(session_id, "grep target", Some("src"))
            .await
            .expect("grep workspace");
        assert_eq!(workspace_grep.len(), 1);
        assert_eq!(workspace_grep[0].path, "/src/lib.rs");

        let host_filter = store.display_path("/src");
        let host_path_grep = store
            .grep_files(session_id, "grep target", Some(&host_filter))
            .await
            .expect("grep workspace via host display path");
        assert_eq!(host_path_grep.len(), 1);
        assert_eq!(host_path_grep[0].path, "/src/lib.rs");
    }

    #[tokio::test]
    async fn yolop_file_store_routes_background_artifacts_to_session_dir() {
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let session_id = SessionId::from_seed(3);

        store
            .write_file(
                session_id,
                "/.background/run_1/output.log",
                "background output",
                "text",
            )
            .await
            .expect("write background artifact");

        assert_eq!(
            std::fs::read_to_string(session.path().join(".background/run_1/output.log"))
                .expect("session background artifact"),
            "background output"
        );
        assert!(!workspace.path().join(".background").exists());
    }

    #[tokio::test]
    async fn production_file_store_forwards_contextual_grep_through_write_blocklist() {
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let session_id = SessionId::from_seed(12);
        let host = Arc::new(
            WorkspaceHost::new(
                Arc::new(RwLock::new(workspace.path().to_path_buf())),
                workspace.path().to_path_buf(),
            )
            .expect("workspace host"),
        );
        let factory = CodingCliSessionFileSystemFactory {
            workspace: host,
            session_dir: session.path().to_path_buf(),
            session_id,
            skill_global: None,
            skill_system: None,
            environment_skill: None,
            extension_skills: Vec::new(),
        };
        let store = factory
            .create_session_file_system(SessionFileSystemFactoryContext::default())
            .await
            .expect("session file system");

        store
            .write_file(
                session_id,
                "/outputs/context.stdout",
                "before\nError: protocol mismatch\nROOT_CAUSE=duplicate_done_sentinel\nafter\n",
                "text",
            )
            .await
            .expect("write contextual output");
        let contextual = store
            .grep_files_with_options(
                session_id,
                "Error|failed",
                &GrepOptions {
                    path_pattern: Some("/outputs/context.stdout".to_string()),
                    before_context: 1,
                    after_context: 1,
                    ..GrepOptions::default()
                },
            )
            .await
            .expect("grep output with context");

        assert_eq!(contextual.returned_matches, 1);
        assert_eq!(contextual.blocks.len(), 1);
        assert_eq!(contextual.blocks[0].path, "/outputs/context.stdout");
        assert_eq!(contextual.blocks[0].start_line, 1);
        assert_eq!(contextual.blocks[0].end_line, 3);

        for blocked_dir in everruns_runtime::DEFAULT_WRITE_BLOCKLIST {
            let path = format!("/nested/{blocked_dir}/blocked.txt");
            let result = store.write_file(session_id, &path, "blocked", "text").await;
            assert!(
                result.is_err(),
                "compatibility adapter must retain the {blocked_dir} write block"
            );
            assert!(
                !workspace
                    .path()
                    .join(format!("nested/{blocked_dir}/blocked.txt"))
                    .exists(),
                "compatibility adapter must retain the {blocked_dir} write block"
            );
        }
    }

    #[test]
    fn yolop_file_store_displays_real_workspace_and_session_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let workspace_root = std::fs::canonicalize(workspace.path()).expect("canonical workspace");

        assert_eq!(store.display_root(), workspace_root.display().to_string());
        assert_eq!(
            store.display_path("/src/lib.rs"),
            workspace_root.join("src/lib.rs").display().to_string()
        );
        assert_eq!(
            store.display_path("/outputs/call.stdout"),
            "/outputs/call.stdout"
        );
    }

    #[test]
    fn file_tool_narration_uses_real_workspace_path() {
        use everruns_core::tool_narration::{
            ToolNarrationContext, ToolNarrationPhase, narrate_list_directory,
        };

        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let workspace_root = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
        let narration = narrate_list_directory(
            &serde_json::json!({ "path": "/workspace/src" }),
            ToolNarrationPhase::Completed,
            None,
            ToolNarrationContext::new(Some(store.as_ref())),
        );

        assert!(
            narration.contains(&workspace_root.join("src").display().to_string()),
            "narration should use the active host path: {narration}"
        );
        assert!(!narration.contains("/workspace"));
    }

    #[tokio::test]
    async fn yolop_file_store_routes_skill_scope_roots_outside_workspace() {
        use crate::capabilities::skills::{GLOBAL_SKILLS_VFS, SYSTEM_SKILLS_VFS};
        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let global = tempfile::tempdir().expect("global");
        let system = tempfile::tempdir().expect("system");
        let host = Arc::new(
            WorkspaceHost::new(
                Arc::new(RwLock::new(workspace.path().to_path_buf())),
                workspace.path().to_path_buf(),
            )
            .expect("host"),
        );
        let store = CodingCliSessionFileStore::new(
            host,
            session.path().to_path_buf(),
            Some(global.path().to_path_buf()),
            Some(system.path().to_path_buf()),
        )
        .expect("store");
        let session_id = SessionId::from_seed(7);

        // write_skill into the global scope lands in the global dir, not the workspace.
        store
            .write_file(
                session_id,
                &format!("{GLOBAL_SKILLS_VFS}/greeter/SKILL.md"),
                "global skill",
                "text",
            )
            .await
            .expect("write global skill");
        assert_eq!(
            std::fs::read_to_string(global.path().join("greeter/SKILL.md")).expect("global skill"),
            "global skill"
        );
        assert!(!workspace.path().join(".yolop").exists());

        // A skill placed in the system dir is discoverable via the system VFS root.
        std::fs::create_dir_all(system.path().join("joke")).unwrap();
        std::fs::write(system.path().join("joke/SKILL.md"), "system skill").unwrap();
        let listed = store
            .list_directory(session_id, SYSTEM_SKILLS_VFS)
            .await
            .expect("list system skills");
        assert!(listed.iter().any(|e| e.is_directory && e.name == "joke"));
        let read = store
            .read_file(session_id, &format!("{SYSTEM_SKILLS_VFS}/joke/SKILL.md"))
            .await
            .expect("read system skill")
            .expect("system skill exists");
        assert_eq!(read.content.as_deref(), Some("system skill"));
    }

    #[tokio::test]
    async fn skills_capability_discovers_routed_skill_with_host_path() {
        // End-to-end: the upstream ScopedSkillsCapability, configured by yolop and
        // driven against yolop's routed file store, discovers a system skill and
        // reports a real host path (so the agent's host `bash` can read it).
        use crate::capabilities::skills::{SkillDirs, skills_config};
        use everruns_core::ToolContext;
        use everruns_core::capabilities::Capability;
        use everruns_core::tools::ToolExecutionResult;

        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let system = tempfile::tempdir().expect("system");
        std::fs::create_dir_all(system.path().join("joke")).unwrap();
        std::fs::write(
            system.path().join("joke/SKILL.md"),
            "---\nname: joke\ndescription: Tell a joke\n---\nBe funny.",
        )
        .unwrap();

        let dirs = SkillDirs {
            workspace: workspace.path().join(".agents").join("skills"),
            global: None,
            system: Some(system.path().to_path_buf()),
        };
        let host = Arc::new(
            WorkspaceHost::new(
                Arc::new(RwLock::new(workspace.path().to_path_buf())),
                workspace.path().to_path_buf(),
            )
            .expect("host"),
        );
        let store: Arc<dyn SessionFileSystem> = Arc::new(
            CodingCliSessionFileStore::new(
                host,
                session.path().to_path_buf(),
                None,
                Some(system.path().to_path_buf()),
            )
            .expect("store"),
        );
        let cap = ScopedSkillsCapability::new(skills_config(&dirs, false, &[]));
        let tools = cap.tools();
        let list = tools
            .iter()
            .find(|t| t.name() == "list_skills")
            .expect("list_skills tool");
        let ctx = ToolContext::with_file_store(SessionId::from_seed(8), store);

        match list.execute_with_context(serde_json::json!({}), &ctx).await {
            ToolExecutionResult::Success(val) => {
                let skills = val["skills"].as_array().expect("skills array");
                let joke = skills
                    .iter()
                    .find(|s| s["name"] == "joke")
                    .expect("joke discovered");
                assert_eq!(joke["scope"], "system");
                // The reported path is a real host path under the system dir,
                // not a VFS path — that is what `${SKILL_DIR}`/bash needs.
                // Compare as paths so separators are platform-correct.
                let path = joke["path"].as_str().unwrap();
                assert_eq!(
                    std::path::PathBuf::from(path),
                    system.path().join("joke").join("SKILL.md"),
                    "expected the real host path to the skill"
                );
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn herdr_skill_is_visible_from_the_ephemeral_environment_scope() {
        use crate::capabilities::herdr::HerdrCapability;
        use crate::capabilities::skills::ENVIRONMENT_SKILLS_VFS;

        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let session_id = SessionId::from_seed(81);
        let host = Arc::new(
            WorkspaceHost::new(
                Arc::new(RwLock::new(workspace.path().to_path_buf())),
                workspace.path().to_path_buf(),
            )
            .expect("host"),
        );
        let factory = CodingCliSessionFileSystemFactory {
            workspace: host,
            session_dir: session.path().to_path_buf(),
            session_id,
            skill_global: None,
            skill_system: None,
            environment_skill: HerdrCapability::skill_content(true),
            extension_skills: Vec::new(),
        };

        let store = factory
            .create_session_file_system(SessionFileSystemFactoryContext::default())
            .await
            .expect("session file system");
        let entries = store
            .list_directory(session_id, ENVIRONMENT_SKILLS_VFS)
            .await
            .expect("list environment skills");
        assert!(
            entries
                .iter()
                .any(|entry| entry.is_directory && entry.name == "herdr")
        );
        let skill = store
            .read_file(
                session_id,
                &format!("{ENVIRONMENT_SKILLS_VFS}/herdr/SKILL.md"),
            )
            .await
            .expect("read environment skill")
            .expect("Herdr skill exists");
        assert!(skill.content.as_deref().unwrap().contains("name: herdr"));
        assert!(
            store
                .write_file(
                    session_id,
                    &format!("{ENVIRONMENT_SKILLS_VFS}/herdr/SKILL.md"),
                    "changed",
                    "text",
                )
                .await
                .is_err(),
            "environment skill must remain read-only"
        );
    }

    #[tokio::test]
    async fn extension_contributed_skill_is_visible_and_read_only() {
        use crate::capabilities::skills::extension_skills_vfs;

        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let ext = tempfile::tempdir().expect("extension");
        let session_id = SessionId::from_seed(82);

        // An installed extension ships skills/greet/SKILL.md.
        let skills_dir = ext.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("greet")).unwrap();
        std::fs::write(
            skills_dir.join("greet/SKILL.md"),
            "---\nname: greet\ndescription: Say hi\n---\nBe warm.",
        )
        .unwrap();

        let host = Arc::new(
            WorkspaceHost::new(
                Arc::new(RwLock::new(workspace.path().to_path_buf())),
                workspace.path().to_path_buf(),
            )
            .expect("host"),
        );
        let factory = CodingCliSessionFileSystemFactory {
            workspace: host,
            session_dir: session.path().to_path_buf(),
            session_id,
            skill_global: None,
            skill_system: None,
            environment_skill: None,
            extension_skills: vec![("demo".to_string(), skills_dir.clone())],
        };
        let store = factory
            .create_session_file_system(SessionFileSystemFactoryContext::default())
            .await
            .expect("session file system");

        let vfs = extension_skills_vfs("demo");
        let entries = store
            .list_directory(session_id, &vfs)
            .await
            .expect("list extension skills");
        assert!(
            entries.iter().any(|e| e.is_directory && e.name == "greet"),
            "extension skill dir should be listed: {entries:?}"
        );
        let skill = store
            .read_file(session_id, &format!("{vfs}/greet/SKILL.md"))
            .await
            .expect("read")
            .expect("skill exists");
        assert!(skill.content.as_deref().unwrap().contains("name: greet"));
        // Contributed skills are read-only.
        assert!(
            store
                .write_file(session_id, &format!("{vfs}/greet/SKILL.md"), "x", "text")
                .await
                .is_err(),
            "extension skills must be read-only"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn yolop_file_store_secures_output_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let session_id = SessionId::from_seed(3);

        store
            .write_file(
                session_id,
                "/outputs/private.stdout",
                "sensitive output",
                "text",
            )
            .await
            .expect("write output file");

        let output_mode = std::fs::metadata(session.path().join("outputs/private.stdout"))
            .expect("output metadata")
            .permissions()
            .mode()
            & 0o777;
        let output_dir_mode = std::fs::metadata(session.path().join("outputs"))
            .expect("output dir metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(output_mode, 0o600);
        assert_eq!(output_dir_mode, 0o700);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn yolop_file_store_secures_nested_output_directories() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().expect("workspace");
        let session = tempfile::tempdir().expect("session");
        let store = test_file_store(workspace.path(), session.path());
        let session_id = SessionId::from_seed(4);

        store
            .write_file(
                session_id,
                "/outputs/run/log/output.txt",
                "deep artifact",
                "text",
            )
            .await
            .expect("write nested output file");

        let mode_of = |relative: &str| -> u32 {
            std::fs::metadata(session.path().join(relative))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };

        assert_eq!(mode_of("outputs/run/log/output.txt"), 0o600);
        assert_eq!(mode_of("outputs/run/log"), 0o700);
        assert_eq!(mode_of("outputs/run"), 0o700);
        assert_eq!(mode_of("outputs"), 0o700);
    }
    #[test]
    fn openai_input_message_carries_reasoning_effort() {
        let provider = ProviderChoice::OpenAi {
            model: "gpt-5.5".to_string(),
            reasoning_effort: Some("medium".to_string()),
        };

        let input = provider.input_message("hello");

        assert_eq!(
            input
                .controls
                .and_then(|controls| controls.reasoning)
                .and_then(|reasoning| reasoning.effort),
            Some("medium".to_string())
        );
    }

    #[test]
    fn openrouter_input_message_carries_reasoning_effort() {
        let provider = ProviderChoice::OpenRouter {
            model: "nvidia/nemotron-3-super-120b-a12b".to_string(),
            base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            reasoning_effort: Some("high".to_string()),
        };

        let input = provider.input_message("hello");

        assert_eq!(
            input
                .controls
                .and_then(|controls| controls.reasoning)
                .and_then(|reasoning| reasoning.effort),
            Some("high".to_string())
        );
    }

    #[test]
    fn harness_applies_message_metadata_from_settings() {
        use crate::config::capability_settings::CapabilityOverride;
        use everruns_core::capabilities::MESSAGE_METADATA_CAPABILITY_ID;

        let mut settings = Settings::default();
        settings.capabilities.push(CapabilityOverride {
            capability_ref: MESSAGE_METADATA_CAPABILITY_ID.to_string(),
            enabled: Some(true),
            append: false,
            config: serde_json::json!({ "fields": ["timestamp"] }),
        });
        let ids = coding_harness_capabilities(false, None, &settings);
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == MESSAGE_METADATA_CAPABILITY_ID)
        );
    }

    #[test]
    fn coding_harness_enables_tool_output_persistence() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == TOOL_OUTPUT_PERSISTENCE_CAPABILITY_ID)
        );
    }

    #[test]
    fn system_prompt_uses_tool_schemas_as_the_operational_contract() {
        assert!(SYSTEM_PROMPT.contains("descriptions and schemas as the operational contract"));
        assert!(SYSTEM_PROMPT.contains("Load hidden"));
        assert!(SYSTEM_PROMPT.contains("schemas with `tool_search`"));
        assert!(!SYSTEM_PROMPT.contains("## Permanent Tools"));
        assert!(!SYSTEM_PROMPT.contains("## Searchable Tools"));
    }

    #[test]
    fn system_prompt_requires_verification_before_finishing_edits() {
        let workflow = SYSTEM_PROMPT
            .split("## Workflow")
            .nth(1)
            .and_then(|tail| tail.split("## Safety").next())
            .expect("workflow section should be present");
        // Normalize whitespace so line-wrapping in the prompt can't split an
        // asserted phrase across a newline.
        let workflow = workflow.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(workflow.contains("Verify expected behavior with assertions"));
        assert!(workflow.contains("edge cases"));
        assert!(workflow.contains("affected call sites"));
        assert!(workflow.contains("review the diff"));
        assert!(workflow.contains("one decisive validation"));
        assert!(workflow.contains("fix the root cause"));
    }

    #[test]
    fn coding_harness_enables_tool_search() {
        // Deferred tool loading must be wired for every host configuration —
        // it works on every provider, so there is no reason to scope it.
        for client_commands in [false, true] {
            let ids = coding_harness_capabilities(client_commands, None, &Settings::default());
            assert!(
                ids.iter()
                    .any(|cap| cap.capability_id() == TOOL_SEARCH_CAPABILITY_ID),
                "tool_search must be enabled (client_commands={client_commands})"
            );
        }
    }

    #[test]
    fn tool_search_keeps_routing_tool_schemas_loaded() {
        use everruns_core::capabilities::{Capability, DEFAULT_TOOL_SEARCH_THRESHOLD};
        use everruns_core::tool_types::{
            BuiltinTool, DeferrablePolicy, ToolDefinition, ToolHints, ToolPolicy,
        };

        fn fake_tool(name: impl Into<String>) -> ToolDefinition {
            ToolDefinition::Builtin(BuiltinTool {
                name: name.into(),
                display_name: None,
                description: "fake tool".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"]
                }),
                policy: ToolPolicy::Auto,
                category: None,
                deferrable: DeferrablePolicy::Automatic,
                hints: ToolHints::default(),
                full_parameters: None,
            })
        }

        let mut tools = vec![
            ToolDefinition::Builtin(BuiltinTool {
                name: "write_todos".to_string(),
                display_name: None,
                description: "write todos".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "todos": { "type": "array" } },
                    "required": ["todos"]
                }),
                policy: ToolPolicy::Auto,
                category: None,
                deferrable: DeferrablePolicy::Automatic,
                hints: ToolHints::default(),
                full_parameters: None,
            }),
            ToolDefinition::Builtin(BuiltinTool {
                name: "activate_skill".to_string(),
                display_name: None,
                description: "activate skill".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }),
                policy: ToolPolicy::Auto,
                category: None,
                deferrable: DeferrablePolicy::Automatic,
                hints: ToolHints::default(),
                full_parameters: None,
            }),
            fake_tool("write_session_title"),
        ];
        tools
            .extend((0..DEFAULT_TOOL_SEARCH_THRESHOLD).map(|idx| fake_tool(format!("fake_{idx}"))));

        let hook = ToolSearchCapability::new()
            .with_never_defer(YOLOP_NEVER_DEFER_TOOLS.iter().copied())
            .tool_definition_hooks()
            .into_iter()
            .next()
            .expect("tool_search hook");
        let transformed = hook.transform(tools);

        let write_todos = transformed
            .iter()
            .find(|tool| tool.name() == "write_todos")
            .expect("write_todos definition");
        assert!(
            write_todos.parameters().get("properties").is_some(),
            "write_todos must keep its full schema so models pass the required todos field"
        );

        let activate_skill = transformed
            .iter()
            .find(|tool| tool.name() == "activate_skill")
            .expect("activate_skill definition");
        assert!(
            activate_skill.parameters().get("properties").is_some(),
            "activate_skill must keep its full schema so models pass the required name field"
        );

        let write_session_title = transformed
            .iter()
            .find(|tool| tool.name() == "write_session_title")
            .expect("write_session_title definition");
        assert!(
            write_session_title.parameters().get("properties").is_some(),
            "write_session_title must be callable on the first substantive turn"
        );

        let fake = transformed
            .iter()
            .find(|tool| tool.name() == "fake_0")
            .expect("fake tool definition");
        assert!(
            fake.parameters().get("properties").is_none(),
            "precondition: deferral should be active for non-allowlisted tools"
        );
    }

    #[test]
    fn coding_harness_enables_repo_map() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == REPO_MAP_CAPABILITY_ID),
            "repo_map should be available for on-demand codebase orientation"
        );
    }

    #[test]
    fn coding_harness_enables_session_history() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == SESSION_HISTORY_CAPABILITY_ID),
            "search_sessions should be available for grounding prior-session investigations"
        );
    }

    #[test]
    fn coding_harness_enables_ast_grep() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == AST_GREP_CAPABILITY_ID),
            "ast_grep should be available for structural code search"
        );
    }

    /// `ast_edit` is registered for the catalog but stays off the default harness
    /// until explicitly enabled in settings.toml.
    #[test]
    fn coding_harness_does_not_enable_ast_edit_by_default() {
        use crate::capabilities::ast_grep::AST_EDIT_CAPABILITY_ID;

        let ids = coding_harness_capabilities(false, None, &Settings::default());
        assert!(
            !ids.iter()
                .any(|cap| cap.capability_id() == AST_EDIT_CAPABILITY_ID),
            "ast_edit must remain opt-in"
        );

        let mut settings = Settings::default();
        settings
            .capabilities
            .push(crate::config::capability_settings::CapabilityOverride {
                capability_ref: AST_EDIT_CAPABILITY_ID.to_string(),
                enabled: Some(true),
                append: false,
                config: serde_json::Value::Null,
            });
        let ids = coding_harness_capabilities(false, None, &settings);
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == AST_EDIT_CAPABILITY_ID),
            "a [[capabilities]] override should enable ast_edit"
        );
    }

    /// LSP spawns external language-server processes, so it must stay opt-in:
    /// registered (enable-able via `[[capabilities]] ref = "lsp"`) but never
    /// part of the default harness.
    #[test]
    fn coding_harness_does_not_enable_lsp_by_default() {
        use crate::capabilities::lsp::LSP_CAPABILITY_ID;

        let ids = coding_harness_capabilities(false, None, &Settings::default());
        assert!(
            !ids.iter()
                .any(|cap| cap.capability_id() == LSP_CAPABILITY_ID),
            "lsp must remain opt-in; it starts external server processes"
        );

        let mut settings = Settings::default();
        settings
            .capabilities
            .push(crate::config::capability_settings::CapabilityOverride {
                capability_ref: LSP_CAPABILITY_ID.to_string(),
                enabled: Some(true),
                append: false,
                config: serde_json::Value::Null,
            });
        let ids = coding_harness_capabilities(false, None, &settings);
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == LSP_CAPABILITY_ID),
            "a [[capabilities]] override should enable lsp"
        );
    }

    #[test]
    fn coding_harness_enables_hooks_authoring_and_yolop_framing() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == HOOKS_CAPABILITY_ID),
            "hook authoring should be a dedicated capability"
        );
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == YOLOP_CAPABILITY_ID),
            "yolop framing should remain available in the default harness"
        );
    }

    /// Tool search only activates once the tool surface crosses
    /// `DEFAULT_TOOL_SEARCH_THRESHOLD`; below it, full schemas are sent even
    /// with the capability on. This guards the integration: if yolop's tool
    /// count ever drops below the threshold, deferred loading silently stops
    /// helping and this test fails loudly so the threshold can be revisited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_surface_exceeds_tool_search_threshold() {
        use everruns_core::capabilities::DEFAULT_TOOL_SEARCH_THRESHOLD;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let settings = Arc::new(SettingsStore::open(sessions.path().join("settings.toml")));
        let built = build_with_options(
            workspace.path().to_path_buf(),
            ProviderChoice::Sim,
            None,
            sessions.path().to_path_buf(),
            settings,
            BuildOptions::default(),
        )
        .await
        .expect("build runtime");

        let tool_count = built.startup.tool_names.len();
        assert!(
            tool_count > DEFAULT_TOOL_SEARCH_THRESHOLD,
            "tool surface ({tool_count}) must exceed the tool_search threshold \
             ({DEFAULT_TOOL_SEARCH_THRESHOLD}) for deferred loading to activate; \
             if the surface shrinks, lower the threshold via \
             ToolSearchCapability::with_threshold (or DEFAULT_TOOL_SEARCH_THRESHOLD)"
        );
    }

    #[test]
    fn coding_harness_enables_loop_detection() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == LOOP_DETECTION_CAPABILITY_ID)
        );
    }

    #[test]
    fn coding_harness_enables_progress_guard() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == PROGRESS_GUARD_CAPABILITY_ID)
        );
    }

    #[test]
    fn coding_harness_enables_durable_compaction_with_searchable_history() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == INFINITY_CONTEXT_CAPABILITY_ID)
        );
        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == CONTEXT_COST_CONTROL_CAPABILITY_ID)
        );
        let compaction = ids
            .iter()
            .find(|cap| cap.capability_id() == COMPACTION_CAPABILITY_ID)
            .expect("durable compaction must be enabled");
        assert_eq!(compaction.config["strategy"], "auto");
        assert_eq!(compaction.config["proactive"], true);
        assert_eq!(compaction.config["budget_percent"], serde_json::json!(0.85));
    }

    #[test]
    fn coding_harness_enables_yolop_attribution() {
        let ids = coding_harness_capabilities(false, None, &Settings::default());

        assert!(
            ids.iter()
                .any(|cap| cap.capability_id() == ATTRIBUTION_CAPABILITY_ID)
        );
    }

    #[test]
    fn coding_harness_gates_client_commands_on_flag() {
        let without = coding_harness_capabilities(false, None, &Settings::default());
        assert!(
            !without
                .iter()
                .any(|cap| cap.capability_id() == CLIENT_COMMANDS_CAPABILITY_ID),
            "client commands must stay off for hosts that can't apply them"
        );

        let with = coding_harness_capabilities(true, None, &Settings::default());
        assert!(
            with.iter()
                .any(|cap| cap.capability_id() == CLIENT_COMMANDS_CAPABILITY_ID),
            "the TUI host enables the terminal-side commands"
        );
    }

    #[test]
    fn coding_harness_orders_stable_prompt_before_project_context() {
        let ids = coding_harness_capabilities(true, None, &Settings::default());
        let position = |id: &str| {
            ids.iter()
                .position(|cap| cap.capability_id() == id)
                .unwrap_or_else(|| panic!("{id} should be enabled"))
        };

        assert!(
            position(CLIENT_COMMANDS_CAPABILITY_ID) < position(AGENT_INSTRUCTIONS_CAPABILITY_ID)
        );
        assert!(
            position(SESSION_FILE_SYSTEM_CAPABILITY_ID)
                < position(AGENT_INSTRUCTIONS_CAPABILITY_ID)
        );
        assert!(position(SKILLS_CAPABILITY_ID) < position(AGENT_INSTRUCTIONS_CAPABILITY_ID));
        assert!(position(APPROVAL_CAPABILITY_ID) < position(AGENT_INSTRUCTIONS_CAPABILITY_ID));
        assert!(
            position(AGENT_INSTRUCTIONS_CAPABILITY_ID)
                < position(ENVIRONMENT_CONTEXT_CAPABILITY_ID)
        );
        assert_eq!(position(ENVIRONMENT_CONTEXT_CAPABILITY_ID), ids.len() - 1);

        let agent_instructions = ids
            .iter()
            .find(|cap| cap.capability_id() == AGENT_INSTRUCTIONS_CAPABILITY_ID)
            .expect("agent instructions");
        assert_eq!(
            agent_instructions.config["files"],
            serde_json::json!(["AGENTS.md"])
        );
    }

    #[test]
    fn coding_harness_keeps_environment_context_last_with_user_hooks() {
        let ids = coding_harness_capabilities(
            true,
            Some(serde_json::json!({ "hooks": [] })),
            &Settings::default(),
        );
        let position = |id: &str| {
            ids.iter()
                .position(|cap| cap.capability_id() == id)
                .unwrap_or_else(|| panic!("{id} should be enabled"))
        };

        assert!(position(USER_HOOKS_CAPABILITY_ID) < position(ENVIRONMENT_CONTEXT_CAPABILITY_ID));
        assert_eq!(position(ENVIRONMENT_CONTEXT_CAPABILITY_ID), ids.len() - 1);
    }

    /// System prompt is paid on every turn — keep it small enough that the
    /// first-turn input does not balloon for trivial requests. Bump
    /// intentionally and document why in the commit message; never raise
    /// silently.
    #[test]
    fn system_prompt_within_budget() {
        const MAX_BYTES: usize = 1_200;
        assert!(
            SYSTEM_PROMPT.len() <= MAX_BYTES,
            "SYSTEM_PROMPT is {} bytes (~{} tokens), cap is {} bytes",
            SYSTEM_PROMPT.len(),
            SYSTEM_PROMPT.len() / 4,
            MAX_BYTES,
        );
    }

    /// `SYSTEM_PROMPT` is under a tenth of what the model actually reads: the
    /// rest is capability blocks, and for a long time nothing watched their
    /// total. Trimming them once does not keep them trimmed — each new
    /// capability adds prose that looks small on its own — so the budget covers
    /// the sum.
    ///
    /// Only always-on blocks with no per-session state are listed; blocks that
    /// need a live store (`config`, `memory`, `user_ask`) are reveal-gated or
    /// dominated by data rather than prose. Adding a static block means adding
    /// it here, which is the point: growth becomes a deliberate edit to a cap,
    /// not a silent side effect.
    ///
    /// Before the Claude-5-generation prompt pass this totalled ~9,000 bytes.
    #[test]
    fn always_on_capability_prompts_within_budget() {
        use crate::capabilities::approval::render_approval_block;
        use crate::capabilities::attribution::yolop_attribution_prompt;
        use crate::capabilities::background::BACKGROUND_SYSTEM_PROMPT;
        use crate::capabilities::client_commands::CLIENT_COMMANDS_PROMPT;
        use crate::capabilities::host::SETUP_TOOLS_PROMPT;
        use crate::config::ApprovalMode;

        // Current total is 5,657; the headroom is deliberately thin.
        const MAX_BYTES: usize = 5_800;

        let approval = render_approval_block(ApprovalMode::Normal).expect("normal contributes");
        let blocks: Vec<(&str, usize)> = vec![
            ("system.md", SYSTEM_PROMPT.len()),
            ("approval", approval.len()),
            ("background", BACKGROUND_SYSTEM_PROMPT.len()),
            ("client_commands", CLIENT_COMMANDS_PROMPT.len()),
            ("setup", SETUP_TOOLS_PROMPT.len()),
            ("attribution", yolop_attribution_prompt().len()),
        ];

        let total: usize = blocks.iter().map(|(_, bytes)| bytes).sum();
        assert!(
            total <= MAX_BYTES,
            "always-on prompt blocks total {total} bytes (~{} tokens), cap is {MAX_BYTES}: {blocks:?}",
            total / 4,
        );
    }
}

//! Workspace/global hook config loading for yolop.
//!
//! The hook engine itself lives in `everruns-core` (`user_hooks`). yolop owns
//! only local config discovery, scope merge, and the `your` tools that write
//! those config files.

use crate::settings::SettingsStore;
use anyhow::{Context, Result, anyhow};
use everruns_core::user_hook_types::UserHookSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const GLOBAL_HOOKS_FILE_NAME: &str = "hooks.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HookScope {
    Global,
    Workspace,
}

impl HookScope {
    pub fn as_str(self) -> &'static str {
        match self {
            HookScope::Global => "global",
            HookScope::Workspace => "workspace",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "global" => Ok(Self::Global),
            "workspace" => Ok(Self::Workspace),
            other => Err(anyhow!(
                "unknown hook scope `{other}`; expected global or workspace"
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HooksFile {
    pub hooks: Vec<Value>,
    pub disabled: Vec<String>,
    pub disabled_contributions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedHook {
    pub scope: HookScope,
    pub path: PathBuf,
    pub hook: Value,
    pub id: Option<String>,
    pub event: String,
    pub description: Option<String>,
    pub matcher: Value,
}

impl ResolvedHook {
    fn from_value(scope: HookScope, path: PathBuf, hook: Value) -> Result<Self> {
        let spec = parse_hook_spec(&hook)?;
        Ok(Self {
            scope,
            path,
            hook,
            id: spec.id.clone(),
            event: spec.event.as_str().to_string(),
            description: spec.description.clone(),
            matcher: serde_json::to_value(&spec.matcher).unwrap_or_else(|_| json!({})),
        })
    }

    pub fn to_summary_json(&self) -> Value {
        json!({
            "id": self.id,
            "scope": self.scope.as_str(),
            "event": self.event,
            "matcher": self.matcher,
            "description": self.description,
            "path": self.path.display().to_string(),
        })
    }

    pub fn to_validation_json(&self) -> Value {
        json!({
            "id": self.id,
            "event": self.event,
            "matcher": self.matcher,
            "description": self.description,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveHooks {
    pub global_path: PathBuf,
    pub workspace_path: PathBuf,
    pub hooks: Vec<ResolvedHook>,
    pub disabled_contributions: Vec<String>,
}

impl EffectiveHooks {
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty() && self.disabled_contributions.is_empty()
    }

    pub fn capability_config(&self) -> Value {
        json!({
            "hooks": self.hooks.iter().map(|entry| entry.hook.clone()).collect::<Vec<_>>(),
            "disabled_contributions": self.disabled_contributions,
        })
    }

    pub fn scope_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for hook in &self.hooks {
            *counts.entry(hook.scope.as_str().to_string()).or_insert(0) += 1;
        }
        counts
    }

    pub fn summaries(&self) -> Vec<Value> {
        self.hooks
            .iter()
            .map(ResolvedHook::to_summary_json)
            .collect()
    }
}

#[derive(Debug, Clone)]
struct ScopedHooksFile {
    scope: HookScope,
    path: PathBuf,
    file: HooksFile,
}

#[derive(Debug, Clone)]
pub struct HooksStore {
    global_path: PathBuf,
    workspace_path: PathBuf,
}

impl HooksStore {
    pub fn new(global_path: PathBuf, workspace_root: PathBuf) -> Self {
        Self {
            global_path,
            workspace_path: workspace_hooks_config_path(&workspace_root),
        }
    }

    pub fn beside_settings(settings: &SettingsStore, workspace_root: PathBuf) -> Self {
        Self::new(
            global_hooks_config_path_beside_settings(settings),
            workspace_root,
        )
    }

    pub fn path_for(&self, scope: HookScope) -> &Path {
        match scope {
            HookScope::Global => &self.global_path,
            HookScope::Workspace => &self.workspace_path,
        }
    }

    pub fn effective(&self) -> EffectiveHooks {
        load_merged(Some(&self.global_path), &self.workspace_path)
    }

    pub fn validate_hook(&self, hook: &Value) -> Result<ResolvedHook> {
        ResolvedHook::from_value(HookScope::Global, self.global_path.clone(), hook.clone())
    }

    pub fn upsert_hook(&self, scope: HookScope, hook: Value) -> Result<ResolvedHook> {
        let parsed = parse_hook_spec(&hook)?;
        let id = parsed
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("hook id is required for upsert"))?
            .to_string();

        let path = self.path_for(scope).to_path_buf();
        let mut file = read_config_strict(&path)?;
        file.hooks
            .retain(|existing| hook_id(existing).as_deref() != Some(id.as_str()));
        file.disabled.retain(|disabled| disabled != &id);
        file.hooks.push(hook.clone());
        save_config(&path, &file)?;

        ResolvedHook::from_value(scope, path, hook)
    }

    pub fn remove_hook(&self, scope: HookScope, id: &str) -> Result<bool> {
        let id = id.trim();
        if id.is_empty() {
            return Err(anyhow!("hook id is required"));
        }
        let path = self.path_for(scope).to_path_buf();
        let mut file = read_config_strict(&path)?;
        let before = file.hooks.len();
        file.hooks
            .retain(|existing| hook_id(existing).as_deref() != Some(id));
        let removed = before != file.hooks.len();

        if scope == HookScope::Workspace && !file.disabled.iter().any(|disabled| disabled == id) {
            file.disabled.push(id.to_string());
        }
        save_config(&path, &file)?;
        Ok(removed)
    }
}

pub fn global_hooks_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("yolop").join(GLOBAL_HOOKS_FILE_NAME))
}

pub fn global_hooks_config_path_beside_settings(settings: &SettingsStore) -> PathBuf {
    settings
        .path()
        .parent()
        .map(|p| p.join(GLOBAL_HOOKS_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(GLOBAL_HOOKS_FILE_NAME))
}

pub fn workspace_hooks_config_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".agents").join(GLOBAL_HOOKS_FILE_NAME)
}

fn load_merged(global_path: Option<&Path>, workspace_path: &Path) -> EffectiveHooks {
    let global = global_path
        .map(|path| read_scope_best_effort(HookScope::Global, path))
        .unwrap_or_else(|| ScopedHooksFile {
            scope: HookScope::Global,
            path: global_hooks_config_path()
                .unwrap_or_else(|| PathBuf::from(GLOBAL_HOOKS_FILE_NAME)),
            file: HooksFile::default(),
        });
    let workspace = read_scope_best_effort(HookScope::Workspace, workspace_path);

    let mut hooks = resolve_scope_hooks(&global);
    for disabled in &workspace.file.disabled {
        hooks.retain(|entry| entry.id.as_deref() != Some(disabled.as_str()));
    }
    for entry in resolve_scope_hooks(&workspace) {
        if let Some(id) = entry.id.as_deref() {
            hooks.retain(|existing| existing.id.as_deref() != Some(id));
        }
        hooks.push(entry);
    }

    let mut disabled_contributions = global.file.disabled_contributions.clone();
    disabled_contributions.extend(workspace.file.disabled_contributions.clone());

    EffectiveHooks {
        global_path: global.path,
        workspace_path: workspace.path,
        hooks,
        disabled_contributions,
    }
}

fn resolve_scope_hooks(scope_file: &ScopedHooksFile) -> Vec<ResolvedHook> {
    let mut out = Vec::new();
    for hook in &scope_file.file.hooks {
        match ResolvedHook::from_value(scope_file.scope, scope_file.path.clone(), hook.clone()) {
            Ok(entry) => out.push(entry),
            Err(error) => tracing::warn!(
                path = %scope_file.path.display(),
                scope = scope_file.scope.as_str(),
                %error,
                "ignoring invalid hook entry"
            ),
        }
    }
    out
}

fn read_scope_best_effort(scope: HookScope, path: &Path) -> ScopedHooksFile {
    match read_config_strict(path) {
        Ok(file) => ScopedHooksFile {
            scope,
            path: path.to_path_buf(),
            file,
        },
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                scope = scope.as_str(),
                %error,
                "ignoring malformed hooks config"
            );
            ScopedHooksFile {
                scope,
                path: path.to_path_buf(),
                file: HooksFile::default(),
            }
        }
    }
}

fn read_config_strict(path: &Path) -> Result<HooksFile> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HooksFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn save_config(path: &Path, config: &HooksFile) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("create hooks dir {}", parent.display()))?;

    let content = serde_json::to_string_pretty(config).context("serialize hooks config")?;
    let file_name = path
        .file_name()
        .with_context(|| format!("hooks path has no file name: {}", path.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = parent.join(tmp_name);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);

    let write_result = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp_path)
            .with_context(|| format!("open temp hooks config {}", tmp_path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("write temp hooks config {}", tmp_path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("write temp hooks config {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp hooks config {}", tmp_path.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    #[cfg(windows)]
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error).with_context(|| format!("remove {}", path.display()));
        }
    }
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error)
            .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()));
    }
    Ok(())
}

fn parse_hook_spec(hook: &Value) -> Result<UserHookSpec> {
    let spec: UserHookSpec = serde_json::from_value(hook.clone()).context("parse hook spec")?;
    spec.validate().context("validate hook spec")?;
    Ok(spec)
}

fn hook_id(hook: &Value) -> Option<String> {
    hook.get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_string)
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block_git_hook(id: &str) -> Value {
        json!({
            "id": id,
            "event": "pre_tool_use",
            "matcher": {
                "tool_name": "bash",
                "args_jsonpath": "$.command",
                "match_regex": "(^|[;&|()[:space:]])git([[:space:]]|$)"
            },
            "executor": {
                "type": "bash",
                "command": "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"blocked\"}'"
            },
            "timeout_ms": 1000,
            "on_error": "block",
            "description": "Block git"
        })
    }

    fn malformed_hook(id: &str) -> Value {
        json!({
            "id": id,
            "event": "not_a_hook_event",
            "matcher": { "tool_name": "bash" },
            "executor": { "type": "bash", "command": "true" }
        })
    }

    #[test]
    fn missing_scopes_load_empty() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let effective = load_merged(Some(&global.path().join("hooks.json")), workspace.path());

        assert!(effective.is_empty());
        assert_eq!(effective.hooks.len(), 0);
    }

    #[test]
    fn workspace_overrides_global_by_id() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let global_path = global.path().join("hooks.json");
        let workspace_path = workspace.path().join("hooks.json");
        std::fs::write(
            &global_path,
            serde_json::to_string(&HooksFile {
                hooks: vec![block_git_hook("shared")],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();
        let mut ws_hook = block_git_hook("shared");
        ws_hook["description"] = json!("Workspace hook wins");
        std::fs::write(
            &workspace_path,
            serde_json::to_string(&HooksFile {
                hooks: vec![ws_hook],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();

        let effective = load_merged(Some(&global_path), &workspace_path);

        assert_eq!(effective.hooks.len(), 1);
        assert_eq!(effective.hooks[0].scope, HookScope::Workspace);
        assert_eq!(
            effective.hooks[0].description.as_deref(),
            Some("Workspace hook wins")
        );
    }

    #[test]
    fn workspace_disabled_removes_global_hook() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let global_path = global.path().join("hooks.json");
        let workspace_path = workspace.path().join("hooks.json");
        std::fs::write(
            &global_path,
            serde_json::to_string(&HooksFile {
                hooks: vec![block_git_hook("block-git")],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            &workspace_path,
            serde_json::to_string(&HooksFile {
                disabled: vec!["block-git".to_string()],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();

        let effective = load_merged(Some(&global_path), &workspace_path);

        assert!(effective.hooks.is_empty());
    }

    #[test]
    fn invalid_hook_entry_does_not_sink_valid_entries() {
        let global = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let global_path = global.path().join("hooks.json");
        std::fs::write(
            &global_path,
            serde_json::to_string(&HooksFile {
                hooks: vec![malformed_hook("bad"), block_git_hook("good")],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();

        let effective = load_merged(Some(&global_path), workspace.path());

        assert_eq!(effective.hooks.len(), 1);
        assert_eq!(effective.hooks[0].id.as_deref(), Some("good"));
    }

    #[test]
    fn upsert_and_remove_match_malformed_existing_entries_by_raw_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = HooksStore::new(tmp.path().join("hooks.json"), tmp.path().join("ws"));
        std::fs::write(
            store.path_for(HookScope::Global),
            serde_json::to_string(&HooksFile {
                hooks: vec![malformed_hook("block-git")],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();

        store
            .upsert_hook(HookScope::Global, block_git_hook("block-git"))
            .expect("replace malformed hook");
        let on_disk: HooksFile = serde_json::from_str(
            &std::fs::read_to_string(store.path_for(HookScope::Global)).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk.hooks.len(), 1);
        assert_eq!(hook_id(&on_disk.hooks[0]).as_deref(), Some("block-git"));
        assert_eq!(on_disk.hooks[0]["event"], json!("pre_tool_use"));

        std::fs::write(
            store.path_for(HookScope::Global),
            serde_json::to_string(&HooksFile {
                hooks: vec![malformed_hook("broken")],
                ..HooksFile::default()
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            store
                .remove_hook(HookScope::Global, "broken")
                .expect("remove malformed hook")
        );
        let on_disk: HooksFile = serde_json::from_str(
            &std::fs::read_to_string(store.path_for(HookScope::Global)).unwrap(),
        )
        .unwrap();
        assert!(on_disk.hooks.is_empty());
    }

    #[test]
    fn upsert_hook_requires_id_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = HooksStore::new(tmp.path().join("hooks.json"), tmp.path().join("ws"));

        let upserted = store
            .upsert_hook(HookScope::Global, block_git_hook("block-git"))
            .expect("upsert hook");

        assert_eq!(upserted.id.as_deref(), Some("block-git"));
        let on_disk =
            std::fs::read_to_string(store.path_for(HookScope::Global)).expect("read hooks");
        assert!(on_disk.contains("\"block-git\""));
    }

    #[test]
    fn remove_workspace_hook_adds_disable_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let store = HooksStore::new(tmp.path().join("hooks.json"), tmp.path().join("ws"));
        store
            .upsert_hook(HookScope::Workspace, block_git_hook("block-git"))
            .expect("upsert hook");

        assert!(
            store
                .remove_hook(HookScope::Workspace, "block-git")
                .expect("remove")
        );

        let on_disk =
            std::fs::read_to_string(store.path_for(HookScope::Workspace)).expect("read hooks");
        assert!(on_disk.contains("\"disabled\""));
        assert!(on_disk.contains("\"block-git\""));
    }
}

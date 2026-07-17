//! The `extensions` capability: the management surface for installing,
//! listing, enabling, and removing extension packages. Distinct from the
//! per-package `ext:<name>` capabilities — this one is always on the default
//! harness (like `connectors`) and owns the verbs. Tools so both the user
//! and the model can drive setup; a thin `/extensions` command mirrors them.

use super::package::{discover_extensions, extension_capability_id};
use super::scaffold::{self, HookSpec, Language, ScaffoldRequest, ToolSpec};
use super::store::{self, CrateFetcher, GitRunner, Source, SystemCrateFetcher, SystemGit};
use crate::settings::SettingsStore;
use async_trait::async_trait;
use everruns_core::capabilities::Capability;
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub const EXTENSIONS_CAPABILITY_ID: &str = "extensions";

pub struct ExtensionsCapability {
    extensions_dir: PathBuf,
    workspace_root: PathBuf,
    settings: Arc<SettingsStore>,
    git: Arc<dyn GitRunner>,
    crates: Arc<dyn CrateFetcher>,
}

impl ExtensionsCapability {
    pub fn new(
        extensions_dir: PathBuf,
        workspace_root: PathBuf,
        settings: Arc<SettingsStore>,
    ) -> Self {
        Self {
            extensions_dir,
            workspace_root,
            settings,
            git: Arc::new(SystemGit),
            crates: Arc::new(SystemCrateFetcher::default()),
        }
    }
}

#[async_trait]
impl Capability for ExtensionsCapability {
    fn id(&self) -> &str {
        EXTENSIONS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Extensions"
    }

    fn description(&self) -> &str {
        "Install, list, enable, and remove yolop extensions — capability-level \
         packages served over the extension protocol."
    }

    fn category(&self) -> Option<&str> {
        Some("Extensions")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(
            "Extensions are installable capability packages. Use `list_extensions` to see what is \
             installed and enabled, `install_extension` for a crates.io crate \
             (`crates.io:yolop-extension-<name>`), git URL, or local path, \
             `enable_extension`/`disable_extension` to toggle one in the harness (takes effect \
             next session), and `remove_extension` to uninstall. To build a NEW extension \
             yourself, `scaffold_extension` generates a ready-to-edit package (manifest + \
             capability server) — declare what it contributes via `tools`, `hooks`, and/or \
             `prompt` — then edit the generated `handle_*` bodies, `install_extension \
             source=<dir>`, `doctor_extension` to verify, and `enable_extension`. Installing \
             runs third-party code on the user's machine — confirm the source with the user \
             first.",
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        let ctx = Arc::new(ManageCtx {
            extensions_dir: self.extensions_dir.clone(),
            workspace_root: self.workspace_root.clone(),
            settings: self.settings.clone(),
            git: self.git.clone(),
            crates: self.crates.clone(),
        });
        vec![
            Box::new(ManageTool::new(ctx.clone(), Verb::Scaffold)),
            Box::new(ManageTool::new(ctx.clone(), Verb::List)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Install)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Remove)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Enable)),
            Box::new(ManageTool::new(ctx.clone(), Verb::Disable)),
            Box::new(ManageTool::new(ctx, Verb::Doctor)),
        ]
    }
}

struct ManageCtx {
    extensions_dir: PathBuf,
    workspace_root: PathBuf,
    settings: Arc<SettingsStore>,
    git: Arc<dyn GitRunner>,
    crates: Arc<dyn CrateFetcher>,
}

#[derive(Clone, Copy)]
enum Verb {
    Scaffold,
    List,
    Install,
    Remove,
    Enable,
    Disable,
    Doctor,
}

struct ManageTool {
    ctx: Arc<ManageCtx>,
    verb: Verb,
}

impl ManageTool {
    fn new(ctx: Arc<ManageCtx>, verb: Verb) -> Self {
        Self { ctx, verb }
    }

    fn scaffold(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        let language = match Language::parse(
            args.get("language")
                .and_then(Value::as_str)
                .unwrap_or("python"),
        ) {
            Ok(lang) => lang,
            Err(err) => return ToolExecutionResult::ToolError(err),
        };
        let tools = args
            .get("tools")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        let name = t.get("name").and_then(Value::as_str)?.to_string();
                        let description = t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some(ToolSpec { name, description })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hooks = args
            .get("hooks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|h| {
                        let event = h.get("event").and_then(Value::as_str)?.to_string();
                        let tool_name_glob = h
                            .get("tool_name_glob")
                            .and_then(Value::as_str)
                            .unwrap_or("*")
                            .to_string();
                        Some(HookSpec {
                            event,
                            tool_name_glob,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Default target: a sibling `scaffold/<name>` of the extensions dir, so
        // authored packages sit next to where they install without cluttering
        // the workspace. An explicit `dir` (a parent) overrides.
        let dir = match args.get("dir").and_then(Value::as_str) {
            Some(parent) => PathBuf::from(parent).join(name),
            None => self
                .ctx
                .extensions_dir
                .parent()
                .unwrap_or(&self.ctx.extensions_dir)
                .join("scaffold")
                .join(name),
        };
        let req = ScaffoldRequest {
            name: name.to_string(),
            description: args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            language,
            tools,
            hooks,
            prompt,
            dir,
        };
        match scaffold::scaffold(&req) {
            Ok(out) => ToolExecutionResult::Success(json!({
                "scaffolded": name,
                "dir": out.dir.display().to_string(),
                "server": out.server.display().to_string(),
                "files": out.files,
                "next": format!(
                    "Edit the handle_* bodies in {}, then: install_extension source={} → \
                     doctor_extension name={name} → enable_extension name={name} (next session).",
                    out.server.display(),
                    out.dir.display(),
                ),
            })),
            Err(err) => ToolExecutionResult::ToolError(format!("scaffold failed: {err}")),
        }
    }

    fn list(&self) -> ToolExecutionResult {
        let installed = discover_extensions(&self.ctx.extensions_dir);
        let settings = self.ctx.settings.snapshot();
        let items: Vec<Value> = installed
            .iter()
            .map(|pkg| {
                let cap_id = extension_capability_id(&pkg.manifest.name);
                let enabled = settings
                    .capability_overrides_for(&cap_id)
                    .iter()
                    .any(|(_, entry)| !entry.is_remove());
                json!({
                    "name": pkg.manifest.name,
                    "description": pkg.manifest.description,
                    "version": pkg.manifest.version,
                    "enabled": enabled,
                    "capability_ref": cap_id,
                    "tools": pkg.manifest.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                })
            })
            .collect();
        ToolExecutionResult::Success(json!({ "extensions": items }))
    }

    async fn install(&self, args: &Value) -> ToolExecutionResult {
        let Some(spec) = args.get("source").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`source` is required".into());
        };
        let source = match Source::parse(spec) {
            Ok(source) => source,
            Err(err) => return ToolExecutionResult::ToolError(err.to_string()),
        };
        match store::install(
            &self.ctx.extensions_dir,
            &source,
            self.ctx.git.as_ref(),
            self.ctx.crates.as_ref(),
        )
        .await
        {
            Ok(installed) => {
                let m = &installed.manifest;
                ToolExecutionResult::Success(json!({
                    "installed": m.name,
                    "version": m.version,
                    // The contribution summary the user consents to (D4).
                    "contributes": {
                        "server_command": m.capability_server.command,
                        "tools": m.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                        "prompt": m.prompt,
                    },
                    "content_hash": installed.content_hash,
                    "grant_changed": installed.previous_hash.is_some(),
                    "note": format!(
                        "Installed but not enabled. Run enable_extension name={} to add it to the harness.",
                        m.name
                    ),
                }))
            }
            Err(err) => ToolExecutionResult::ToolError(format!("install failed: {err}")),
        }
    }

    fn remove(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        // Also drop the enabling override so a reinstall starts clean.
        let _ = self
            .ctx
            .settings
            .set_capability_enabled(&extension_capability_id(name), false);
        match store::remove(&self.ctx.extensions_dir, name) {
            Ok(true) => ToolExecutionResult::Success(json!({ "removed": name })),
            Ok(false) => ToolExecutionResult::ToolError(format!("no extension named `{name}`")),
            Err(err) => ToolExecutionResult::ToolError(format!("remove failed: {err}")),
        }
    }

    fn toggle(&self, args: &Value, enable: bool) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        if enable
            && !discover_extensions(&self.ctx.extensions_dir)
                .iter()
                .any(|pkg| pkg.manifest.name == name)
        {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed; install_extension first"
            ));
        }
        let cap_id = extension_capability_id(name);
        match self.ctx.settings.set_capability_enabled(&cap_id, enable) {
            Ok(changed) => ToolExecutionResult::Success(json!({
                "name": name,
                "enabled": enable,
                "changed": changed,
                "note": "Takes effect on the next session.",
            })),
            Err(err) => ToolExecutionResult::ToolError(format!("config write failed: {err}")),
        }
    }

    async fn doctor(&self, args: &Value) -> ToolExecutionResult {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolExecutionResult::ToolError("`name` is required".into());
        };
        let Some(pkg) = discover_extensions(&self.ctx.extensions_dir)
            .into_iter()
            .find(|pkg| pkg.manifest.name == name)
        else {
            return ToolExecutionResult::ToolError(format!(
                "no extension named `{name}` is installed"
            ));
        };
        // Bounded probe: spawn the server, handshake, check, shut down.
        let report = super::doctor::doctor(
            &pkg.manifest,
            &pkg.dir,
            &self.ctx.workspace_root,
            std::time::Duration::from_secs(20),
        )
        .await;
        ToolExecutionResult::Success(
            json!({ "name": name, "ok": report.ok, "checks": report.checks }),
        )
    }
}

#[async_trait]
impl Tool for ManageTool {
    fn name(&self) -> &str {
        match self.verb {
            Verb::Scaffold => "scaffold_extension",
            Verb::List => "list_extensions",
            Verb::Install => "install_extension",
            Verb::Remove => "remove_extension",
            Verb::Enable => "enable_extension",
            Verb::Disable => "disable_extension",
            Verb::Doctor => "doctor_extension",
        }
    }

    fn description(&self) -> &str {
        match self.verb {
            Verb::Scaffold => {
                "Scaffold a new, ready-to-edit extension package (manifest + a self-contained \
                 capability server) so you can author an extension end-to-end. Generates a \
                 correct-by-construction skeleton that installs and passes doctor out of the \
                 box; you then fill in the `handle_*` bodies. Declare what it contributes via \
                 `tools`, `hooks`, and/or `prompt`. After editing, install it with \
                 `install_extension source=<dir>`, verify with `doctor_extension`, and turn it \
                 on with `enable_extension` (effective next session)."
            }
            Verb::List => "List installed extensions and whether each is enabled.",
            Verb::Install => {
                "Install an extension from crates.io (`crates.io:yolop-extension-<name>[@ver]`, \
                 or the bare `<name>` shorthand), a git URL (`https://…[@rev]`), or a local \
                 path. crates.io installs are toolchain-free (no cargo/rustc). Runs third-party \
                 code; confirm the source with the user. Does not enable it."
            }
            Verb::Remove => "Uninstall an extension by name and drop its enable override.",
            Verb::Enable => {
                "Enable an installed extension (adds `ext:<name>` to the harness). Effective next session."
            }
            Verb::Disable => {
                "Disable an extension without uninstalling it. Effective next session."
            }
            Verb::Doctor => {
                "Conformance-check an installed extension: spawn its server, run the \
                 protocol handshake, and verify its tools/prompt against the manifest. \
                 Returns per-check pass/warn/fail. Use to diagnose why an extension \
                 isn't working."
            }
        }
    }

    fn parameters_schema(&self) -> Value {
        match self.verb {
            Verb::Scaffold => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string",
                        "description": "Extension name (ascii letters, digits, `-`, `_`)." },
                    "description": { "type": "string",
                        "description": "One-line summary of what the extension does." },
                    "language": { "type": "string", "enum": ["python", "typescript"],
                        "default": "python",
                        "description": "Server language template. Both are single-file and \
                            toolchain-free (no build step); `typescript` emits a dependency-free \
                            Node.js server." },
                    "tools": { "type": "array", "description": "Tool contributions.",
                        "items": { "type": "object", "properties": {
                            "name": { "type": "string" },
                            "description": { "type": "string" }
                        }, "required": ["name"] } },
                    "hooks": { "type": "array",
                        "description": "Lifecycle hook subscriptions. A pre_tool_use hook can \
                            block a tool call (e.g. deny git).",
                        "items": { "type": "object", "properties": {
                            "event": { "type": "string",
                                "enum": ["pre_tool_use", "post_tool_use"] },
                            "tool_name_glob": { "type": "string", "default": "*",
                                "description": "Glob over tool names to fire for." }
                        }, "required": ["event"] } },
                    "prompt": { "type": "string",
                        "description": "Static system-prompt contribution text, if any." },
                    "dir": { "type": "string",
                        "description": "Parent directory to create `<name>/` under. Defaults to \
                            a `scaffold/` dir beside the extensions store." }
                },
                "required": ["name"]
            }),
            Verb::List => json!({ "type": "object", "properties": {} }),
            Verb::Install => json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string",
                        "description": "`crates.io:<crate>[@ver]` (or bare `<name>` → \
                            `yolop-extension-<name>`), a git URL (optionally `@rev`), or a \
                            local directory path." }
                },
                "required": ["source"]
            }),
            _ => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Extension name." }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        match self.verb {
            Verb::Scaffold => self.scaffold(&arguments),
            Verb::List => self.list(),
            Verb::Install => self.install(&arguments).await,
            Verb::Remove => self.remove(&arguments),
            Verb::Enable => self.toggle(&arguments, true),
            Verb::Disable => self.toggle(&arguments, false),
            Verb::Doctor => self.doctor(&arguments).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::package::MANIFEST_FILE;
    use std::path::Path;

    fn seed_package(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            json!({
                "name": name, "description": "T.",
                "yolop": { "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" }, "tools": [{"name": "t"}] }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn capability(tmp: &Path) -> (ExtensionsCapability, Arc<SettingsStore>, PathBuf) {
        let ext_dir = tmp.join("extensions");
        let settings = Arc::new(SettingsStore::open(tmp.join("settings.toml")));
        let cap = ExtensionsCapability {
            extensions_dir: ext_dir.clone(),
            workspace_root: tmp.to_path_buf(),
            settings: settings.clone(),
            git: Arc::new(crate::extensions::store::SystemGit),
            crates: Arc::new(crate::extensions::store::SystemCrateFetcher::default()),
        };
        (cap, settings, ext_dir)
    }

    #[tokio::test]
    async fn scaffold_then_install_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let (cap, _settings, _ext_dir) = capability(tmp.path());
        let tools = cap.tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();

        // Scaffold a hook extension into an explicit parent dir.
        let parent = tmp.path().join("authored");
        let dir = match get("scaffold_extension")
            .execute(json!({
                "name": "git-guard",
                "description": "Blocks git.",
                "hooks": [{ "event": "pre_tool_use" }],
                "dir": parent.to_str().unwrap(),
            }))
            .await
        {
            ToolExecutionResult::Success(v) => {
                assert_eq!(v["scaffolded"], "git-guard");
                assert!(parent.join("git-guard").join("plugin.json").exists());
                v["dir"].as_str().unwrap().to_string()
            }
            other => panic!("{other:?}"),
        };

        // The scaffolded package installs (manifest parses, tree copied).
        match get("install_extension")
            .execute(json!({ "source": dir }))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["installed"], "git-guard"),
            other => panic!("{other:?}"),
        }
        match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => assert_eq!(v["extensions"][0]["name"], "git-guard"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn scaffold_rejects_no_contributions() {
        let tmp = tempfile::tempdir().unwrap();
        let (cap, _s, _e) = capability(tmp.path());
        let tools = cap.tools();
        let scaffold = tools
            .iter()
            .find(|t| t.name() == "scaffold_extension")
            .unwrap();
        match scaffold.execute(json!({ "name": "empty" })).await {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("contribute"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn install_list_enable_disable_remove_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        seed_package(&src, "echo");
        let (cap, settings, _ext_dir) = capability(tmp.path());
        let tools = cap.tools();
        let get = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();

        // install
        let r = get("install_extension")
            .execute(json!({ "source": src.to_str().unwrap() }))
            .await;
        assert!(matches!(r, ToolExecutionResult::Success(_)));

        // list shows it, disabled
        match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => {
                assert_eq!(v["extensions"][0]["name"], "echo");
                assert_eq!(v["extensions"][0]["enabled"], false);
            }
            other => panic!("{other:?}"),
        }

        // enable writes the override
        get("enable_extension")
            .execute(json!({"name": "echo"}))
            .await;
        assert!(
            settings
                .snapshot()
                .capability_overrides_for("ext:echo")
                .iter()
                .any(|(_, e)| !e.is_remove())
        );
        // list now enabled
        match get("list_extensions").execute(json!({})).await {
            ToolExecutionResult::Success(v) => assert_eq!(v["extensions"][0]["enabled"], true),
            other => panic!("{other:?}"),
        }

        // disable drops it
        get("disable_extension")
            .execute(json!({"name": "echo"}))
            .await;
        assert!(
            settings
                .snapshot()
                .capability_overrides_for("ext:echo")
                .is_empty()
        );

        // enable on a missing extension errors
        match get("enable_extension")
            .execute(json!({"name": "ghost"}))
            .await
        {
            ToolExecutionResult::ToolError(m) => assert!(m.contains("no extension")),
            other => panic!("{other:?}"),
        }

        // remove
        match get("remove_extension")
            .execute(json!({"name": "echo"}))
            .await
        {
            ToolExecutionResult::Success(v) => assert_eq!(v["removed"], "echo"),
            other => panic!("{other:?}"),
        }
    }
}

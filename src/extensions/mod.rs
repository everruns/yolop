//! Extensions: installable capability-level packages served over YEP, the
//! yolop extension protocol. See `specs/extensions.md`. Implemented:
//! the protocol core + persistent process manager + generic
//! `ExtensionCapability` adapter (phase 1); install/enable management
//! surface + lockfile (phase 2); contributed MCP servers (phase 3);
//! hook subscriptions + dynamic prompt over RPC (phase 4); the
//! `doctor_extension` conformance probe (`doctor` module); toolchain-free
//! `crates.io` install (`store::SystemCrateFetcher`). Later: `ui/ask`,
//! `workspace/changed`, providers.
//!
//! Registration: discovered packages are registered in the capability
//! registry (so they appear in the catalog and validate config) but are
//! NOT on the default harness — users enable one with
//! `[[capabilities]] ref = "ext:<name>"` in settings.toml, exactly like
//! the built-in `lsp`.

pub(crate) mod capability;
pub(crate) mod client;
pub(crate) mod doctor;
pub(crate) mod hooks;
pub(crate) mod manage;
pub(crate) mod manager;
pub(crate) mod package;
pub(crate) mod protocol;
pub(crate) mod scaffold;
pub(crate) mod store;

pub(crate) use capability::ExtensionCapability;
pub(crate) use client::{AskSink, StatusSink};
pub(crate) use manage::ExtensionsCapability;
pub(crate) use manager::LiveProcessRegistry;
pub(crate) use package::{
    discover_extensions, extension_capability_id, extension_skill_scopes, extensions_dir,
};

#[cfg(test)]
mod spawn_tests {
    //! End-to-end proof over a real child process: a minimal Python YEP
    //! server (`tests/fixtures/yep_echo_server.py`) is spawned, handshaken,
    //! and driven through a streamed tool call — the conformance shape the
    //! future `/extensions doctor` will automate.

    use super::capability::ExtensionCapability;
    use super::package::{ExtensionPackage, parse_manifest};
    use crate::capabilities::EnvironmentContextRegistry;
    use everruns_core::capabilities::Capability;
    use everruns_core::tools::ToolExecutionResult;
    use serde_json::json;

    fn python3() -> Option<String> {
        which_python(&["python3", "python"])
    }

    fn have_cargo() -> bool {
        std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// A package whose capability server is the `yolop-yep` `echo` example,
    /// run via `cargo run --example` so the Rust SDK server is exercised by
    /// yolop's real client over the wire (the SDK's interop proof).
    fn sdk_example_package() -> ExtensionPackage {
        let manifest_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/yolop-yep/Cargo.toml");
        let manifest = parse_manifest(
            &json!({
                "name": "echo",
                "description": "yolop-yep SDK example.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": {
                        "command": "cargo",
                        "args": ["run", "-q", "--example", "echo",
                                 "--manifest-path", manifest_path.display().to_string()]
                    },
                    "tools": [
                        { "name": "echo", "description": "Echo text.", "never_defer": true }
                    ],
                    "prompt": true,
                    "dynamic_prompt": true,
                    "hooks": [
                        { "event": "pre_tool_use", "tool_name_glob": "*" }
                    ]
                }
            })
            .to_string(),
        )
        .expect("sdk example manifest");
        ExtensionPackage {
            dir: std::env::temp_dir(),
            manifest,
        }
    }

    #[tokio::test]
    async fn yolop_yep_sdk_example_server_interops_with_the_host() {
        if !have_cargo() {
            eprintln!("skipping: cargo not available");
            return;
        }
        let capability = ExtensionCapability::new(sdk_example_package(), std::env::temp_dir());

        // Tool call round-trips through the SDK-built server.
        let tools = capability.tools();
        let echo = tools.iter().find(|t| t.name() == "echo").expect("echo");
        match echo.execute(json!({ "text": "via-sdk" })).await {
            ToolExecutionResult::Success(v) => assert_eq!(v["echoed"], "via-sdk"),
            other => panic!("expected success, got {other:?}"),
        }

        // Dynamic prompt comes from the SDK server's handler.
        let ctx = everruns_core::capabilities::SystemPromptContext::without_file_store(
            everruns_core::typed_id::SessionId::new(),
        );
        let prompt = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("prompt");
        assert!(prompt.contains("dynamic echo prompt"), "{prompt}");

        // Pre-hook served by the SDK blocks a forbidden call.
        use everruns_core::atoms::PreToolUseDecision;
        use everruns_core::tool_types::{BuiltinTool, ToolCall, ToolDefinition};
        let hooks = capability.pre_tool_use_hooks_with_config(&json!(null));
        let tool_def = ToolDefinition::Builtin(BuiltinTool {
            name: "bash".into(),
            display_name: None,
            description: "run".into(),
            parameters: json!({ "type": "object" }),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints: Default::default(),
            full_parameters: None,
        });
        let ctx2 =
            everruns_core::traits::ToolContext::new(everruns_core::typed_id::SessionId::new());
        let deny = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({ "forbidden": true }),
        };
        assert!(matches!(
            hooks[0].before_exec(deny, &tool_def, &ctx2).await,
            PreToolUseDecision::Block { .. }
        ));
    }

    fn which_python(candidates: &[&str]) -> Option<String> {
        for candidate in candidates {
            if std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
            {
                return Some(candidate.to_string());
            }
        }
        None
    }

    fn fixture_package(python: &str) -> ExtensionPackage {
        let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/yep_echo_server.py");
        let manifest = parse_manifest(
            &json!({
                "name": "echo",
                "description": "Echo fixture.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": {
                        "command": python,
                        "args": [server.display().to_string()]
                    },
                    "tools": [
                        { "name": "echo", "description": "Echo text.",
                          "schema": { "type": "object" }, "never_defer": true },
                        // Approved in the manifest but the fixture doesn't
                        // serve it — exercises the served-tools gate.
                        { "name": "unserved", "description": "Never served." }
                    ],
                    "prompt": true
                }
            })
            .to_string(),
        )
        .expect("fixture manifest");
        ExtensionPackage {
            dir: std::env::temp_dir(),
            manifest,
        }
    }

    #[tokio::test]
    async fn spawns_real_server_handshakes_and_calls_tool() {
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let capability = ExtensionCapability::new(fixture_package(&python), std::env::temp_dir());
        assert_eq!(capability.id(), "ext:echo");
        assert_eq!(capability.never_defer_tools(), vec!["echo".to_string()]);

        let tools = capability.tools();
        let echo = tools
            .iter()
            .find(|tool| tool.name() == "echo")
            .expect("echo tool");
        match echo.execute(json!({"text": "round-trip"})).await {
            ToolExecutionResult::Success(value) => {
                assert_eq!(value["echoed"], "round-trip");
            }
            other => panic!("expected success, got {other:?}"),
        }

        // A manifest-approved tool the server did not declare is refused at
        // call time with a clear message.
        let unserved = tools
            .iter()
            .find(|tool| tool.name() == "unserved")
            .expect("unserved tool");
        match unserved.execute(json!({})).await {
            ToolExecutionResult::ToolError(message) => {
                assert!(message.contains("not served"), "{message}");
            }
            other => panic!("expected tool error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prompt_contribution_updates_trailing_environment_context() {
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let registry = EnvironmentContextRegistry::default();
        let capability = ExtensionCapability::new(fixture_package(&python), std::env::temp_dir())
            .with_environment_context(registry.clone());
        let ctx = everruns_core::capabilities::SystemPromptContext::without_file_store(
            everruns_core::typed_id::SessionId::new(),
        );
        let contribution = capability.system_prompt_contribution(&ctx).await;
        assert_eq!(contribution, None);
        assert_eq!(
            registry
                .snapshot()
                .get("extension_echo")
                .map(String::as_str),
            Some("echo fixture prompt")
        );
    }

    /// A hook + dynamic-prompt manifest whose server is the same fixture.
    fn hooks_package(python: &str) -> ExtensionPackage {
        let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/yep_echo_server.py");
        let manifest = parse_manifest(
            &json!({
                "name": "echo",
                "description": "Echo fixture.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": python,
                        "args": [server.display().to_string()] },
                    "dynamic_prompt": true,
                    "hooks": [
                        { "event": "pre_tool_use", "tool_name_glob": "*",
                          "timeout_ms": 5000, "on_error": "warn" }
                    ]
                }
            })
            .to_string(),
        )
        .expect("hooks manifest");
        ExtensionPackage {
            dir: std::env::temp_dir(),
            manifest,
        }
    }

    #[tokio::test]
    async fn pre_tool_use_hook_blocks_via_server_decision() {
        use everruns_core::atoms::PreToolUseDecision;
        use everruns_core::tool_types::{BuiltinTool, ToolCall, ToolDefinition};
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let capability = ExtensionCapability::new(hooks_package(&python), std::env::temp_dir());
        let hooks = capability.pre_tool_use_hooks_with_config(&json!(null));
        assert_eq!(hooks.len(), 1);
        let hook = &hooks[0];
        let tool_def = ToolDefinition::Builtin(BuiltinTool {
            name: "bash".into(),
            display_name: None,
            description: "run".into(),
            parameters: json!({ "type": "object" }),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints: Default::default(),
            full_parameters: None,
        });
        let ctx =
            everruns_core::traits::ToolContext::new(everruns_core::typed_id::SessionId::new());

        // Allowed call passes through.
        let allow = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        };
        assert!(matches!(
            hook.before_exec(allow, &tool_def, &ctx).await,
            PreToolUseDecision::Continue(_)
        ));

        // Server blocks a call whose args carry {"forbidden": true}.
        let deny = ToolCall {
            id: "2".into(),
            name: "bash".into(),
            arguments: json!({"forbidden": true}),
        };
        match hook.before_exec(deny, &tool_def, &ctx).await {
            PreToolUseDecision::Block { reason, .. } => {
                assert!(reason.contains("forbidden"), "{reason}");
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    /// The self-writing acceptance case, run for one language template:
    /// `scaffold_extension` produces a package whose generated server, once the
    /// author fills in the hook body, blocks a real tool call when spawned
    /// exactly as the runtime spawns it (command resolved via the package's
    /// `bin/` on PATH — no absolute paths, no build step).
    async fn assert_scaffolded_git_block(language: super::scaffold::Language) {
        use super::scaffold::{HookSpec, ScaffoldRequest, scaffold};
        use everruns_core::atoms::PreToolUseDecision;
        use everruns_core::tool_types::{BuiltinTool, ToolCall, ToolDefinition};

        if which_python(&[language.interpreter()]).is_none() {
            eprintln!("skipping: {} not available", language.interpreter());
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let out = scaffold(&ScaffoldRequest {
            name: "git-guard".into(),
            description: "Blocks git.".into(),
            language,
            tools: vec![],
            hooks: vec![HookSpec {
                event: "pre_tool_use".into(),
                tool_name_glob: "*".into(),
            }],
            commands: Vec::new(),
            prompt: None,
            status: false,
            skills: false,
            dir: tmp.path().join("git-guard"),
        })
        .unwrap();

        // Author step: fill in the hook body the scaffold left as a no-op.
        let server = std::fs::read_to_string(&out.edit).unwrap();
        let filled = match language {
            super::scaffold::Language::Python => server.replace(
                "\n    return {}\n\n\ndef handle_prompt",
                "\n    command = (args or {}).get(\"command\", \"\")\n    \
                 if event == \"pre_tool_use\" and \"git\" in command.split():\n        \
                 return {\"block\": True, \"reason\": \"git is disabled by git-guard\"}\n    \
                 return {}\n\n\ndef handle_prompt",
            ),
            super::scaffold::Language::Node => server.replace(
                "\n  return {};\n}\n\nfunction handlePrompt",
                "\n  const command = (args || {}).command || \"\";\n  \
                 if (event === \"pre_tool_use\" && command.split(/\\s+/).includes(\"git\")) {\n    \
                 return { block: true, reason: \"git is disabled by git-guard\" };\n  }\n  \
                 return {};\n}\n\nfunction handlePrompt",
            ),
            super::scaffold::Language::Rust => server.replace(
                "    let _ = (event, tool_name, args);\n    json!({})",
                "    if event == \"pre_tool_use\" {\n        \
                 let command = args.get(\"command\").and_then(Value::as_str).unwrap_or(\"\");\n        \
                 if command.split_whitespace().any(|w| w == \"git\") {\n            \
                 return json!({\"block\": true, \"reason\": \"git is disabled by git-guard\"});\n        }\n    }\n    \
                 let _ = tool_name;\n    json!({})",
            ),
        };
        assert_ne!(filled, server, "hook body anchor must match");
        std::fs::write(&out.edit, filled).unwrap();

        // Rust is compiled: run the build step, then drop the binary in bin/.
        // Build (debug, isolated in the package's own target/) — the same shape
        // as `out.build`, which the author/skill runs before install.
        if out.build.is_some() {
            let status = std::process::Command::new("cargo")
                .args(["build", "--manifest-path"])
                .arg(out.dir.join("Cargo.toml"))
                .status()
                .expect("cargo build");
            assert!(status.success(), "scaffolded Rust crate must build");
            std::fs::copy(
                out.dir
                    .join("target")
                    .join("debug")
                    .join("git-guard-server"),
                out.dir.join("bin").join("git-guard-server"),
            )
            .expect("copy built binary into bin/");
        }

        let manifest =
            parse_manifest(&std::fs::read_to_string(out.dir.join("plugin.json")).unwrap()).unwrap();
        let capability = ExtensionCapability::new(
            ExtensionPackage {
                dir: out.dir.clone(),
                manifest,
            },
            tmp.path().to_path_buf(),
        );
        let hooks = capability.pre_tool_use_hooks_with_config(&json!(null));
        let hook = &hooks[0];
        let tool_def = ToolDefinition::Builtin(BuiltinTool {
            name: "bash".into(),
            display_name: None,
            description: "run".into(),
            parameters: json!({ "type": "object" }),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints: Default::default(),
            full_parameters: None,
        });
        let ctx =
            everruns_core::traits::ToolContext::new(everruns_core::typed_id::SessionId::new());

        // A git command is denied by the self-authored server.
        let deny = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({ "command": "git status" }),
        };
        match hook.before_exec(deny, &tool_def, &ctx).await {
            PreToolUseDecision::Block { reason, .. } => assert!(reason.contains("git"), "{reason}"),
            other => panic!("expected block, got {other:?}"),
        }

        // A non-git command passes through.
        let allow = ToolCall {
            id: "2".into(),
            name: "bash".into(),
            arguments: json!({ "command": "ls -la" }),
        };
        assert!(matches!(
            hook.before_exec(allow, &tool_def, &ctx).await,
            PreToolUseDecision::Continue(_)
        ));
    }

    #[tokio::test]
    async fn scaffolded_python_extension_blocks_git_end_to_end() {
        assert_scaffolded_git_block(super::scaffold::Language::Python).await;
    }

    #[tokio::test]
    async fn scaffolded_node_extension_blocks_git_end_to_end() {
        assert_scaffolded_git_block(super::scaffold::Language::Node).await;
    }

    #[tokio::test]
    async fn scaffolded_rust_extension_blocks_git_end_to_end() {
        assert_scaffolded_git_block(super::scaffold::Language::Rust).await;
    }

    /// Slash-command acceptance: a scaffolded extension declaring a command
    /// exposes it namespaced (`<ext>:<cmd>`) via `commands()`, and invoking it
    /// routes `command/execute` to the server, whose message comes back.
    #[tokio::test]
    async fn scaffolded_extension_serves_a_slash_command() {
        use super::scaffold::{Language, ScaffoldRequest, scaffold};
        use everruns_core::capabilities::Capability;
        use everruns_core::command::{CommandExecutionContext, ExecuteCommandRequest};

        if python3().is_none() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let out = scaffold(&ScaffoldRequest {
            name: "greeter".into(),
            description: "Greets.".into(),
            language: Language::Python,
            tools: vec![],
            hooks: vec![],
            commands: vec!["hello".into()],
            prompt: None,
            status: false,
            skills: false,
            dir: tmp.path().join("greeter"),
        })
        .unwrap();

        let manifest =
            parse_manifest(&std::fs::read_to_string(out.dir.join("plugin.json")).unwrap()).unwrap();
        assert_eq!(manifest.commands.len(), 1);
        let capability = ExtensionCapability::new(
            ExtensionPackage {
                dir: out.dir.clone(),
                manifest,
            },
            tmp.path().to_path_buf(),
        );

        // The command is exposed namespaced so it can't shadow a built-in.
        let commands = capability.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "greeter:hello");

        // Invoking it round-trips to the server (default handler echoes args).
        let request = ExecuteCommandRequest {
            name: "greeter:hello".into(),
            arguments: Some("world".into()),
            controls: None,
        };
        let result = capability
            .execute_command(
                &request,
                &CommandExecutionContext::without_host(everruns_core::typed_id::SessionId::new()),
            )
            .await
            .expect("command executes");
        assert!(result.success, "{result:?}");
        assert!(
            result.message.contains("greeter:hello") && result.message.contains("world"),
            "server message should echo the command + args: {:?}",
            result.message
        );
    }

    /// The status-bar acceptance case: `scaffold_extension` with `status` yields
    /// a package whose server, after the author fills the hook to count and
    /// `emit_status`, pushes `status/changed` — and the host routes it to the
    /// status-bar sink, attributed to the extension.
    #[tokio::test]
    async fn scaffolded_status_extension_pushes_to_the_sink() {
        use super::client::StatusSink;
        use super::scaffold::{HookSpec, Language, ScaffoldRequest, scaffold};
        use everruns_core::atoms::PreToolUseDecision;
        use everruns_core::tool_types::{BuiltinTool, ToolCall, ToolDefinition};
        use std::sync::{Arc, Mutex};

        if python3().is_none() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let out = scaffold(&ScaffoldRequest {
            name: "char-counter".into(),
            description: "Counts characters seen in tool calls.".into(),
            language: Language::Python,
            tools: vec![],
            hooks: vec![HookSpec {
                event: "pre_tool_use".into(),
                tool_name_glob: "*".into(),
            }],
            commands: Vec::new(),
            prompt: None,
            status: true,
            skills: false,
            dir: tmp.path().join("char-counter"),
        })
        .unwrap();

        // Author step: count characters across observed tool-call args and push
        // the running total to the status bar via the generated emit_status().
        let server = std::fs::read_to_string(&out.edit).unwrap();
        let filled = server.replace(
            "\n    return {}\n\n\ndef handle_prompt",
            "\n    handle_hook.total = getattr(handle_hook, \"total\", 0) + len(json.dumps(args))\n    \
             emit_status(str(handle_hook.total) + \" chars\")\n    return {}\n\n\ndef handle_prompt",
        );
        assert_ne!(filled, server, "hook body anchor must match");
        std::fs::write(&out.edit, filled).unwrap();

        let recorded: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink: StatusSink = {
            let recorded = recorded.clone();
            Arc::new(move |ext: &str, p: super::protocol::StatusChangedParams| {
                recorded.lock().unwrap().push((ext.to_string(), p.status));
            })
        };
        let manifest =
            parse_manifest(&std::fs::read_to_string(out.dir.join("plugin.json")).unwrap()).unwrap();
        assert!(manifest.status, "manifest must declare the status facet");
        let capability = ExtensionCapability::new(
            ExtensionPackage {
                dir: out.dir.clone(),
                manifest,
            },
            tmp.path().to_path_buf(),
        )
        .with_status_sink(Some(sink));

        let hooks = capability.pre_tool_use_hooks_with_config(&json!(null));
        let tool_def = ToolDefinition::Builtin(BuiltinTool {
            name: "bash".into(),
            display_name: None,
            description: "run".into(),
            parameters: json!({ "type": "object" }),
            policy: Default::default(),
            category: None,
            deferrable: Default::default(),
            hints: Default::default(),
            full_parameters: None,
        });
        let ctx =
            everruns_core::traits::ToolContext::new(everruns_core::typed_id::SessionId::new());
        let call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({ "command": "git status" }),
        };
        // The hook allows the call (it only counts) and pushes a status first.
        assert!(matches!(
            hooks[0].before_exec(call, &tool_def, &ctx).await,
            PreToolUseDecision::Continue(_)
        ));

        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1, "one status push expected: {recorded:?}");
        assert_eq!(recorded[0].0, "char-counter");
        assert!(
            recorded[0].1.ends_with("chars"),
            "status should be the char counter: {:?}",
            recorded[0].1
        );
    }

    #[tokio::test]
    async fn dynamic_prompt_is_served_per_turn() {
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let capability = ExtensionCapability::new(hooks_package(&python), std::env::temp_dir());
        let ctx = everruns_core::capabilities::SystemPromptContext::without_file_store(
            everruns_core::typed_id::SessionId::new(),
        );
        let contribution = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("dynamic prompt");
        assert!(
            contribution.contains("dynamic echo prompt"),
            "{contribution}"
        );
    }

    /// A minimal Python YEP server whose one tool returns `value` — baked into
    /// the source and captured at process start, so the returned value only
    /// changes when the *process* is respawned (not per call). Lets the reload
    /// test distinguish "same process" from "fresh process".
    fn write_versioned_echo_server(path: &std::path::Path, value: &str) {
        let src = format!(
            r#"import json, sys
VALUE = {value:?}
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, msg_id = msg.get("method"), msg.get("id")
    if method == "initialize":
        send({{"id": msg_id, "result": {{"protocol_version": "1.0", "name": "reloadable",
             "capabilities": ["tools"], "capability_params": {{"tools": [{{"name": "version"}}]}}}}}})
    elif method == "tool/call":
        send({{"id": msg_id, "result": {{"version": VALUE}}}})
    elif msg_id is not None:
        send({{"id": msg_id, "error": {{"code": -32601, "message": "nope"}}}})
"#
        );
        std::fs::write(path, src).unwrap();
    }

    /// Live reload: after editing a running extension's server, `reload`
    /// respawns it so the next call runs the new code — without rebuilding the
    /// harness. The approved surface (the manifest) is untouched, so this is
    /// the self-writing iteration seam, not a grant change.
    #[tokio::test]
    async fn reload_respawns_the_server_with_edited_code() {
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server.py");
        write_versioned_echo_server(&server, "v1");

        let manifest = parse_manifest(
            &json!({
                "name": "reloadable",
                "description": "Reload fixture.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": {
                        "command": python,
                        "args": [server.display().to_string()]
                    },
                    "tools": [{ "name": "version", "description": "Report baked version." }]
                }
            })
            .to_string(),
        )
        .expect("reload manifest");
        let registry = super::LiveProcessRegistry::default();
        let capability = ExtensionCapability::new(
            ExtensionPackage {
                dir: tmp.path().to_path_buf(),
                manifest,
            },
            tmp.path().to_path_buf(),
        )
        .with_process_registry(registry.clone());

        let call = || async {
            let tools = capability.tools();
            let tool = tools.iter().find(|t| t.name() == "version").unwrap();
            match tool.execute(json!({})).await {
                ToolExecutionResult::Success(v) => v["version"].as_str().unwrap().to_string(),
                other => panic!("expected success, got {other:?}"),
            }
        };

        // First call spawns the server; it reports the v1 code.
        assert_eq!(call().await, "v1");

        // Edit the server on disk. The running process still holds v1.
        write_versioned_echo_server(&server, "v2");
        assert_eq!(call().await, "v1", "running server should not see the edit");

        // Reload tears the old server down; the next call respawns and runs v2.
        assert_eq!(
            registry.reload("reloadable").await,
            Some(true),
            "a live server should be reloaded"
        );
        assert_eq!(
            call().await,
            "v2",
            "reloaded server should run the edited code"
        );

        // An extension with no live process in this session can't be reloaded.
        assert_eq!(registry.reload("ghost").await, None);
    }
}

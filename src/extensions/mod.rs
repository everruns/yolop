//! Extensions: installable capability-level packages served over YEP, the
//! yolop extension protocol. See `specs/extensions.md`. Implemented:
//! the protocol core + persistent process manager + generic
//! `ExtensionCapability` adapter (phase 1); install/enable management
//! surface + lockfile (phase 2); contributed MCP servers (phase 3);
//! hook subscriptions + dynamic prompt over RPC (phase 4); the
//! `doctor_extension` conformance probe (`doctor` module). Later: `ui/ask`,
//! `workspace/changed`, `crates.io` install, providers.
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
pub(crate) mod store;

pub(crate) use capability::ExtensionCapability;
pub(crate) use manage::ExtensionsCapability;
pub(crate) use package::{discover_extensions, extension_capability_id, extensions_dir};

#[cfg(test)]
mod spawn_tests {
    //! End-to-end proof over a real child process: a minimal Python YEP
    //! server (`tests/fixtures/yep_echo_server.py`) is spawned, handshaken,
    //! and driven through a streamed tool call — the conformance shape the
    //! future `/extensions doctor` will automate.

    use super::capability::ExtensionCapability;
    use super::package::{ExtensionPackage, parse_manifest};
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
    async fn prompt_contribution_comes_from_handshake_clamped_by_manifest() {
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let capability = ExtensionCapability::new(fixture_package(&python), std::env::temp_dir());
        let ctx = everruns_core::capabilities::SystemPromptContext::without_file_store(
            everruns_core::typed_id::SessionId::new(),
        );
        let contribution = capability
            .system_prompt_contribution(&ctx)
            .await
            .expect("prompt facet");
        assert!(contribution.contains("<capability id=\"ext:echo\">"));
        assert!(contribution.contains("echo fixture prompt"));
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
}

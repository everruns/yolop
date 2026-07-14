//! Extensions: installable capability-level packages served over YEP, the
//! yolop extension protocol. Phase 1 of `specs/extensions.md`: package
//! discovery, the protocol core, the persistent process manager, and the
//! generic `ExtensionCapability` adapter. Install verbs, contributed MCP
//! servers, hooks, and the dynamic-prompt RPC are later phases.
//!
//! Registration: discovered packages are registered in the capability
//! registry (so they appear in the catalog and validate config) but are
//! NOT on the default harness — users enable one with
//! `[[capabilities]] ref = "ext:<name>"` in settings.toml, exactly like
//! the built-in `lsp`.

pub(crate) mod capability;
pub(crate) mod client;
pub(crate) mod manage;
pub(crate) mod manager;
pub(crate) mod package;
pub(crate) mod protocol;
pub(crate) mod store;

pub(crate) use capability::ExtensionCapability;
pub(crate) use manage::ExtensionsCapability;
pub(crate) use package::{discover_extensions, extensions_dir};

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
}

//! AGENTS.md / CLAUDE.md discovery as a capability.
//!
//! everruns ships this as a built-in (`agent_instructions`); agentyk has no
//! equivalent, so the backend carries its own. Deliberately minimal — the
//! workspace root only, no parent-directory chain and no user-level file —
//! because the point is to show the seam fits, not to re-implement everruns'
//! version.

use std::path::{Path, PathBuf};

use agentyk::Capability;
use agentyk::capability::SystemPromptContext;
use async_trait::async_trait;

/// Files read from the workspace root, in order. A `CLAUDE.md` that only says
/// `@AGENTS.md` is common, so both are read and deduplicated by content.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

pub struct WorkspaceInstructions {
    workspace: PathBuf,
}

impl WorkspaceInstructions {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }
}

#[async_trait]
impl Capability for WorkspaceInstructions {
    fn id(&self) -> &str {
        "agent_instructions"
    }

    fn description(&self) -> &str {
        "Workspace instructions from AGENTS.md / CLAUDE.md."
    }

    async fn system_prompt_contribution(&self, _context: &SystemPromptContext) -> Option<String> {
        let mut sections: Vec<String> = Vec::new();
        for name in INSTRUCTION_FILES {
            let Ok(contents) = tokio::fs::read_to_string(self.workspace.join(name)).await else {
                continue;
            };
            let trimmed = contents.trim().to_string();
            if trimmed.is_empty() || sections.iter().any(|section| section.contains(&trimmed)) {
                continue;
            }
            sections.push(format!("# {name}\n\n{trimmed}"));
        }
        match sections.is_empty() {
            true => None,
            false => Some(format!(
                "Workspace instructions — follow them exactly:\n\n{}",
                sections.join("\n\n")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::SessionId;

    #[tokio::test]
    async fn reads_agents_md_and_skips_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Be terse.").unwrap();
        // A pointer file whose content is already covered contributes nothing.
        std::fs::write(dir.path().join("CLAUDE.md"), "Be terse.").unwrap();
        let capability = WorkspaceInstructions::new(dir.path());
        let prompt = capability
            .system_prompt_contribution(&SystemPromptContext::new(SessionId::new()))
            .await
            .expect("instructions found");
        assert!(prompt.contains("Be terse."));
        assert_eq!(prompt.matches("Be terse.").count(), 1, "{prompt}");
    }

    #[tokio::test]
    async fn contributes_nothing_without_instruction_files() {
        let dir = tempfile::tempdir().unwrap();
        let capability = WorkspaceInstructions::new(dir.path());
        assert!(
            capability
                .system_prompt_contribution(&SystemPromptContext::new(SessionId::new()))
                .await
                .is_none()
        );
    }
}

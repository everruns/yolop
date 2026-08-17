// The `yolop` capability — framing when the user addresses yolop itself.
//
// Users say "what is your config?", "what can yolop do?", or "set yolop blue"
// when they mean the tool, not the current repo. This capability contributes a
// short system-prompt block so the model treats those requests as global yolop
// questions rather than project work (which belongs in AGENTS.md, source, tests).
//
// See knowledge/specs/yolop.md.

use async_trait::async_trait;
use everruns_core::{Capability, CapabilityStatus, SystemPromptContext};

pub(crate) const YOLOP_CAPABILITY_ID: &str = "yolop";

pub(crate) struct YolopCapability;

#[async_trait]
impl Capability for YolopCapability {
    fn id(&self) -> &str {
        YOLOP_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Yolop"
    }
    fn description(&self) -> &str {
        "Framing for requests addressed to yolop itself rather than the current project."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Personalization")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(format!(
            "<capability id=\"{}\">\nWhen the user addresses yolop itself — e.g. \"what can \
             you do?\", \"what is your config?\", \"set yolop blue\" — treat it as a global \
             request about yolop, not a change to the current project. Project-specific \
             guidance belongs in the repo's AGENTS.md instead.\n</capability>",
            self.id()
        ))
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(format!(
            "<capability id=\"{}\">\nGlobal requests about yolop itself, not the current \
             project.\n</capability>",
            self.id()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_exposes_no_tools_or_slash_commands() {
        let capability = YolopCapability;

        assert!(capability.tools().is_empty());
        assert!(capability.commands().is_empty());
    }

    #[test]
    fn system_prompt_uses_standard_capability_block() {
        let capability = YolopCapability;
        let block = capability
            .system_prompt_preview()
            .expect("preview should be present");

        assert!(block.starts_with("<capability id=\"yolop\">\n"));
        assert!(block.ends_with("</capability>"));
        assert!(!block.contains("`remember`"));
        assert!(!block.contains("`hooks`"));
    }
}

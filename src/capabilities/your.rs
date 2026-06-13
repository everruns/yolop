// The `your` capability — yolop's personalization framing.
//
// "your" is how a user addresses yolop itself: "what is your config?", "set
// yolop blue", "remember that I prefer terse answers". These are GLOBAL
// personalization requests about yolop the tool, distinct from changes to the
// current project (which belong in the repo's AGENTS.md, source, and tests).
//
// Durable, cross-session user memory lives in its own `memory` capability
// (`remember` / `recall` / `forget` — see capabilities::memory). Hook
// self-configuration lives in the dedicated `hooks` capability. `your` keeps
// only the personalization framing and routes requests to the right surface.
//
// See specs/your.md for the full vision (global skills, hooks, user-defined
// capabilities) — all of which hang off the same central config dir.

use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};

pub(crate) const YOUR_CAPABILITY_ID: &str = "your";

/// Render the `<your>` system-prompt block: personalization framing that routes
/// "remember that…" to the `memory` capability and hook requests to the `hooks`
/// capability. Pure so it is unit-testable without a `SystemPromptContext`.
fn render_your_block() -> String {
    let mut out = String::new();
    out.push_str("<your>\n");
    out.push_str(
        "yolop's personalization layer. When the user addresses \"you\" or \"yolop\" itself \
         — e.g. \"what is your config?\", \"update your settings\", \"set yolop blue\", \
         \"remember that I prefer X\" — treat it as a GLOBAL personalization request about \
         yolop, NOT a change to the current project. Persist durable user preferences and facts \
         with the `remember` tool and read them back with `recall` (the global `memory` \
         capability); project-specific guidance belongs in the repo's AGENTS.md instead. \
         Configure hook requests such as \"yolop setup a hook to prevent calls to git\" with \
         the `hooks` capability tools (`validate_hook`, `upsert_hook`), not by storing a \
         memory note.\n",
    );
    out.push_str("</your>");
    out
}

// ---------- capability ----------

pub(crate) struct YourCapability;

#[async_trait]
impl Capability for YourCapability {
    fn id(&self) -> &str {
        YOUR_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Your (personalization)"
    }
    fn description(&self) -> &str {
        "Global yolop personalization framing. Routes durable user memory to the `memory` \
         capability and hook setup requests to the `hooks` capability."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Personalization")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        Some(render_your_block())
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "\
<your>
yolop's personalization layer (global). Persist durable user preferences with `remember` /
`recall` (the memory capability); configure hooks with the `hooks` capability tools.
</your>"
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_exposes_no_hook_tools_or_slash_commands() {
        let capability = YourCapability;

        assert!(capability.tools().is_empty());
        assert!(capability.commands().is_empty());
    }

    #[test]
    fn your_block_frames_personalization_and_routes_memory_and_hooks() {
        let block = render_your_block();
        assert!(block.starts_with("<your>\n"));
        assert!(block.ends_with("</your>"));
        // Routes durable memory to the memory capability tools, not a local note.
        assert!(block.contains("`remember`"));
        assert!(block.contains("`recall`"));
        // Routes hook self-configuration to the dedicated capability.
        assert!(block.contains("`hooks` capability"));
        assert!(block.contains("upsert_hook"));
    }
}

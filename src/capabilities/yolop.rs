//! The `yolop` capability: self-address framing plus attached administration.
//!
//! The framing teaches the model when a request is about yolop itself (a global
//! request about the tool) rather than a change to the current project. The
//! administration teaches how to act on the live session: run
//! `yolop <subcommand> ...` in the foreground bash tool, with the route list
//! derived from the routes actually registered. Administration is deliberately
//! not a set of model tools (their schemas would cost context every turn).

use crate::control::ControlRoute;
use everruns_core::{Capability, CapabilityStatus};

pub(crate) const YOLOP_CAPABILITY_ID: &str = "yolop";

/// Framing for requests about yolop itself. Kept separate from the dynamic
/// administration below so the stable part stays greppable in tests.
const FRAMING: &str = "When the user addresses yolop itself, e.g. \"what can you do?\", \"what is your config?\", \"set yolop blue\", treat it as a global request about yolop, not a change to the current project. Project-specific guidance belongs in the repo's AGENTS.md instead.";

/// The single `yolop` prompt block: framing always, administration only when
/// routes are registered.
pub(crate) struct YolopCapability {
    prompt: String,
}

impl YolopCapability {
    pub(crate) fn new(routes: &[ControlRoute]) -> Self {
        let mut prompt = String::from(FRAMING);
        if !routes.is_empty() {
            prompt.push_str("\n\nAdminister this session by running `yolop <subcommand> ...` in the foreground bash tool: it attaches to the running session and takes effect live. Run `yolop <subcommand> --help` for the operations; there are no equivalent tools.\n");
            for route in routes {
                prompt.push_str(&format!(
                    "- `{sub}`, {summary}\n",
                    sub = route.cli_subcommand,
                    summary = route.summary
                ));
            }
            prompt.push_str(
                "Invoke directly: shell composition loses attachment. Do not repeat `--help`.",
            );
        }
        Self { prompt }
    }
}

impl Capability for YolopCapability {
    fn id(&self) -> &'static str {
        YOLOP_CAPABILITY_ID
    }

    fn name(&self) -> &'static str {
        "Yolop"
    }

    fn description(&self) -> &'static str {
        "Global framing for requests about yolop itself plus attached administration of the live session."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Personalization")
    }

    fn system_prompt_addition(&self) -> Option<&str> {
        Some(&self.prompt)
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(format!(
            "<capability id=\"{id}\">Global requests about yolop itself, not the project.</capability>",
            id = YOLOP_CAPABILITY_ID
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::model_list::MODEL_LIST_CONTROL_ROUTE;
    use crate::capabilities::session_coordination::COORDINATION_CONTROL_ROUTE;
    use crate::extensions::EXTENSIONS_CONTROL_ROUTE;

    #[test]
    fn exposes_no_tools() {
        let capability = YolopCapability::new(&[]);
        assert!(capability.tools().is_empty());
    }

    #[test]
    fn id_is_yolop() {
        let capability = YolopCapability::new(&[]);
        assert_eq!(capability.id(), "yolop");
    }

    #[test]
    fn framing_is_always_present() {
        let capability = YolopCapability::new(&[]);
        let addition = capability
            .system_prompt_addition()
            .expect("framing always contributes");
        assert!(addition.contains("global request about yolop"));
    }

    #[test]
    fn framing_alone_has_no_administration() {
        let capability = YolopCapability::new(&[]);
        let block = capability.system_prompt_addition().expect("block");
        assert!(!block.contains("Administer this session"));
    }

    #[test]
    fn administration_uses_registered_routes() {
        let capability =
            YolopCapability::new(&[COORDINATION_CONTROL_ROUTE, EXTENSIONS_CONTROL_ROUTE]);
        let block = capability.system_prompt_addition().expect("block");
        assert!(block.contains("Administer this session"));
        assert!(block.contains("global request about yolop"));
        for route in [COORDINATION_CONTROL_ROUTE, EXTENSIONS_CONTROL_ROUTE] {
            assert!(
                block.contains(route.cli_subcommand),
                "missing route {}",
                route.cli_subcommand
            );
            assert!(block.contains(route.summary));
        }
    }

    #[test]
    fn administration_discourages_repeated_help_probes() {
        let capability = YolopCapability::new(&[COORDINATION_CONTROL_ROUTE]);
        let block = capability.system_prompt_addition().expect("block");
        assert!(block.contains("Do not repeat `--help`"));
    }

    #[test]
    fn administration_advertises_config_settings() {
        let capability = YolopCapability::new(&[MODEL_LIST_CONTROL_ROUTE]);
        let block = capability.system_prompt_addition().expect("block");
        assert!(block.contains("`config`"));
        assert!(
            block.contains("persistent settings"),
            "config route should advertise settings, not only models"
        );
    }
}

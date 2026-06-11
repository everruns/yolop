// Optional-capability catalog — the single source of truth for which agent
// capabilities can be toggled per user, what each one does, and whether it is
// on by default. See `specs/capabilities.md` for the selection rationale
// (including upstream capabilities deliberately left out).
//
// Toggles persist in settings.toml under `[capabilities]` (bool per name) and
// are read once at runtime build, so a change applies on the next run. The
// catalog names are user-facing (`web_search`), not necessarily the upstream
// capability id (`duckduckgo`); `coding_harness_capabilities` owns that
// mapping because some entries also carry a capability config.

use crate::settings::Settings;

pub(crate) struct OptionalCapabilitySpec {
    /// Settings key under `[capabilities]` and the name users address in
    /// `set_config key=capabilities.<name>`.
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Effective state when the settings file has no entry for this name.
    pub default_enabled: bool,
}

/// Every toggleable capability, in user-facing display order.
pub(crate) const OPTIONAL_CAPABILITIES: &[OptionalCapabilitySpec] = &[
    OptionalCapabilitySpec {
        name: "web_search",
        title: "Web search",
        description: "Free DuckDuckGo web search (`duckduckgo_search` tool); no API key needed. \
                      Turn off for fully offline or no-network sessions.",
        default_enabled: true,
    },
    OptionalCapabilitySpec {
        name: "web_fetch",
        title: "Web fetch",
        description: "Fetch URLs into the conversation and download files into the workspace \
                      (`web_fetch` tool). Turn off for fully offline or no-network sessions.",
        default_enabled: true,
    },
    OptionalCapabilitySpec {
        name: "tool_search",
        title: "Deferred tool loading",
        description: "Defers long-tail tool schemas behind a `tool_search` tool to keep the \
                      system prompt small. Turn off to always send every tool schema in full.",
        default_enabled: true,
    },
    OptionalCapabilitySpec {
        name: "session_storage",
        title: "Session storage",
        description: "Session-scoped `kv_store` and `secret_store` tools for scratch state \
                      during long tasks. In-memory: contents do not survive a restart.",
        default_enabled: false,
    },
];

pub(crate) fn find(name: &str) -> Option<&'static OptionalCapabilitySpec> {
    OPTIONAL_CAPABILITIES.iter().find(|spec| spec.name == name)
}

/// Effective state for a catalog name: the persisted toggle if present,
/// otherwise the catalog default. Unknown names (never produced by the
/// schema-validated config surface) read as disabled.
pub(crate) fn enabled(settings: &Settings, name: &str) -> bool {
    match find(name) {
        Some(spec) => settings
            .capability_enabled(name)
            .unwrap_or(spec.default_enabled),
        None => false,
    }
}

/// Comma-separated catalog names, for error messages.
pub(crate) fn known_names() -> String {
    OPTIONAL_CAPABILITIES
        .iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_no_toggle_is_stored() {
        let settings = Settings::default();
        assert!(enabled(&settings, "web_search"));
        assert!(enabled(&settings, "web_fetch"));
        assert!(enabled(&settings, "tool_search"));
        assert!(!enabled(&settings, "session_storage"));
    }

    #[test]
    fn stored_toggle_overrides_default_in_both_directions() {
        let mut settings = Settings::default();
        settings
            .capabilities
            .insert("web_search".to_string(), false);
        settings
            .capabilities
            .insert("session_storage".to_string(), true);
        assert!(!enabled(&settings, "web_search"));
        assert!(enabled(&settings, "session_storage"));
    }

    #[test]
    fn unknown_name_reads_disabled() {
        let mut settings = Settings::default();
        // Tolerant loading can leave entries the catalog does not know.
        settings.capabilities.insert("frobnicate".to_string(), true);
        assert!(!enabled(&settings, "frobnicate"));
        assert!(find("frobnicate").is_none());
    }
}

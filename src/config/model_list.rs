//! The user's model list: the ordered, cross-provider menu yolop offers when
//! you switch models.
//!
//! Stored as `[[models]]` in `settings.toml`, one entry per line of the menu:
//!
//! ```toml
//! [[models]]
//! provider = "openai"
//! model = "gpt-5.6-sol"
//! effort = "high"
//! ```
//!
//! An entry names a provider *and* a model, so selecting one switches both:
//! `anthropic/claude-opus-4-8` and `openrouter/anthropic/claude-opus-4-8` are
//! different entries reaching the same weights through different accounts.
//!
//! This is deliberately not "favourites" layered over a full catalog. It *is*
//! the list: `/model`, the status bar, and ACP's model options all serve it,
//! and provider discovery (`model_discovery`) is the browse-everything path
//! behind it rather than the default view. Providers publish hundreds of
//! models; a user switches between a handful.
//!
//! An empty list means "never configured" and resolves to [`default_models`],
//! so a fresh install has a working menu.

use crate::runtime::SUPPORTED_PROVIDERS;
use toml::Table;
use toml::Value as TomlValue;

/// One line of the model menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    /// Provider name, one of [`SUPPORTED_PROVIDERS`].
    pub provider: String,
    /// Provider-relative model id (`gpt-5.6-sol`, `anthropic/claude-opus-4-8`
    /// on OpenRouter). Never carries the reasoning effort; that is `effort`.
    pub model: String,
    /// Reasoning effort to apply with this entry. `None` leaves the model
    /// profile's own default in place.
    pub effort: Option<String>,
    /// Display override. `None` renders as `provider/model`.
    pub label: Option<String>,
}

impl ModelEntry {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            effort: None,
            label: None,
        }
    }

    #[cfg(test)]
    pub fn with_effort(mut self, effort: &str) -> Self {
        self.effort = Some(effort.to_string());
        self
    }

    /// Stable identity of an entry: what ACP sends back as the selected value
    /// and what `yolop config models rm` matches on. Effort is excluded so the same
    /// model at two efforts does not read as two different models here; use
    /// [`ModelEntry::spec`] where effort matters.
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }

    /// The provider-relative `model [effort]` form `/setup model` accepts and
    /// `ProviderChoice::resolve_model_spec` parses.
    pub fn spec(&self) -> String {
        match &self.effort {
            Some(effort) => format!("{} {}", self.model, effort),
            None => self.model.clone(),
        }
    }

    /// One-line rendering for pickers and `yolop config models list`.
    pub fn display(&self) -> String {
        match &self.label {
            Some(label) => label.clone(),
            None => match &self.effort {
                Some(effort) => format!("{}/{} {effort}", self.provider, self.model),
                None => format!("{}/{}", self.provider, self.model),
            },
        }
    }
}

/// The menu a user who has never configured one gets.
///
/// Small on purpose: the current frontier line, reachable through either
/// account that serves it, without reproducing the catalog the list exists to
/// avoid. Entries whose provider has no credential are hidden at display time,
/// so listing several providers here costs nothing to a user who has one.
pub fn default_models() -> Vec<ModelEntry> {
    vec![
        ModelEntry::new("openai", "gpt-5.6-sol"),
        ModelEntry::new("openai", "gpt-5.6-terra"),
        ModelEntry::new("openai", "gpt-5.6-luna"),
        ModelEntry::new("codex", "gpt-5.6-sol"),
        ModelEntry::new("codex", "gpt-5.6-terra"),
        ModelEntry::new("codex", "gpt-5.6-luna"),
    ]
}

/// Parse `[[models]]` from a settings table.
///
/// Tolerant like the rest of settings loading: an entry missing `provider` or
/// `model`, or naming a provider yolop does not support, is skipped rather than
/// failing the load. `models` written as a *table* is the pre-list layout (a
/// per-provider default map, now `[default_models]`) and yields no entries; see
/// `Settings::from_table`.
pub fn parse_models_list(table: &Table) -> Vec<ModelEntry> {
    let Some(TomlValue::Array(items)) = table.get("models") else {
        return Vec::new();
    };
    items.iter().filter_map(parse_entry).collect()
}

fn parse_entry(value: &TomlValue) -> Option<ModelEntry> {
    let entry = value.as_table()?;
    let provider = entry.get("provider").and_then(TomlValue::as_str)?.trim();
    let model = entry.get("model").and_then(TomlValue::as_str)?.trim();
    if model.is_empty() || !SUPPORTED_PROVIDERS.contains(&provider) {
        return None;
    }
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(TomlValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Some(ModelEntry {
        provider: provider.to_string(),
        model: model.to_string(),
        effort: text("effort"),
        label: text("label"),
    })
}

/// Render the list back to `[[models]]` array-of-tables form.
pub fn models_to_toml(models: &[ModelEntry]) -> Vec<TomlValue> {
    models
        .iter()
        .map(|entry| {
            let mut table = Table::new();
            table.insert(
                "provider".to_string(),
                TomlValue::String(entry.provider.clone()),
            );
            table.insert("model".to_string(), TomlValue::String(entry.model.clone()));
            if let Some(effort) = &entry.effort {
                table.insert("effort".to_string(), TomlValue::String(effort.clone()));
            }
            if let Some(label) = &entry.label {
                table.insert("label".to_string(), TomlValue::String(label.clone()));
            }
            TomlValue::Table(table)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Vec<ModelEntry> {
        parse_models_list(&toml.parse::<Table>().expect("valid toml"))
    }

    #[test]
    fn default_list_leads_with_gpt_5_6_sol() {
        let defaults = default_models();
        assert_eq!(defaults[0], ModelEntry::new("openai", "gpt-5.6-sol"));
        for entry in &defaults {
            assert!(
                SUPPORTED_PROVIDERS.contains(&entry.provider.as_str()),
                "default entry names an unsupported provider: {entry:?}"
            );
        }
    }

    #[test]
    fn entries_round_trip_through_toml() {
        let models = vec![
            ModelEntry::new("openai", "gpt-5.6-sol").with_effort("high"),
            ModelEntry {
                provider: "openrouter".into(),
                model: "anthropic/claude-opus-4-8".into(),
                effort: None,
                label: Some("Opus (via OpenRouter)".into()),
            },
        ];
        let mut table = Table::new();
        table.insert(
            "models".to_string(),
            TomlValue::Array(models_to_toml(&models)),
        );
        assert_eq!(parse_models_list(&table), models);
    }

    #[test]
    fn unusable_entries_are_skipped_rather_than_failing_the_load() {
        let parsed = parse(
            r#"
            [[models]]
            provider = "openai"
            model = "gpt-5.6-sol"

            [[models]]
            provider = "not-a-provider"
            model = "whatever"

            [[models]]
            provider = "openai"
            model = ""

            [[models]]
            model = "no-provider-key"
            "#,
        );
        assert_eq!(parsed, vec![ModelEntry::new("openai", "gpt-5.6-sol")]);
    }

    #[test]
    fn the_legacy_per_provider_table_yields_no_entries() {
        // Pre-list settings files wrote `[models]` as a provider -> spec map.
        // `Settings::from_table` reroutes that shape to `default_models`; here
        // it must simply not parse as a list.
        assert!(parse("[models]\nopenai = 'gpt-5.5 high'\n").is_empty());
    }

    #[test]
    fn spec_and_key_separate_effort_from_identity() {
        let entry = ModelEntry::new("openai", "gpt-5.6-sol").with_effort("high");
        assert_eq!(entry.key(), "openai:gpt-5.6-sol");
        assert_eq!(entry.spec(), "gpt-5.6-sol high");
        assert_eq!(entry.display(), "openai/gpt-5.6-sol high");
    }
}

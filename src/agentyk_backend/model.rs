//! Provider resolution → [`agentyk::ModelSpec`].
//!
//! yolop already knows how to turn flags, env, and settings into an
//! `everruns_core::ResolvedModel`; that resolution is reused wholesale and
//! only the last hop — into agentyk's by-value model — happens here. The
//! mapping is where the two provider vocabularies stop lining up: agentyk
//! ships two protocols (`openai`, `anthropic`) plus the offline `llmsim`, so
//! anything yolop serves through a bespoke driver (Codex) has no home yet.

use agentyk::{DriverId, ModelSpec};
use anyhow::{Result, anyhow};

use crate::config::Settings;
use crate::runtime::ProviderChoice;

pub fn resolve(provider: &ProviderChoice, settings: &Settings) -> Result<ModelSpec> {
    // Driver first, credentials second: an unsupported provider should say so
    // instead of failing on a missing key it would never have used.
    let driver = match provider {
        ProviderChoice::Sim => return Ok(ModelSpec::llmsim()),
        ProviderChoice::Anthropic { .. } => DriverId::anthropic(),
        // Everything else yolop supports is an OpenAI-shaped endpoint. The
        // OpenRouter driver is a separate crate on the everruns side; agentyk
        // has no equivalent, and its Chat Completions driver reaches the same
        // gateway with an explicit base URL.
        ProviderChoice::OpenAi { .. }
        | ProviderChoice::Google { .. }
        | ProviderChoice::OpenRouter { .. }
        | ProviderChoice::Ollama { .. }
        | ProviderChoice::Custom { .. } => DriverId::openai(),
        ProviderChoice::Codex { .. } => {
            return Err(anyhow!(
                "the agentyk backend has no driver for `codex` — agentyk ships \
                 `openai`, `anthropic`, and `llmsim` only (see \
                 knowledge/specs/agentyk-backend.md)"
            ));
        }
    };

    let resolved = provider.model_with_provider(settings)?;
    let mut spec = ModelSpec::new(driver, resolved.model.clone());
    if let Some(key) = resolved.api_key.clone() {
        spec = spec.api_key(key);
    }
    if let Some(base_url) = resolved.base_url.clone() {
        spec = spec.base_url(base_url);
    }
    if let Some(effort) = provider.reasoning_effort() {
        spec = spec.reasoning_effort(effort);
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_needs_no_credentials() {
        let spec = resolve(&ProviderChoice::Sim, &Settings::default()).expect("llmsim resolves");
        assert_eq!(spec.driver.as_str(), "llmsim");
    }

    #[test]
    fn anthropic_carries_key_and_effort() {
        // A stored token so the test passes whether or not the ambient
        // environment has a real key (env wins, which is fine — either way a
        // credential must reach the spec).
        let mut settings = Settings::default();
        settings.tokens.insert("anthropic".into(), "key-123".into());
        let choice = ProviderChoice::Anthropic {
            model: "claude-sonnet-4-5".into(),
            reasoning_effort: Some("high".into()),
        };
        let spec = resolve(&choice, &settings).expect("anthropic resolves");
        assert_eq!(spec.driver.as_str(), "anthropic");
        assert_eq!(spec.model, "claude-sonnet-4-5");
        assert!(spec.api_key.is_some());
        assert_eq!(
            spec.reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort.as_deref()),
            Some("high")
        );
    }

    #[test]
    fn codex_is_reported_as_unsupported() {
        let mut settings = Settings::default();
        settings.tokens.insert("openai".into(), "unused".into());
        let choice = ProviderChoice::Codex {
            model: "gpt-5-codex".into(),
            reasoning_effort: None,
        };
        let error = resolve(&choice, &settings).expect_err("codex has no agentyk driver");
        assert!(error.to_string().contains("no driver"), "{error}");
    }
}

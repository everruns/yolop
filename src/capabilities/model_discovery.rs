// Provider model discovery.
//
// Queries the provider's models API through the everruns drivers
// (`LlmDriver::list_models`), falling back to a direct OpenAI-compatible
// `GET <base>/models` for custom endpoints the drivers decline (Ollama,
// Gemini's OpenAI surface, proxies). Discovered models are enriched with
// the everruns-core model profile registry so the UI can show human-readable
// names and descriptions even when the provider's API returns bare ids.

use crate::runtime::ProviderChoice;
use crate::settings::Settings;
use anyhow::{Context, Result, anyhow};
use everruns_core::get_model_profile;
use everruns_core::llm_driver_registry::{
    DiscoveredModel, DriverRegistry, ProviderConfig, ProviderType,
};
use everruns_core::llm_models::LlmProviderType;

/// One model offered by a provider, ready for display: bare id plus
/// human-readable metadata merged from the provider's API response and the
/// everruns-core profile registry.
#[derive(Clone, Debug)]
pub(crate) struct DiscoveredProviderModel {
    pub model_id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Query the provider's models API for the given choice. Returns `Ok(None)`
/// when the provider (or its custom endpoint) does not support model
/// listing; callers should fall back to curated suggestions in that case.
pub(crate) async fn discover_provider_models(
    choice: &ProviderChoice,
    settings: &Settings,
) -> Result<Option<Vec<DiscoveredProviderModel>>> {
    if matches!(choice, ProviderChoice::Sim) {
        return Ok(None);
    }
    let target = choice.model_with_provider(settings)?;
    let provider_type = match target.provider_type {
        LlmProviderType::Openai => ProviderType::OpenAI,
        LlmProviderType::AzureOpenai => ProviderType::AzureOpenAI,
        LlmProviderType::OpenaiCompletions => ProviderType::OpenAICompletions,
        LlmProviderType::Anthropic => ProviderType::Anthropic,
        LlmProviderType::Gemini => ProviderType::Gemini,
        LlmProviderType::LlmSim => return Ok(None),
    };
    let mut config = ProviderConfig::new(provider_type);
    if let Some(key) = &target.api_key {
        config = config.with_api_key(key);
    }
    if let Some(base_url) = &target.base_url {
        config = config.with_base_url(base_url);
    }

    let mut registry = DriverRegistry::new();
    everruns_anthropic::register_driver(&mut registry);
    everruns_openai::register_driver(&mut registry);
    let driver = registry.create_driver(&config)?;

    let models = match driver.list_models().await? {
        Some(models) => Some(models),
        // The everruns drivers decline discovery for unrecognized custom
        // endpoints (Ollama, Gemini's OpenAI-compatible surface, custom
        // OpenRouter proxies). Those endpoints still expose the
        // OpenAI-compatible `GET <base>/models`, so query it directly.
        None => match &target.base_url {
            Some(base_url) => {
                list_openai_compatible_models(base_url, target.api_key.as_deref()).await?
            }
            None => None,
        },
    };
    let Some(mut models) = models else {
        return Ok(None);
    };

    for model in models.iter_mut() {
        // Gemini's OpenAI-compatible surface reports ids as `models/<id>`;
        // the bare id is what chat calls (and profile lookups) expect.
        if let Some(bare) = model.model_id.strip_prefix("models/") {
            model.model_id = bare.to_string();
        }
    }
    models.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.model_id.cmp(&b.model_id))
    });
    Ok(Some(enrich_with_profiles(&target.provider_type, models)))
}

/// Merge each discovered model with metadata from the everruns-core model
/// profile registry. The core profile wins for descriptions (curated, short);
/// the provider's API response wins for display names (it knows its own
/// catalog best — e.g. OpenRouter's `name` field), with the core profile
/// filling the gap for APIs that return bare ids (e.g. OpenAI).
fn enrich_with_profiles(
    provider_type: &LlmProviderType,
    models: Vec<DiscoveredModel>,
) -> Vec<DiscoveredProviderModel> {
    models
        .into_iter()
        .map(|model| {
            let core_profile = get_model_profile(provider_type, &model.model_id);
            let api_profile = model.discovered_profile;
            let display_name = model
                .display_name
                .filter(|name| !name.is_empty() && *name != model.model_id)
                .or_else(|| core_profile.as_ref().map(|profile| profile.name.clone()));
            let description = core_profile
                .as_ref()
                .and_then(|profile| profile.description.clone())
                .or_else(|| {
                    api_profile
                        .as_ref()
                        .and_then(|profile| profile.description.clone())
                });
            DiscoveredProviderModel {
                model_id: model.model_id,
                display_name,
                description,
            }
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct OpenAiCompatibleModelsResponse {
    data: Vec<OpenAiCompatibleModel>,
}

#[derive(serde::Deserialize)]
struct OpenAiCompatibleModel {
    id: String,
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    owned_by: Option<String>,
}

/// Discovery fallback for OpenAI-compatible endpoints the everruns drivers
/// don't recognize: `GET <base>/models` with bearer auth.
async fn list_openai_compatible_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Option<Vec<DiscoveredModel>>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = reqwest::Client::new().get(&url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("fetch models from {url}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "models API at {url} returned {}",
            response.status()
        ));
    }
    let parsed: OpenAiCompatibleModelsResponse = response
        .json()
        .await
        .with_context(|| format!("parse models response from {url}"))?;
    let models = parsed
        .data
        .into_iter()
        .map(|model| DiscoveredModel {
            created_at: model
                .created
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0)),
            display_name: None,
            owned_by: model.owned_by,
            model_id: model.id,
            discovered_profile: None,
        })
        .collect();
    Ok(Some(models))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_discovered(model_id: &str) -> DiscoveredModel {
        DiscoveredModel {
            model_id: model_id.to_string(),
            display_name: None,
            created_at: None,
            owned_by: None,
            discovered_profile: None,
        }
    }

    #[tokio::test]
    async fn discovery_is_unsupported_for_llmsim() {
        // The offline simulator has no models API; discovery must signal
        // "unsupported" (not error) so callers keep their curated lists.
        let result = discover_provider_models(&ProviderChoice::Sim, &Settings::default())
            .await
            .expect("llmsim discovery should not error");
        assert!(result.is_none());
    }

    #[test]
    fn enrichment_fills_names_and_descriptions_from_core_profiles() {
        // OpenAI's models API returns bare ids; the core profile registry
        // supplies the human-readable name and description.
        let enriched =
            enrich_with_profiles(&LlmProviderType::Openai, vec![bare_discovered("gpt-5.5")]);

        assert_eq!(enriched.len(), 1);
        assert_eq!(enriched[0].model_id, "gpt-5.5");
        assert_eq!(enriched[0].display_name.as_deref(), Some("GPT-5.5"));
        assert!(
            enriched[0]
                .description
                .as_deref()
                .is_some_and(|description| description.contains("flagship")),
            "core profile description should be carried over: {:?}",
            enriched[0].description
        );
    }

    #[test]
    fn enrichment_prefers_api_display_name_over_core_profile() {
        let mut model = bare_discovered("gpt-5.5");
        model.display_name = Some("GPT-5.5 (via gateway)".to_string());

        let enriched = enrich_with_profiles(&LlmProviderType::Openai, vec![model]);

        assert_eq!(
            enriched[0].display_name.as_deref(),
            Some("GPT-5.5 (via gateway)")
        );
    }

    #[test]
    fn enrichment_keeps_unknown_models_with_bare_ids() {
        let enriched = enrich_with_profiles(
            &LlmProviderType::Openai,
            vec![bare_discovered("totally-new-model")],
        );

        assert_eq!(enriched[0].model_id, "totally-new-model");
        assert!(enriched[0].display_name.is_none());
        assert!(enriched[0].description.is_none());
    }

    /// Drivers decline listing for unrecognized custom endpoints (here: a
    /// localhost "Ollama"); discovery must then query the OpenAI-compatible
    /// `GET <base>/models` itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discovery_falls_back_to_openai_compatible_endpoint() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"object":"list","data":[
                {"id":"llama3.2:latest","object":"model","created":1700000000,"owned_by":"library"},
                {"id":"models/qwen3","object":"model"}
            ]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        let provider = ProviderChoice::Ollama {
            model: "llama3.2".to_string(),
            base_url: format!("http://{addr}/v1"),
        };
        let models = discover_provider_models(&provider, &Settings::default())
            .await
            .expect("fallback discovery should succeed")
            .expect("openai-compatible endpoint lists models");
        server.join().expect("mock server thread");

        let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
        assert!(ids.contains(&"llama3.2:latest"), "ids: {ids:?}");
        // Gemini-style `models/` prefixes are normalized to bare ids.
        assert!(ids.contains(&"qwen3"), "ids: {ids:?}");
    }

    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY; performs a live models API call"]
    async fn discovery_openrouter_live() {
        if std::env::var("OPENROUTER_API_KEY")
            .map(|v| v.is_empty())
            .unwrap_or(true)
        {
            eprintln!("skipping: OPENROUTER_API_KEY not set");
            return;
        }
        let provider = ProviderChoice::default_for_provider_name("openrouter").unwrap();
        let models = discover_provider_models(&provider, &Settings::default())
            .await
            .expect("openrouter discovery should succeed")
            .expect("openrouter supports model listing");
        assert!(!models.is_empty(), "openrouter should report models");
        assert!(
            models.iter().all(|m| !m.model_id.is_empty()),
            "every discovered model needs an id"
        );
    }
}

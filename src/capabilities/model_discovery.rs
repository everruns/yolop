// Provider model discovery.
//
// Queries the provider's models API through the everruns drivers
// (`LlmDriver::list_models`), falling back to a direct OpenAI-compatible
// `GET <base>/models` for custom endpoints the drivers decline (Ollama,
// Gemini's OpenAI surface, proxies). Discovered models are enriched with
// the everruns-core model profile registry so the UI can show human-readable
// names and descriptions even when the provider's API returns bare ids.

use crate::config::Settings;
use crate::runtime::{ProviderChoice, SUPPORTED_PROVIDERS};
use anyhow::{Context, Result, anyhow};
use everruns_core::DriverId;
use everruns_core::driver_registry::{DiscoveredModel, DriverRegistry, ProviderConfig};
use everruns_core::get_model_profile;
use std::collections::HashSet;

/// One model offered by a provider, ready for display: bare id plus
/// human-readable metadata merged from the provider's API response and the
/// everruns-core profile registry.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    if matches!(choice, ProviderChoice::Sim | ProviderChoice::Codex { .. }) {
        return Ok(None);
    }
    let target = choice.model_with_provider(settings)?;
    // Yolop never registers Bedrock (no Bedrock driver below) or the llmsim
    // discovery driver; treat discovery as unsupported rather than erroring.
    if matches!(target.provider_type, DriverId::Bedrock | DriverId::LlmSim) {
        return Ok(None);
    }
    let mut config = ProviderConfig::new(target.provider_type.clone());
    if let Some(key) = &target.api_key {
        config = config.with_api_key(key);
    }
    if let Some(base_url) = &target.base_url {
        config = config.with_base_url(base_url);
    }

    let mut registry = DriverRegistry::new();
    everruns_anthropic::register_driver(&mut registry);
    everruns_openai::register_driver(&mut registry);
    everruns_openrouter::register_driver(&mut registry);
    let driver = registry.create_chat_driver(&config)?;

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
    let mut models = retain_chat_models(models);
    models.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.model_id.cmp(&b.model_id))
    });
    Ok(Some(enrich_with_profiles(&target.provider_type, models)))
}

/// Keep only models yolop can actually chat with.
///
/// Since 0.17.24 the drivers no longer filter discovery down to chat models —
/// embedding models are discoverable so they can be configured separately in
/// hosted Everruns. Yolop only ever selects a chat model, so anything that
/// declares capabilities without `chat` is dropped here. An empty capability
/// list means "not reported" (the OpenAI-compatible fallback below, Ollama,
/// proxies) and is kept, so unknown endpoints degrade open rather than
/// presenting an empty picker.
fn retain_chat_models(models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    models
        .into_iter()
        .filter(|model| {
            model.capabilities.is_empty()
                || model
                    .capabilities
                    .iter()
                    .any(|capability| capability == "chat")
        })
        .collect()
}

const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn bare_model_id_from_spec(spec: &str) -> &str {
    spec.split_whitespace().next().unwrap_or(spec)
}

/// When the provider exposes a models catalog, verify the resolved model id
/// is still offered. Falls back to the provider default, a curated pick, or
/// the newest discovered model, and returns a human-readable note.
pub(crate) async fn reconcile_provider_with_catalog(
    choice: ProviderChoice,
    settings: &Settings,
) -> (ProviderChoice, Vec<String>) {
    let discovered = tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        discover_provider_models(&choice, settings),
    )
    .await;

    let Ok(Ok(Some(models))) = discovered else {
        return (choice, vec![]);
    };
    if models.is_empty() {
        return (choice, vec![]);
    }

    let ids: HashSet<String> = models.iter().map(|m| m.model_id.clone()).collect();
    let model_id = choice.model_id().to_string();
    if ids.contains(&model_id) {
        return (choice, vec![]);
    }

    let fallback = pick_catalog_fallback(&choice, &ids, &models);
    let note = format!(
        "model \"{model_id}\" not available on {}; using {} instead",
        choice.provider_name(),
        fallback.label()
    );
    (fallback, vec![note])
}

fn pick_catalog_fallback(
    choice: &ProviderChoice,
    ids: &HashSet<String>,
    models: &[DiscoveredProviderModel],
) -> ProviderChoice {
    let provider = choice.provider_name();
    let default =
        ProviderChoice::default_for_provider_name(provider).unwrap_or_else(|_| choice.clone());

    if ids.contains(default.model_id()) {
        return default;
    }

    for suggestion in ProviderChoice::model_suggestions_for_provider(provider) {
        let bare = bare_model_id_from_spec(suggestion);
        if ids.contains(bare)
            && let Ok(resolved) = default.resolve_model_spec(suggestion)
        {
            return resolved;
        }
    }

    if let Some(first) = models.first()
        && let Ok(resolved) = default.resolve_model_spec(&first.model_id)
    {
        return resolved;
    }

    default
}

/// Merge each discovered model with metadata from the everruns-core model
/// profile registry. The core profile wins for descriptions (curated, short);
/// the provider's API response wins for display names (it knows its own
/// catalog best — e.g. OpenRouter's `name` field), with the core profile
/// filling the gap for APIs that return bare ids (e.g. OpenAI).
fn enrich_with_profiles(
    provider_type: &DriverId,
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
            // The bare OpenAI-compatible `/models` shape reports no service
            // capabilities; leave it empty so `retain_chat_models` keeps them.
            capabilities: Vec::new(),
            discovered_profile: None,
        })
        .collect();
    Ok(Some(models))
}

pub(crate) fn provider_is_usable(settings: &Settings, provider: &str) -> bool {
    match provider {
        "llmsim" => true,
        "ollama" => provider_env_present("ollama") || settings.has_token("ollama"),
        "custom" => crate::runtime::custom_base_url(settings).is_some(),
        "codex" => provider_env_present("codex") || settings.has_codex_auth(),
        _ => provider_env_present(provider) || settings.has_token(provider),
    }
}

fn provider_env_present(provider: &str) -> bool {
    let names: &[&str] = match provider {
        "openai" => &["OPENAI_API_KEY"],
        "codex" => &["CODEX_ACCESS_TOKEN"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "ollama" => &["OLLAMA_BASE_URL", "OLLAMA_API_KEY"],
        "custom" => &["CUSTOM_API_KEY"],
        _ => &[],
    };
    names.iter().any(|name| {
        std::env::var(name)
            .map(|value| !value.is_empty())
            .unwrap_or(false)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSearchMatch {
    pub(crate) provider: String,
    pub(crate) model_id: String,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ModelSearchResult {
    pub(crate) matches: Vec<ModelSearchMatch>,
    pub(crate) providers_searched: Vec<String>,
    pub(crate) provider_errors: Vec<String>,
}

/// Search driver-provided model catalogs across every currently usable provider.
/// A provider failure is isolated so successful catalogs still contribute results.
pub(crate) async fn search_configured_models(
    settings: &Settings,
    query: &str,
) -> ModelSearchResult {
    let needle = query.trim().to_lowercase();
    let mut result = ModelSearchResult::default();
    if needle.is_empty() {
        return result;
    }
    for provider_name in SUPPORTED_PROVIDERS {
        if !provider_is_usable(settings, provider_name) {
            continue;
        }
        let choice = match ProviderChoice::default_for_provider_name(provider_name) {
            Ok(choice) => choice,
            Err(error) => {
                result
                    .provider_errors
                    .push(format!("{provider_name}: {error}"));
                continue;
            }
        };
        result.providers_searched.push((*provider_name).to_string());
        match discover_provider_models(&choice, settings).await {
            Ok(Some(models)) => result
                .matches
                .extend(models.into_iter().filter_map(|model| {
                    let display_name = model.display_name.clone();
                    let display = display_name.as_deref().unwrap_or("");
                    (model.model_id.to_lowercase().contains(&needle)
                        || display.to_lowercase().contains(&needle))
                    .then(|| ModelSearchMatch {
                        provider: (*provider_name).to_string(),
                        model_id: model.model_id,
                        display_name,
                    })
                })),
            Ok(None) => {}
            Err(error) => result
                .provider_errors
                .push(format!("{provider_name}: {error}")),
        }
    }
    result
        .matches
        .sort_by(|a, b| (&a.provider, &a.model_id).cmp(&(&b.provider, &b.model_id)));
    result
        .matches
        .dedup_by(|a, b| a.provider == b.provider && a.model_id == b.model_id);
    result
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
            capabilities: vec!["chat".to_string()],
            discovered_profile: None,
        }
    }

    fn discovered_with_capabilities(model_id: &str, capabilities: &[&str]) -> DiscoveredModel {
        let mut model = bare_discovered(model_id);
        model.capabilities = capabilities.iter().map(|c| c.to_string()).collect();
        model
    }

    #[test]
    fn embedding_models_are_excluded_from_chat_discovery() {
        // 0.17.24 stopped filtering driver discovery down to chat models so
        // embedding models could be configured separately upstream. Yolop only
        // ever picks a chat model, so an embeddings-only entry must not reach
        // the picker — nor `pick_catalog_fallback`, which can auto-select the
        // newest discovered model.
        let models = vec![
            discovered_with_capabilities("text-embedding-3-large", &["embeddings"]),
            discovered_with_capabilities("gpt-5.5", &["chat"]),
        ];

        let kept = retain_chat_models(models);

        assert_eq!(
            kept.iter().map(|m| m.model_id.as_str()).collect::<Vec<_>>(),
            vec!["gpt-5.5"]
        );
    }

    #[test]
    fn models_without_declared_capabilities_are_kept() {
        // The OpenAI-compatible fallback and any driver that does not report
        // capabilities leave the list empty; treat unknown as usable rather
        // than silently emptying the picker for Ollama and proxy endpoints.
        let kept = retain_chat_models(vec![discovered_with_capabilities("llama3.3", &[])]);

        assert_eq!(kept.len(), 1);
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
        let enriched = enrich_with_profiles(&DriverId::OpenAI, vec![bare_discovered("gpt-5.5")]);

        assert_eq!(enriched.len(), 1);
        assert_eq!(enriched[0].model_id, "gpt-5.5");
        assert_eq!(enriched[0].display_name.as_deref(), Some("GPT-5.5"));
        assert!(
            enriched[0]
                .description
                .as_deref()
                .is_some_and(|description| description.contains("reasoning model")),
            "core profile description should be carried over: {:?}",
            enriched[0].description
        );
    }

    #[test]
    fn enrichment_prefers_api_display_name_over_core_profile() {
        let mut model = bare_discovered("gpt-5.5");
        model.display_name = Some("GPT-5.5 (via gateway)".to_string());

        let enriched = enrich_with_profiles(&DriverId::OpenAI, vec![model]);

        assert_eq!(
            enriched[0].display_name.as_deref(),
            Some("GPT-5.5 (via gateway)")
        );
    }

    #[test]
    fn pick_catalog_fallback_prefers_provider_default_when_available() {
        use super::pick_catalog_fallback;
        use std::collections::HashSet;

        let choice = ProviderChoice::default_for_provider_name("openai").unwrap();
        let ids: HashSet<String> = ["gpt-5.6-sol", "gpt-4o"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let models = vec![
            DiscoveredProviderModel {
                model_id: "gpt-4o".to_string(),
                display_name: None,
                description: None,
            },
            DiscoveredProviderModel {
                model_id: "gpt-5.6-sol".to_string(),
                display_name: None,
                description: None,
            },
        ];

        let unavailable = ProviderChoice::OpenAi {
            model: "gpt-4o-mini".to_string(),
            reasoning_effort: Some("medium".to_string()),
        };
        let fallback = pick_catalog_fallback(&unavailable, &ids, &models);
        assert_eq!(fallback.label(), choice.label());
    }

    #[test]
    fn enrichment_keeps_unknown_models_with_bare_ids() {
        let enriched = enrich_with_profiles(
            &DriverId::OpenAI,
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
            reasoning_effort: None,
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

    /// The driver path is the one 0.17.24 changed, and it cannot be mocked:
    /// the OpenAI driver deliberately declines discovery for custom base URLs,
    /// so a localhost mock always falls through to the OpenAI-compatible
    /// branch instead. Only a live hosted endpoint exercises the branch that
    /// reports `capabilities`, so the embedding-exclusion proof lives here.
    /// Resolve a provider API key for a live test, mirroring
    /// `tests/integration.rs`'s helper of the same name.
    ///
    /// Returns `None` (the caller then returns early) when the key is absent,
    /// so a plain `cargo test` stays offline without `#[ignore]` — an ignored
    /// test is one nothing ever runs. `YOLOP_REQUIRE_LIVE_TESTS=1` turns a
    /// missing key into a hard failure so a misconfigured secret cannot report
    /// a false green. Presence check only: the value is never read into memory.
    fn live_key_or_skip(var: &str) -> Option<()> {
        if std::env::var_os(var).is_some_and(|value| !value.is_empty()) {
            return Some(());
        }
        assert!(
            std::env::var_os("YOLOP_REQUIRE_LIVE_TESTS").is_none(),
            "{var} is required when YOLOP_REQUIRE_LIVE_TESTS is set"
        );
        eprintln!("skipping live test: {var} not set");
        None
    }

    #[tokio::test]
    async fn discovery_openai_live_excludes_embedding_models() {
        let Some(_) = live_key_or_skip("OPENAI_API_KEY") else {
            return;
        };
        let provider = ProviderChoice::default_for_provider_name("openai").unwrap();
        let models = discover_provider_models(&provider, &Settings::default())
            .await
            .expect("openai discovery should succeed")
            .expect("openai supports model listing");

        assert!(!models.is_empty(), "openai should report models");
        let embeddings: Vec<&str> = models
            .iter()
            .map(|m| m.model_id.as_str())
            .filter(|id| id.starts_with("text-embedding-"))
            .collect();
        assert!(
            embeddings.is_empty(),
            "embedding models must not reach the chat picker: {embeddings:?}"
        );
    }

    #[tokio::test]
    async fn discovery_openrouter_live() {
        let Some(_) = live_key_or_skip("OPENROUTER_API_KEY") else {
            return;
        };
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

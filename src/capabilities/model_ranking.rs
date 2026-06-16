// Heuristics for ordering provider model lists in the setup picker.
//
// OpenRouter exposes hundreds of models; the picker surfaces a short
// "recommended" block (curated ids, the active model, profile-known
// flagship ids) before an alphabetical catalog of everything else.

use crate::capabilities::model_discovery::DiscoveredProviderModel;
use crate::runtime::ProviderChoice;
use everruns_core::DriverId;
use everruns_core::get_model_profile;

/// Models reordered for display plus how many leading rows belong in the
/// recommended section (excluding the trailing "Custom..." picker entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RankedDiscoveredModels {
    pub models: Vec<DiscoveredProviderModel>,
    pub recommended_count: usize,
}

/// Reorder discovered models for the picker. OpenRouter gets a recommended
/// block; other providers keep discovery order (newest-first from the API).
pub(crate) fn rank_discovered_models(
    provider: &str,
    models: Vec<DiscoveredProviderModel>,
    current_model: Option<&str>,
) -> RankedDiscoveredModels {
    if provider == "openrouter" {
        rank_openrouter_models(models, current_model)
    } else {
        RankedDiscoveredModels {
            recommended_count: 0,
            models,
        }
    }
}

fn rank_openrouter_models(
    models: Vec<DiscoveredProviderModel>,
    current_model: Option<&str>,
) -> RankedDiscoveredModels {
    let mut recommended_ids: Vec<String> = Vec::new();

    for suggestion in ProviderChoice::model_suggestions_for_provider("openrouter") {
        let bare = bare_model_id(suggestion);
        if models.iter().any(|model| model.model_id == bare) {
            push_unique(&mut recommended_ids, bare);
        }
    }

    if let Some(current) = current_model.map(bare_model_id)
        && models.iter().any(|model| model.model_id == current)
    {
        push_unique(&mut recommended_ids, current);
    }

    // Profile-known flagship models from major providers — capped so the
    // recommended block stays a short shortlist, not a second full catalog.
    const RECOMMENDED_CAP: usize = 20;
    let mut profile_candidates: Vec<String> = models
        .iter()
        .filter(|model| {
            !recommended_ids.contains(&model.model_id)
                && is_major_openrouter_model(&model.model_id)
                && get_model_profile(&DriverId::OpenRouter, &model.model_id).is_some()
        })
        .map(|model| model.model_id.clone())
        .collect();
    profile_candidates.sort();
    for model_id in profile_candidates {
        if recommended_ids.len() >= RECOMMENDED_CAP {
            break;
        }
        push_unique(&mut recommended_ids, model_id);
    }

    let recommended_count = recommended_ids.len();
    let mut ranked = Vec::with_capacity(models.len());
    for model_id in &recommended_ids {
        if let Some(index) = models.iter().position(|model| &model.model_id == model_id) {
            ranked.push(models[index].clone());
        }
    }

    let mut rest: Vec<DiscoveredProviderModel> = models
        .into_iter()
        .filter(|model| !recommended_ids.contains(&model.model_id))
        .collect();
    rest.sort_by(|a, b| a.model_id.cmp(&b.model_id));
    ranked.extend(rest);

    RankedDiscoveredModels {
        models: ranked,
        recommended_count,
    }
}

fn bare_model_id(spec: &str) -> String {
    spec.split_whitespace().next().unwrap_or(spec).to_string()
}

fn push_unique(ids: &mut Vec<String>, id: String) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

fn is_major_openrouter_model(model_id: &str) -> bool {
    model_id.starts_with("openai/")
        || model_id.starts_with("anthropic/")
        || model_id.starts_with("google/")
        || model_id.starts_with("nvidia/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> DiscoveredProviderModel {
        DiscoveredProviderModel {
            model_id: id.to_string(),
            display_name: None,
            description: None,
        }
    }

    #[test]
    fn openrouter_ranking_puts_curated_and_current_first_then_sorts_rest() {
        let ranked = rank_openrouter_models(
            vec![
                model("zai/glm-5"),
                model("openai/gpt-5.5"),
                model("anthropic/claude-opus-4-8"),
                model("moon/kimi-k3"),
            ],
            Some("moon/kimi-k3"),
        );

        assert_eq!(ranked.recommended_count, 3);
        let ids: Vec<&str> = ranked.models.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(
            ids,
            &[
                "openai/gpt-5.5",
                "anthropic/claude-opus-4-8",
                "moon/kimi-k3",
                "zai/glm-5",
            ]
        );
    }

    #[test]
    fn non_openrouter_providers_skip_ranking() {
        let input = vec![model("gpt-5.5"), model("gpt-5.2")];
        let ranked = rank_discovered_models("openai", input.clone(), None);
        assert_eq!(ranked.recommended_count, 0);
        let ids: Vec<&str> = ranked
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect();
        assert_eq!(ids, &["gpt-5.5", "gpt-5.2"]);
    }

    #[test]
    fn bare_model_id_strips_reasoning_effort_suffix() {
        assert_eq!(
            bare_model_id("nvidia/nemotron-3-super-120b-a12b high"),
            "nvidia/nemotron-3-super-120b-a12b"
        );
    }
}

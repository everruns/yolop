//! Informational schema for the yolop settings file (`settings.toml`).
//!
//! The schema is *informational*, not a validator. Loading settings never
//! checks against it and never rejects unknown keys (see
//! [`crate::settings::Settings::from_table`]), so a user or another tool can
//! drop extra keys into the file without breaking yolop. What the schema adds
//! is **semantics**: a title, description, type, default, and examples for
//! every known configuration key. That lets the agent explain and edit
//! configuration in a human-friendly way through the `get_config` /
//! `set_config` tools and the `yolop-config` skill — the schema here is the
//! single source of truth those surfaces render.
//!
//! Keys are addressed the way a human would name them. Scalar keys are plain
//! (`default_provider`, `attribution`); per-provider tables are addressed with
//! a dotted provider segment (`tokens.openai`, `models.anthropic`).

use crate::runtime::SUPPORTED_PROVIDERS;

/// The kind of value a configuration key accepts. Drives validation in
/// `set_config` and how `get_config` renders the current value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// A free-form string: provider name, model spec, URL, …
    Text,
    /// An on/off boolean.
    Bool,
    /// A secret string (an API token). `get_config` never echoes the value,
    /// only whether one is stored.
    Secret,
}

impl ValueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueKind::Text => "text",
            ValueKind::Bool => "bool",
            ValueKind::Secret => "secret",
        }
    }
}

/// One described configuration key. Every field is `&'static` so the whole
/// schema is a compile-time constant.
pub struct ConfigField {
    /// Canonical name the agent/user addresses. For provider-scoped tables
    /// this is the bare table name (e.g. `tokens`); see `provider_scoped`.
    pub key: &'static str,
    /// Alternative names accepted when reading or writing this key.
    pub aliases: &'static [&'static str],
    pub title: &'static str,
    pub description: &'static str,
    pub kind: ValueKind,
    /// Effective default when the key is unset — for display only.
    pub default: Option<&'static str>,
    pub examples: &'static [&'static str],
    /// When true the key names a `[table]` whose entries are keyed by
    /// provider, so it is addressed as `<key>.<provider>`.
    pub provider_scoped: bool,
}

/// The full configuration schema. Order is the user-facing display order.
pub fn schema() -> &'static [ConfigField] {
    const FIELDS: &[ConfigField] = &[
        ConfigField {
            key: "default_provider",
            aliases: &["provider"],
            title: "Default provider",
            description: "The model provider used when no --provider flag is given. A value set \
                          here takes precedence over environment-credential auto-detection, which \
                          applies only when this is unset. Persisted as `default_provider` (the \
                          legacy `provider` key is still read, and accepted as an alias).",
            kind: ValueKind::Text,
            default: Some("openai (auto-detected from available credentials)"),
            examples: &["anthropic", "openai", "google", "openrouter", "ollama"],
            provider_scoped: false,
        },
        ConfigField {
            key: "default_model",
            aliases: &["model"],
            title: "Default model",
            description: "Global fallback model spec applied to the active provider when that \
                          provider has no per-provider entry under `models`. Provider-relative, \
                          same `model [reasoning-effort]` form `/setup model` accepts. A \
                          per-provider `models.<provider>` pick always wins over this.",
            kind: ValueKind::Text,
            default: Some("the active provider's built-in default model"),
            examples: &["claude-sonnet-4-5", "gpt-5.5 high", "gemini-2.5-pro"],
            provider_scoped: false,
        },
        ConfigField {
            key: "models",
            aliases: &["model_for"],
            title: "Per-provider model",
            description: "Model spec remembered for a specific provider, so a pick survives \
                          restarts and provider switches. Addressed as `models.<provider>`.",
            kind: ValueKind::Text,
            default: None,
            examples: &[
                "models.openai = gpt-5.5 high",
                "models.anthropic = claude-opus-4-5",
            ],
            provider_scoped: true,
        },
        ConfigField {
            key: "tokens",
            aliases: &["token"],
            title: "Provider API token",
            description: "API token for a provider, stored owner-only (0o600). Environment \
                          variables (OPENAI_API_KEY, ANTHROPIC_API_KEY, …) always override the \
                          stored value. Addressed as `tokens.<provider>`.",
            kind: ValueKind::Secret,
            default: None,
            examples: &["tokens.openai = sk-…", "tokens.anthropic = …"],
            provider_scoped: true,
        },
        ConfigField {
            key: "base_urls",
            aliases: &["base_url", "url"],
            title: "Provider endpoint base URL",
            description: "Endpoint base URL for a provider. Used by the `custom` \
                          OpenAI-compatible provider (vLLM, llama.cpp, LM Studio, gateways). \
                          Addressed as `base_urls.<provider>`; must start with http:// or https://.",
            kind: ValueKind::Text,
            default: None,
            examples: &["base_urls.custom = http://localhost:8000/v1"],
            provider_scoped: true,
        },
        ConfigField {
            key: "approval_mode",
            aliases: &["approval"],
            title: "Soft-approval paranoia level",
            description: "How cautious yolop is about confirming critical actions: `protective` \
                          asks before any state change, `normal` asks only before destructive or \
                          outward-facing actions, `off` never asks. Common synonyms like \
                          `paranoid` and `yolo` are also accepted.",
            kind: ValueKind::Text,
            default: Some("normal"),
            examples: &["protective", "normal", "off"],
            provider_scoped: false,
        },
        ConfigField {
            key: "attribution",
            aliases: &[],
            title: "Commit & PR attribution",
            description: "When on, yolop appends a Co-Authored-By git trailer to commits it \
                          makes and a footer to PR descriptions it writes.",
            kind: ValueKind::Bool,
            default: Some("on"),
            examples: &["on", "off"],
            provider_scoped: false,
        },
    ];
    FIELDS
}

/// A parsed, routable configuration target. `set_config`/`get_config` match on
/// this rather than re-parsing key strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyTarget {
    DefaultProvider,
    DefaultModel,
    Attribution,
    ApprovalMode,
    /// Per-provider model spec, for the named provider.
    Model(String),
    /// Per-provider API token.
    Token(String),
    /// Per-provider endpoint base URL.
    BaseUrl(String),
}

impl KeyTarget {
    /// The schema field describing this target.
    pub fn field(&self) -> &'static ConfigField {
        let key = match self {
            KeyTarget::DefaultProvider => "default_provider",
            KeyTarget::DefaultModel => "default_model",
            KeyTarget::Attribution => "attribution",
            KeyTarget::ApprovalMode => "approval_mode",
            KeyTarget::Model(_) => "models",
            KeyTarget::Token(_) => "tokens",
            KeyTarget::BaseUrl(_) => "base_urls",
        };
        schema()
            .iter()
            .find(|f| f.key == key)
            .expect("KeyTarget always maps to a schema field")
    }
}

/// Parse a human-supplied key (with aliases) into a routable target. Provider
/// segments are validated against the supported provider list.
pub fn parse_key(input: &str) -> Result<KeyTarget, String> {
    let norm = input.trim().to_ascii_lowercase();
    if norm.is_empty() {
        return Err(format!("empty config key; known keys: {}", known_keys()));
    }
    let (head, sub) = match norm.split_once('.') {
        Some((h, t)) => (h, Some(t.trim())),
        None => (norm.as_str(), None),
    };

    let scalar = |target: KeyTarget| -> Result<KeyTarget, String> {
        if sub.is_some() {
            return Err(format!(
                "`{head}` is a scalar key and takes no `.<provider>` segment"
            ));
        }
        Ok(target)
    };
    let scoped = |make: fn(String) -> KeyTarget| -> Result<KeyTarget, String> {
        let provider = sub.filter(|s| !s.is_empty()).ok_or_else(|| {
            format!("`{head}` is per-provider; address it as `{head}.<provider>`")
        })?;
        if !SUPPORTED_PROVIDERS.contains(&provider) {
            return Err(format!(
                "unknown provider `{provider}`; expected one of {}",
                SUPPORTED_PROVIDERS.join(", ")
            ));
        }
        Ok(make(provider.to_string()))
    };

    match head {
        "default_provider" | "provider" => scalar(KeyTarget::DefaultProvider),
        "default_model" | "model" => scalar(KeyTarget::DefaultModel),
        "attribution" => scalar(KeyTarget::Attribution),
        "approval_mode" | "approval" => scalar(KeyTarget::ApprovalMode),
        "models" | "model_for" => scoped(KeyTarget::Model),
        "tokens" | "token" => scoped(KeyTarget::Token),
        "base_urls" | "base_url" | "url" => scoped(KeyTarget::BaseUrl),
        _ => Err(format!(
            "unknown config key `{input}`; known keys: {}",
            known_keys()
        )),
    }
}

/// Comma-separated list of canonical keys, for error messages.
pub fn known_keys() -> String {
    schema()
        .iter()
        .map(|f| {
            if f.provider_scoped {
                format!("{}.<provider>", f.key)
            } else {
                f.key.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_canonical_targets() {
        assert_eq!(parse_key("provider").unwrap(), KeyTarget::DefaultProvider);
        assert_eq!(
            parse_key("default_provider").unwrap(),
            KeyTarget::DefaultProvider
        );
        assert_eq!(parse_key("model").unwrap(), KeyTarget::DefaultModel);
        assert_eq!(parse_key("default_model").unwrap(), KeyTarget::DefaultModel);
        assert_eq!(parse_key("approval").unwrap(), KeyTarget::ApprovalMode);
        assert_eq!(parse_key("approval_mode").unwrap(), KeyTarget::ApprovalMode);
    }

    #[test]
    fn provider_scoped_keys_parse_and_validate() {
        assert_eq!(
            parse_key("tokens.openai").unwrap(),
            KeyTarget::Token("openai".to_string())
        );
        assert_eq!(
            parse_key("url.custom").unwrap(),
            KeyTarget::BaseUrl("custom".to_string())
        );
        assert_eq!(
            parse_key("Models.Anthropic").unwrap(),
            KeyTarget::Model("anthropic".to_string())
        );
    }

    #[test]
    fn scalar_key_rejects_provider_segment() {
        assert!(parse_key("attribution.openai").is_err());
        assert!(parse_key("default_model.openai").is_err());
    }

    #[test]
    fn provider_scoped_requires_segment_and_known_provider() {
        assert!(parse_key("tokens").unwrap_err().contains("per-provider"));
        assert!(
            parse_key("tokens.nope")
                .unwrap_err()
                .contains("unknown provider")
        );
    }

    #[test]
    fn unknown_key_lists_known_keys() {
        let err = parse_key("frobnicate").unwrap_err();
        assert!(err.contains("default_provider"));
        assert!(err.contains("tokens.<provider>"));
    }

    #[test]
    fn every_target_maps_to_a_field() {
        for target in [
            KeyTarget::DefaultProvider,
            KeyTarget::DefaultModel,
            KeyTarget::Attribution,
            KeyTarget::ApprovalMode,
            KeyTarget::Model("openai".into()),
            KeyTarget::Token("openai".into()),
            KeyTarget::BaseUrl("custom".into()),
        ] {
            let _ = target.field();
        }
    }
}

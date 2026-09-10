//! Guided hint for the OpenRouter account-attestation gate.
//!
//! Some OpenRouter models refuse with HTTP 403 until the account completes a
//! confirmation such as 18+ age verification. The provider body names the
//! missing attestation types and points at the preferences page, but yolop
//! surfaces the raw JSON inside `turn error:`, where the URL is glued to JSON
//! punctuation and Ghostty cannot autolink it.
//!
//! Stopgap: classify the provider error text here and render a guided hint
//! with a clean, standalone URL. Replace this with the upstream structured
//! error once `everruns-openrouter` reports the gate as a first-class kind
//! (attestation types plus confirm URL) instead of a JSON string.

/// Fallback confirm page when the provider body carries no usable URL.
pub(crate) const OPENROUTER_PREFERENCES_URL: &str = "https://openrouter.ai/settings/preferences";

/// An OpenRouter model gated behind account confirmations.
pub(crate) struct AttestationRequirement {
    /// Clean confirm URL safe to print on its own for terminal autolink.
    pub url: String,
    /// Raw `missing_attestation_types` entries, for example `age_18plus`.
    pub missing_types: Vec<String>,
}

/// Detect the OpenRouter attestation gate in a provider error message.
///
/// Matches the structured `missing_attestation_types` field or the prose
/// `requires you to complete the following before use` sentence, both of
/// which OpenRouter sends with HTTP 403 for gated models.
pub(crate) fn detect_openrouter_attestation(message: &str) -> Option<AttestationRequirement> {
    let gated = message.contains("missing_attestation_types")
        || message.contains("requires you to complete the following before use");
    if !gated || !message.contains("403") {
        return None;
    }
    // Provider bodies arrive Debug-formatted inside the error, so quotes
    // show up as `\"`. Normalize before parsing field names and URLs.
    let message = message.replace("\\\"", "\"");
    Some(AttestationRequirement {
        url: first_clean_url(&message).unwrap_or_else(|| OPENROUTER_PREFERENCES_URL.to_string()),
        missing_types: missing_attestation_types(&message),
    })
}

/// Guided follow-up line for [`detect_openrouter_attestation`].
///
/// Kept as plain `Author::System` text with a bare URL: only
/// `Author::Assistant` lines run through the tuika markdown renderer, so
/// system lines rely on native terminal autolink, which needs the URL clean
/// and separated from JSON punctuation.
pub(crate) fn attestation_error_hint(message: &str) -> Option<String> {
    let requirement = detect_openrouter_attestation(message)?;
    if requirement.missing_types.is_empty() {
        return Some(format!(
            "This OpenRouter model needs an account confirmation before use. Complete it at {} and retry the turn.",
            requirement.url
        ));
    }
    Some(format!(
        "This OpenRouter model needs confirmation before use (missing: {}). Complete it at {} and retry the turn.",
        requirement.missing_types.join(", "),
        requirement.url
    ))
}

/// Collect the quoted entries of `"missing_attestation_types": [...]`.
fn missing_attestation_types(message: &str) -> Vec<String> {
    let field = "\"missing_attestation_types\"";
    let start = match message.find(field) {
        Some(index) => index + field.len(),
        None => return Vec::new(),
    };
    let rest = &message[start..];
    let open = match rest.find('[') {
        Some(index) => index + 1,
        None => return Vec::new(),
    };
    let rest = &rest[open..];
    let end = match rest.find(']') {
        Some(index) => index,
        None => return Vec::new(),
    };
    let list = &rest[..end];
    let bytes = list.as_bytes();
    let mut types = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        index += 1;
        let mut value = String::new();
        while index < bytes.len() && bytes[index] != b'"' {
            if bytes[index] == b'\\' && index + 1 < bytes.len() {
                index += 1;
            }
            value.push(bytes[index] as char);
            index += 1;
        }
        if !value.is_empty() {
            types.push(value);
        }
    }
    types
}

/// First `https://` URL in the message, stripped of surrounding JSON
/// punctuation and unescaped (`\/` to `/`) so terminals can autolink it.
fn first_clean_url(message: &str) -> Option<String> {
    // Some bodies escape slashes as `\/`; unescape before scanning so the
    // `https://` prefix is found and terminals get a clean link.
    let message = message.replace("\\/", "/");
    let start = message.find("https://")?;
    let rest = &message[start..];
    let end = rest
        .find(|char: char| {
            char.is_whitespace() || matches!(char, '"' | '\\' | '<' | '>' | '`' | ')' | ']' | '}')
        })
        .unwrap_or(rest.len());
    let url = rest[..end]
        .trim_end_matches(['.', ',', ';', '\'', '!', ':'])
        .replace("\\/", "/");
    if url.len() > "https://".len() + 3 && url[8..].contains('.') {
        Some(url)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATED_ERROR: &str = r#"provider 'openrouter': OpenAI Responses error (403 Forbidden): "{\"error\":{\"message\":\"This model requires you to complete the following before use: 18+ age confirmation. Confirm at https://openrouter.ai/settings/preferences.\",\"code\":403,\"metadata\":{\"missing_attestation_types\":[\"age_18plus\"],\"failed_routing_step\":\"Gate Endpoints with Attestations\"}}}"#;

    #[test]
    fn detects_gate_with_types_and_clean_url() {
        let requirement =
            detect_openrouter_attestation(GATED_ERROR).expect("gated error is detected");
        assert_eq!(requirement.missing_types, vec!["age_18plus"]);
        assert_eq!(
            requirement.url,
            "https://openrouter.ai/settings/preferences"
        );
    }

    #[test]
    fn hint_names_gate_and_points_at_preferences() {
        let hint = attestation_error_hint(GATED_ERROR).expect("hint is produced");
        assert!(hint.contains("age_18plus"), "hint: {hint}");
        assert!(
            hint.contains("https://openrouter.ai/settings/preferences"),
            "hint: {hint}"
        );
        assert!(!hint.contains('{'), "hint carries no JSON blob: {hint}");
    }

    #[test]
    fn ignores_unrelated_quota_403() {
        let message = "provider 'openrouter': OpenAI Responses error (403 Forbidden): \
            {\"error\":{\"message\":\"Insufficient credits. Top up at https://openrouter.ai/account.\",\"code\":403}}";
        assert!(detect_openrouter_attestation(message).is_none());
        assert!(attestation_error_hint(message).is_none());
    }

    #[test]
    fn falls_back_to_preferences_url_without_url_in_body() {
        let message = "OpenAI Responses error (403 Forbidden): \
            missing_attestation_types [age_18plus] requires you to complete the following before use";
        let requirement =
            detect_openrouter_attestation(message).expect("gate without URL is detected");
        assert_eq!(
            requirement.url, OPENROUTER_PREFERENCES_URL,
            "falls back to the confirm page"
        );
    }

    #[test]
    fn unescapes_json_slash_sequences_in_url() {
        let message = "error 403 missing_attestation_types [age_18plus] confirm at \
            https:\\/\\/openrouter.ai\\/settings\\/preferences.\"";
        let requirement = detect_openrouter_attestation(message).expect("escaped URL is detected");
        assert_eq!(
            requirement.url,
            "https://openrouter.ai/settings/preferences"
        );
    }
}

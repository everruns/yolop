//! Guided hints for OpenRouter account, billing, and data-policy gates.
//!
//! Some OpenRouter models refuse until the account completes a confirmation
//! (18+ age verification, HTTP 403) or until a data-policy setting allows the
//! endpoint (paid-model training, HTTP 404). The provider body names the
//! missing step and points at a settings page, but yolop used to surface the
//! raw JSON inside `turn error:`, where the URL is glued to JSON punctuation
//! and Ghostty cannot autolink it. The generic assistant apology
//! ("misconfiguration", "try again later") is worse: it hides an action the
//! user can take.
//!
//! Stopgap: classify the provider error text here and render a guided hint
//! with a labeled markdown link. Assistant lines run through the tuika
//! markdown renderer, so `[OpenRouter preferences](url)` becomes an OSC 8
//! target. Compact work also shows Assistant lines, while a System hint would
//! sit in collapsed details. Replace this with the upstream structured error
//! once `everruns-openrouter` reports these gates as first-class kinds.
//! Provider 0.21 added `LlmErrorKind::AttestationRequired` plus a structured
//! parser, but the parser drops overlong types and returns `None` where this
//! module promises truncation and an empty fallback, so the string-level
//! contract here stays until the driver itself surfaces the gate.
/// Fallback confirm page when an attestation body carries no usable URL.
pub(crate) const OPENROUTER_PREFERENCES_URL: &str = "https://openrouter.ai/settings/preferences";

/// Fallback data-policy page when a guardrail body carries no usable URL.
pub(crate) const OPENROUTER_PRIVACY_URL: &str = "https://openrouter.ai/settings/privacy";

/// Credits page for OpenRouter 402 billing pressure (in-flight budget). Matches
/// the remedy OpenRouter itself sends in the error body.
pub(crate) const OPENROUTER_CREDITS_URL: &str = "https://openrouter.ai/settings/credits";

/// An OpenRouter model gated behind account confirmations.
pub(crate) struct AttestationRequirement {
    /// Clean confirm URL safe to print on its own for terminal autolink.
    pub url: String,
    /// Raw `missing_attestation_types` entries, for example `age_18plus`.
    pub missing_types: Vec<String>,
}

/// An OpenRouter model blocked by the account's data-policy / guardrail filter.
pub(crate) struct GuardrailBlock {
    /// Clean settings URL safe to print on its own for terminal autolink.
    pub url: String,
    /// Humanized `ineligibility_reasons[].reason` entries.
    pub reasons: Vec<String>,
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
        missing_types: json_string_array(&message, "missing_attestation_types"),
    })
}

/// Detect the OpenRouter data-policy / guardrail filter.
///
/// Matches `ineligibility_reasons`, the "guardrail restrictions and data policy"
/// sentence, or `Filter by Guardrails`. Distinct from the attestation 403.
pub(crate) fn detect_openrouter_guardrail(message: &str) -> Option<GuardrailBlock> {
    if detect_openrouter_attestation(message).is_some() {
        return None;
    }
    let blocked = message.contains("ineligibility_reasons")
        || message.contains("guardrail restrictions")
        || message.contains("Filter by Guardrails");
    if !blocked {
        return None;
    }
    let message = message.replace("\\\"", "\"");
    let reasons = ineligibility_reasons(&message);
    let url = json_string_values(&message, "configure_url")
        .into_iter()
        .find(|url| url.starts_with("https://"))
        .or_else(|| first_clean_url(&message))
        .unwrap_or_else(|| OPENROUTER_PRIVACY_URL.to_string());
    Some(GuardrailBlock { url, reasons })
}

/// Guided follow-up for [`detect_openrouter_attestation`].
///
/// Assistant markdown so the confirm URL is a labeled OSC 8 link, not a
/// URL glued to JSON punctuation on a System line.
pub(crate) fn attestation_error_hint(message: &str) -> Option<String> {
    let requirement = detect_openrouter_attestation(message)?;
    let link = markdown_link(openrouter_link_label(&requirement.url), &requirement.url);
    if requirement.missing_types.is_empty() {
        return Some(format!(
            "This OpenRouter model needs an account confirmation before use. Complete it at {link} and retry the turn."
        ));
    }
    let missing = requirement
        .missing_types
        .iter()
        .map(|item| humanize_gate_token(item))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "This OpenRouter model needs confirmation before use (missing: {missing}). Complete it at {link} and retry the turn."
    ))
}

/// Guided follow-up for [`detect_openrouter_guardrail`].
pub(crate) fn guardrail_error_hint(message: &str) -> Option<String> {
    let block = detect_openrouter_guardrail(message)?;
    let link = markdown_link(openrouter_link_label(&block.url), &block.url);
    if block.reasons.is_empty() {
        return Some(format!(
            "OpenRouter blocked this model under your data policy. Change the setting at {link} and retry the turn."
        ));
    }
    Some(format!(
        "OpenRouter blocked this model under your data policy ({}). Change the setting at {link} and retry the turn.",
        block.reasons.join(", ")
    ))
}

/// OpenRouter 402 billing pressure: the request would exceed available credits
/// while other requests are still in flight.
pub(crate) struct BillingPressure {
    /// Parsed `Retry-After` seconds from the provider body, when present.
    pub retry_after_secs: Option<u64>,
}

/// Detect OpenRouter 402 billing pressure in a provider error message.
///
/// Scoped to 402 plus a billing signal (`in_flight_budget_exhausted`,
/// `available credits`, `insufficient credits`, `payment required`) so
/// unrelated 402s and the 403 quota case keep their current behavior.
pub(crate) fn detect_openrouter_billing(message: &str) -> Option<BillingPressure> {
    let lower = message.to_lowercase();
    if !lower.contains("402") {
        return None;
    }
    let billing = lower.contains("in_flight_budget_exhausted")
        || (lower.contains("in-flight") && lower.contains("credit"))
        || lower.contains("available credits")
        || lower.contains("insufficient credits")
        || lower.contains("payment required");
    if !billing {
        return None;
    }
    Some(BillingPressure {
        retry_after_secs: parse_retry_after_secs(message),
    })
}

/// Guided follow-up for [`detect_openrouter_billing`].
pub(crate) fn billing_error_hint(message: &str) -> Option<String> {
    let pressure = detect_openrouter_billing(message)?;
    let wait = match pressure.retry_after_secs {
        Some(secs) => format!(
            "Wait {} for those to settle and retry",
            format_retry_after(secs)
        ),
        None => "Wait for those to settle and retry".to_string(),
    };
    let link = markdown_link("OpenRouter credits", OPENROUTER_CREDITS_URL);
    Some(format!(
        "OpenRouter paused this request: it would exceed your available credits while other requests are still in flight. {wait}, or add credits at {link} to raise the limit."
    ))
}

/// First digit run after the `Retry-After` marker, for example
/// `"Retry-After":"120"` or `Retry-After: 120`.
fn parse_retry_after_secs(message: &str) -> Option<u64> {
    let lower = message.to_lowercase();
    let marker = lower.find("retry-after")?;
    let rest = &message[marker + "retry-after".len()..];
    let mut digits = String::new();
    let mut started = false;
    for ch in rest.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            started = true;
        } else if started {
            break;
        }
    }
    let secs: u64 = digits.parse().ok()?;
    if secs == 0 { None } else { Some(secs) }
}

fn format_retry_after(secs: u64) -> String {
    if secs < 60 {
        if secs == 1 {
            "about 1 second".to_string()
        } else {
            format!("about {secs} seconds")
        }
    } else {
        let minutes = (secs + 30) / 60;
        if minutes == 1 {
            "about 1 minute".to_string()
        } else {
            format!("about {minutes} minutes")
        }
    }
}

/// Actionable OpenRouter hints for a failed turn: attestation, guardrail, billing.
pub(crate) fn openrouter_error_hints(message: &str) -> Vec<String> {
    if let Some(hint) = attestation_error_hint(message) {
        return vec![hint];
    }
    if let Some(hint) = guardrail_error_hint(message) {
        return vec![hint];
    }
    if let Some(hint) = billing_error_hint(message) {
        return vec![hint];
    }
    Vec::new()
}

/// Generic everruns apologies that hide a classified, user-fixable error.
pub(crate) fn is_generic_provider_apology(text: &str) -> bool {
    let text = text.trim();
    text.eq_ignore_ascii_case(
        "There is a misconfiguration with the AI provider. Please contact support.",
    ) || text.starts_with("I encountered an error while processing your request.")
}

fn markdown_link(label: &str, url: &str) -> String {
    format!("[{label}]({url})")
}

fn openrouter_link_label(url: &str) -> &'static str {
    if url.contains("/settings/privacy") {
        "OpenRouter privacy settings"
    } else if url.contains("/settings/preferences") {
        "OpenRouter preferences"
    } else if url.contains("/settings/credits") {
        "OpenRouter credits"
    } else {
        "OpenRouter settings"
    }
}

fn humanize_gate_token(token: &str) -> String {
    match token {
        "age_18plus" => "18+ age confirmation".to_string(),
        "paid-model-training-violation-by-account" => "paid-model training".to_string(),
        other => {
            let trimmed = other
                .trim_end_matches("-by-account")
                .trim_end_matches("_by_account");
            trimmed.replace(['_', '-'], " ")
        }
    }
}

fn ineligibility_reasons(message: &str) -> Vec<String> {
    let field = "\"ineligibility_reasons\"";
    let start = match message.find(field) {
        Some(index) => index + field.len(),
        None => return Vec::new(),
    };
    json_string_values(&message[start..], "reason")
        .into_iter()
        .map(|reason| humanize_gate_token(&reason))
        .collect()
}

/// Every `"field": "value"` occurrence. Does not walk into arrays of strings.
fn json_string_values(message: &str, field: &str) -> Vec<String> {
    let needle = format!("\"{field}\"");
    let mut values = Vec::new();
    let mut search = message;
    while let Some(index) = search.find(&needle) {
        let after = &search[index + needle.len()..];
        if let Some(value) = json_string_value(after) {
            values.push(value);
        }
        search = &search[index + needle.len()..];
    }
    values
}

/// Quoted entries of `"field": ["a", "b"]`.
fn json_string_array(message: &str, field: &str) -> Vec<String> {
    let needle = format!("\"{field}\"");
    let start = match message.find(&needle) {
        Some(index) => index + needle.len(),
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
    quoted_strings_in(&rest[..end])
}

fn json_string_value(after_field: &str) -> Option<String> {
    let rest = after_field.trim_start();
    let rest = rest.strip_prefix(':')?;
    let rest = rest.trim_start();
    if rest.starts_with('[') {
        return None;
    }
    quoted_strings_in(rest).into_iter().next()
}

fn quoted_strings_in(list: &str) -> Vec<String> {
    let bytes = list.as_bytes();
    let mut values = Vec::new();
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
            values.push(value);
        }
        if index < bytes.len() {
            index += 1;
        }
    }
    values
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

    const GUARDRAIL_ERROR: &str = r#"provider 'openrouter': OpenAI Responses API error (404 Not Found): {"error":{"message":"0 endpoints out of 1 requested are available matching your guardrail restrictions and data policy. We removed them for the following reasons (an endpoint may have matched multiple reasons):\nPaid model training violation (account settings): 1 endpoint excluded; configurable at https://openrouter.ai/settings/privacy","code":404,"metadata":{"input_endpoint_count":1,"ineligibility_reasons":[{"reason":"paid-model-training-violation-by-account","count":1,"configure_url":"https://openrouter.ai/settings/privacy"}],"routing_funnel":[{"step":"Initial Endpoints","endpoint_count":1}],"failed_routing_step":"Filter by Guardrails"}}}"#;

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
        assert!(hint.contains("18+ age confirmation"), "hint: {hint}");
        assert!(
            hint.contains("[OpenRouter preferences](https://openrouter.ai/settings/preferences)"),
            "hint is a labeled markdown link: {hint}"
        );
        assert!(!hint.contains('{'), "hint carries no JSON blob: {hint}");
        assert!(!hint.contains("age_18plus"), "hint is humanized: {hint}");
    }

    #[test]
    fn ignores_unrelated_quota_403() {
        let message = "provider 'openrouter': OpenAI Responses error (403 Forbidden): \
            {\"error\":{\"message\":\"Insufficient credits. Top up at https://openrouter.ai/account.\",\"code\":403}}";
        assert!(detect_openrouter_attestation(message).is_none());
        assert!(attestation_error_hint(message).is_none());
        assert!(detect_openrouter_guardrail(message).is_none());
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

    #[test]
    fn detects_guardrail_block_with_reason_and_configure_url() {
        let block =
            detect_openrouter_guardrail(GUARDRAIL_ERROR).expect("guardrail error is detected");
        assert_eq!(block.reasons, vec!["paid-model training"]);
        assert_eq!(block.url, "https://openrouter.ai/settings/privacy");
        assert!(detect_openrouter_attestation(GUARDRAIL_ERROR).is_none());
    }

    #[test]
    fn guardrail_hint_is_a_labeled_privacy_link() {
        let hint = guardrail_error_hint(GUARDRAIL_ERROR).expect("hint is produced");
        assert!(hint.contains("paid-model training"), "hint: {hint}");
        assert!(
            hint.contains("[OpenRouter privacy settings](https://openrouter.ai/settings/privacy)"),
            "hint is a labeled markdown link: {hint}"
        );
        assert!(!hint.contains('{'), "hint carries no JSON blob: {hint}");
        assert!(
            !hint.contains("paid-model-training-violation-by-account"),
            "hint is humanized: {hint}"
        );
    }

    #[test]
    fn guardrail_falls_back_to_privacy_url_without_url_in_body() {
        let message = "OpenAI Responses API error (404 Not Found): \
            0 endpoints matching your guardrail restrictions and data policy";
        let block =
            detect_openrouter_guardrail(message).expect("guardrail without URL is detected");
        assert_eq!(
            block.url, OPENROUTER_PRIVACY_URL,
            "falls back to the privacy page"
        );
    }

    #[test]
    fn openrouter_hints_prefer_attestation_over_guardrail() {
        assert_eq!(openrouter_error_hints(GATED_ERROR).len(), 1);
        assert_eq!(openrouter_error_hints(GUARDRAIL_ERROR).len(), 1);
        assert!(openrouter_error_hints("connection reset by peer").is_empty());
    }

    const BILLING_ERROR: &str = r#"provider 'openrouter': OpenAI Responses API error (402 Payment Required): {"error":{"message":"This request would exceed your available credits given your current in-flight requests. Retry after in-flight requests settle, or add credits.","code":402,"metadata":{"reason":"in_flight_budget_exhausted","remedy_hint":"Retry after your in-flight requests settle (see the Retry-After header). Adding credits at https://openrouter.ai/settings/credits raises your in-flight budget.","headers":{"Retry-After":"120"}},"user_id":"user_39S0vhHVDLm80mLSdZVcs9SGU1yB"}"#;

    #[test]
    fn detects_billing_pressure_with_retry_after() {
        let pressure = detect_openrouter_billing(BILLING_ERROR).expect("billing error is detected");
        assert_eq!(pressure.retry_after_secs, Some(120));
    }

    #[test]
    fn billing_hint_names_wait_and_credits_link() {
        let hint = billing_error_hint(BILLING_ERROR).expect("hint is produced");
        assert!(
            hint.contains("about 2 minutes"),
            "billing hint names the retry wait"
        );
        assert!(
            hint.contains("[OpenRouter credits](https://openrouter.ai/settings/credits)"),
            "billing hint links to OpenRouter credits with a label"
        );
        assert!(!hint.contains('{'), "billing hint carries no JSON blob");
        assert!(
            !hint.contains("user_39S0"),
            "billing hint drops the provider user id"
        );
    }

    #[test]
    fn billing_without_retry_after_uses_generic_wait() {
        let message = "OpenAI Responses API error (402 Payment Required): \
            this request would exceed your available credits given in-flight requests";
        let pressure =
            detect_openrouter_billing(message).expect("billing without header is detected");
        assert_eq!(pressure.retry_after_secs, None);
        let hint = billing_error_hint(message).expect("hint is produced");
        assert!(
            hint.contains("Wait for those to settle"),
            "billing hint advises waiting for in-flight requests"
        );
    }

    #[test]
    fn ignores_unrelated_402_without_billing_words() {
        assert!(detect_openrouter_billing("request failed with status 402").is_none());
        assert!(billing_error_hint("request failed with status 402").is_none());
        assert!(openrouter_error_hints("request failed with status 402").is_empty());
    }

    #[test]
    fn quota_403_is_not_billing_pressure() {
        let message = "provider 'openrouter': OpenAI Responses error (403 Forbidden): \
            {\"error\":{\"message\":\"Insufficient credits. Top up at https://openrouter.ai/account.\",\"code\":403}}";
        assert!(detect_openrouter_billing(message).is_none());
        assert!(billing_error_hint(message).is_none());
    }

    #[test]
    fn openrouter_hints_include_billing() {
        let hints = openrouter_error_hints(BILLING_ERROR);
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].contains("OpenRouter paused this request"),
            "hint: {}",
            hints[0]
        );
    }

    #[test]
    fn generic_provider_apologies_are_recognized() {
        assert!(is_generic_provider_apology(
            "There is a misconfiguration with the AI provider. Please contact support."
        ));
        assert!(is_generic_provider_apology(
            "I encountered an error while processing your request. Please try again later."
        ));
        assert!(is_generic_provider_apology(
            "I encountered an error while processing your request."
        ));
        assert!(!is_generic_provider_apology(
            "OpenRouter blocked this model under your data policy."
        ));
    }
}

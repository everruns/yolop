//! Deterministic credit-exhaustion detection, readable formatting, and background probe.
//!
//! The screenshot that motivated this shows three texts merged into one
//! paragraph: an upstream credit rewrite, the raw OpenRouter 402 message, and
//! the in-flight hint, with a bare key URL. Paseo via ACP concatenates
//! successive message chunks, so the fix is to emit one markdown block with
//! blank-line separated paragraphs and markdown links. No information is
//! dropped: requested/affordable token counts, both URLs, retry guidance, and
//! a short raw excerpt are all preserved.
//!
//! The watcher is deterministic on purpose: fixed poll interval, same probe,
//! bounded attempts. Probing hits the OpenRouter key endpoint (no token spend)
//! instead of retrying the turn, so waiting costs nothing.

use std::time::Duration;

/// Public OpenRouter credits page. Stable, safe to link always.
pub const CREDITS_URL: &str = "https://openrouter.ai/settings/credits";
/// Key info endpoint. Cheap GET, does not spend tokens.
pub const KEY_INFO_URL: &str = "https://openrouter.ai/api/v1/auth/key";
/// Default probe cadence. Fixed so behavior is deterministic.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Default bound: 60 probes at 60s covers one hour of waiting.
pub const DEFAULT_MAX_PROBES: u32 = 60;

/// Parsed credit-exhaustion signal from a provider error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditExhaustion {
    /// What the turn asked for (max output tokens), when the error states it.
    pub requested_tokens: Option<u64>,
    /// What the balance covers, when the error states it.
    pub affordable_tokens: Option<u64>,
    /// Per-key limit URL embedded in the error, when present.
    pub key_url: Option<String>,
    /// Retry-After seconds, when the error carries one.
    pub retry_after_secs: Option<u64>,
    /// True when the error mentions other requests still in flight.
    pub in_flight: bool,
}

impl CreditExhaustion {
    /// Detect credit exhaustion in a provider/turn error string.
    ///
    /// Matches both the insufficient-for-max-tokens shape ("requires more
    /// credits, or fewer max_tokens ... can only afford N") and the upstream
    /// rewrite ("balance covers N"), plus the in-flight shape ("still in
    /// flight ... would exceed your available credits").
    pub fn detect(error: &str) -> Option<Self> {
        let lower = error.to_lowercase();
        let mentions_402 = lower.contains("402") || lower.contains("payment required");
        let mentions_credits = lower.contains("more credits")
            || lower.contains("out of credits")
            || lower.contains("can only afford")
            || lower.contains("balance covers")
            || lower.contains("fewer max_tokens")
            || lower.contains("fewer max tokens");
        let in_flight = lower.contains("still in flight")
            || lower.contains("still in-flight")
            || lower.contains("in flight")
                && lower.contains("exceed")
                && lower.contains("available credits");
        // Upstream rewrites sometimes omit the status code but keep the credit
        // phrasing plus token counts. Accept those too, and the distinctive
        // in-flight shape on its own.
        let upstream = lower.contains("out of credits")
            && (lower.contains("output tokens") || lower.contains("max output tokens"));
        if !mentions_402 && !upstream && !in_flight {
            return None;
        }

        let requested_tokens = parse_requested_tokens(error);
        let affordable_tokens = parse_affordable_tokens(error);
        // Require at least one credit signal beyond a bare 402.
        if requested_tokens.is_none()
            && affordable_tokens.is_none()
            && !mentions_credits
            && !in_flight
        {
            return None;
        }

        Some(Self {
            requested_tokens,
            affordable_tokens,
            key_url: extract_key_url(error),
            retry_after_secs: parse_retry_after(error),
            in_flight,
        })
    }

    /// Render one markdown block with blank-line separated paragraphs.
    ///
    /// Keeps every fact: token counts, key URL, credits URL, in-flight note,
    /// watcher notice, and a trimmed raw excerpt. Links are markdown so Paseo
    /// renders them clickable.
    pub fn markdown_block(&self, raw_excerpt: &str) -> String {
        let mut block =
            String::from("**Out of credits: turn paused, background checker running.**\n\n");
        match (self.requested_tokens, self.affordable_tokens) {
            (Some(req), Some(afford)) => {
                block.push_str(&format!(
                    "This request asked for up to {req} output tokens, but the balance covers about {afford}. Add credits or lower max output tokens to continue.\n\n"
                ));
            }
            (None, Some(afford)) => {
                block.push_str(&format!(
                    "The balance covers about {afford} tokens for this request. Add credits or lower max output tokens to continue.\n\n"
                ));
            }
            _ => {
                block.push_str(
                    "The provider rejected this request for lack of credits. Add credits or lower max output tokens to continue.\n\n",
                );
            }
        }

        if self.in_flight {
            block.push_str(
                "OpenRouter also reports other requests still in flight: this one would exceed the remaining balance while they settle. Waiting usually clears this without any change.\n\n",
            );
        }

        block.push_str("What helps:\n\n");
        if let Some(key_url) = &self.key_url {
            block.push_str(&format!("- Adjust the [key's daily limit]({key_url}).\n"));
        }
        block.push_str(&format!(
            "- Add credits at [OpenRouter credits]({CREDITS_URL}).\n"
        ));
        if let Some(retry_after) = self.retry_after_secs {
            block.push_str(&format!(
                "- The provider asked to retry after about {retry_after}s.\n"
            ));
        } else {
            block.push_str("- Or wait for in-flight requests to settle, then retry.\n");
        }
        block.push_str(
            "\nBackground checker: probing the key endpoint every 60s without spending tokens. It posts here when credits are back and retries the paused turn once.\n\n",
        );

        let trimmed = raw_excerpt.trim();
        if !trimmed.is_empty() {
            let excerpt: String = trimmed.chars().take(500).collect();
            block.push_str(&format!("Details: {excerpt}"));
        }
        block
    }
}

/// Fixed-interval watcher state. No jitter, no backoff randomness.
#[derive(Debug, Clone)]
pub struct CreditWatcher {
    /// How often to probe.
    pub poll_interval: Duration,
    /// How many probes before giving up.
    pub max_probes: u32,
    /// Probes already performed.
    pub attempts: u32,
}

impl CreditWatcher {
    /// Start watching with deterministic defaults.
    pub fn start() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_probes: DEFAULT_MAX_PROBES,
            attempts: 0,
        }
    }

    /// Record one probe. Returns true when the watcher should keep waiting.
    pub fn record_probe(&mut self) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts < self.max_probes
    }

    /// Message posted when a probe shows credits are back.
    pub fn credits_back_message() -> &'static str {
        "Credits are back: the background checker sees available balance again. Retrying the paused turn once now."
    }
}

/// Parse "up to 65536" style requested token counts.
fn parse_requested_tokens(error: &str) -> Option<u64> {
    for needle in ["up to ", "upto "] {
        let mut search = error;
        while let Some(idx) = search.to_lowercase().find(needle) {
            let rest = &search[idx + needle.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            // Skip thousands separators like 65,536 by stripping commas first.
            let digits: String = digits.chars().collect();
            if let Ok(value) = digits.parse::<u64>() {
                return Some(value);
            }
            search = &search[idx + needle.len()..];
            if search.is_empty() {
                break;
            }
        }
    }
    // Fallback: "requested up to 65536 tokens" wording varies by provider.
    find_number_after(error, &["requested up to", "asked for up to"])
}

/// Parse "can only afford 53197" / "balance covers 53197" style counts.
fn parse_affordable_tokens(error: &str) -> Option<u64> {
    find_number_after(error, &["can only afford", "balance covers", "covers "])
}

/// Find the first number after any of the given case-insensitive markers.
fn find_number_after(haystack: &str, markers: &[&str]) -> Option<u64> {
    let lower = haystack.to_lowercase();
    for marker in markers {
        let mut offset = 0;
        while let Some(idx) = lower[offset..].find(marker) {
            let rest = &haystack[offset + idx + marker.len()..];
            // Walk past non-digits, then collect digits (ignoring commas).
            let mut digits = String::new();
            let mut in_number = false;
            for ch in rest.chars() {
                if ch.is_ascii_digit() {
                    digits.push(ch);
                    in_number = true;
                } else if ch == ',' && in_number {
                    continue;
                } else if in_number {
                    break;
                }
            }
            if let Ok(value) = digits.parse::<u64>() {
                return Some(value);
            }
            offset += idx + marker.len();
            if offset >= haystack.len() {
                break;
            }
        }
    }
    None
}

/// Extract the first OpenRouter key-limit URL, trimming trailing punctuation.
fn extract_key_url(error: &str) -> Option<String> {
    let marker = "https://openrouter.ai/workspaces/";
    let idx = error.find(marker)?;
    let rest = &error[idx..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == '>')
        .unwrap_or(rest.len());
    let mut url = rest[..end]
        .trim_end_matches(['.', ',', ';', ':'])
        .to_string();
    // ACP sometimes glues the next sentence directly after the URL without a
    // space (see the original screenshot: "...limitOpenRouter paused...").
    // Cut at the first sentence-case "OpenRouter" that follows the key id.
    if let Some(glue) = url.find("OpenRouter") {
        url.truncate(glue);
        url = url.trim_end_matches(['.', ',', ';', ':']).to_string();
    }
    if url.len() > marker.len() {
        Some(url)
    } else {
        None
    }
}

/// Parse "retry after 12s" / "Retry-After: 12" style hints.
fn parse_retry_after(error: &str) -> Option<u64> {
    find_number_after(error, &["retry after", "retry-after", "retry in"])
}

/// Probe outcome for the OpenRouter key endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Balance looks usable again.
    Available,
    /// Still exhausted (401/402 or usage at limit).
    Exhausted,
    /// Transient network/parse failure. Keep waiting.
    Unknown,
}

/// Cheap probe: GET the OpenRouter key endpoint with the same bearer key.
///
/// This spends no tokens. `limit: null` means unlimited. Otherwise
/// `usage < limit` means credits are back.
pub async fn probe_openrouter_key(api_key: &str) -> ProbeOutcome {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build();
    let client = match client {
        Ok(client) => client,
        Err(_) => return ProbeOutcome::Unknown,
    };
    let response = client.get(KEY_INFO_URL).bearer_auth(api_key).send().await;
    let response = match response {
        Ok(response) => response,
        Err(_) => return ProbeOutcome::Unknown,
    };
    let status = response.status().as_u16();
    if status == 401 || status == 402 {
        return ProbeOutcome::Exhausted;
    }
    if !response.status().is_success() {
        return ProbeOutcome::Unknown;
    }
    let body: serde_json::Value = match response.json().await {
        Ok(body) => body,
        Err(_) => return ProbeOutcome::Unknown,
    };
    let data = &body["data"];
    // Some deployments return the shape flat instead of nested.
    let limit = data["limit"].as_f64().or_else(|| body["limit"].as_f64());
    let usage = data["usage"]
        .as_f64()
        .or_else(|| body["usage"].as_f64())
        .unwrap_or(f64::NAN);
    match limit {
        None => ProbeOutcome::Available,
        Some(limit) => {
            if usage.is_nan() {
                ProbeOutcome::Unknown
            } else if usage < limit {
                ProbeOutcome::Available
            } else {
                ProbeOutcome::Exhausted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREENSHOT_ERROR: &str = "The AI provider account is out of credits for this request: it asked for up to 65536 output tokens, but the balance covers 53197. Add credits or lower the max output tokens to continue.turn error: OpenRouter (402 Payment Required): This request requires more credits, or fewer max_tokens. You requested up to 65536 tokens, but can only afford 53197. To increase, visit https://openrouter.ai/workspaces/default/keys/490a6ef336b30e2bdc5b2b32343982eed7d96396e2f23d2819fd348c40e9809 and adjust the key's daily limitOpenRouter paused this request: it would exceed your available credits while other requests are still in flight. Wait for those to settle, or retry, or add credits at OpenRouter credits to raise the limit.";

    #[test]
    fn detects_screenshot_error_with_counts_and_key_url() {
        let exhaustion = CreditExhaustion::detect(SCREENSHOT_ERROR).expect("must detect");
        assert_eq!(exhaustion.requested_tokens, Some(65536));
        assert_eq!(exhaustion.affordable_tokens, Some(53197));
        assert!(exhaustion.in_flight);
        let key_url = exhaustion.key_url.expect("must extract key url");
        assert!(key_url.starts_with("https://openrouter.ai/workspaces/"));
        assert!(!key_url.contains("OpenRouter paused"));
    }

    #[test]
    fn markdown_block_keeps_facts_with_clickable_links_and_separators() {
        let exhaustion = CreditExhaustion::detect(SCREENSHOT_ERROR).expect("must detect");
        let block = exhaustion.markdown_block("OpenRouter (402 Payment Required): raw excerpt");
        assert!(block.contains("65536"));
        assert!(block.contains("53197"));
        assert!(block.contains("[OpenRouter credits](https://openrouter.ai/settings/credits)"));
        assert!(block.contains("[key's daily limit]("));
        assert!(
            block.contains("\n\n"),
            "paragraphs need blank-line separators"
        );
        assert!(
            !block.contains("continue.turn error:"),
            "must not glue sentences together"
        );
        assert!(
            !block.contains("limitOpenRouter"),
            "must not glue the key URL to the next sentence"
        );
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(CreditExhaustion::detect("boom: connection reset").is_none());
        assert!(CreditExhaustion::detect("OpenRouter (500 Internal Error): overloaded").is_none());
    }

    #[test]
    fn watcher_is_bounded_and_deterministic() {
        let mut watcher = CreditWatcher::start();
        assert_eq!(watcher.poll_interval, Duration::from_secs(60));
        for _ in 0..DEFAULT_MAX_PROBES - 1 {
            assert!(watcher.record_probe());
        }
        assert!(!watcher.record_probe());
    }
}

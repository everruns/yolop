//! Credit-exhaustion detection and readable formatting, plus notices for the
//! engine's credit wait.
//!
//! The screenshot that motivated this shows three texts merged into one
//! paragraph: an upstream credit rewrite, the raw OpenRouter 402 message, and
//! the in-flight hint, with a bare key URL. Paseo via ACP concatenates
//! successive message chunks, so the fix is to emit one markdown block with
//! blank-line separated paragraphs and markdown links. No information is
//! dropped: requested/affordable token counts, both URLs, retry guidance, and
//! a short raw excerpt are all preserved.
//!
//!
//! Waiting is the engine's: a refused turn schedules a check every minute for
//! up to an hour (`turn.credit_shortfall` events) and continues the turn when
//! credits are back. These notices say so.

use everruns_core::events::{CreditShortfallAction, TurnCreditShortfallData};

/// Public OpenRouter credits page. Stable, safe to link always.
pub const CREDITS_URL: &str = "https://openrouter.ai/settings/credits";
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
        let mut block = String::from("**Out of credits: turn paused.**\n\n");
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
        let trimmed = raw_excerpt.trim();
        if !trimmed.is_empty() {
            let excerpt: String = trimmed.chars().take(500).collect();
            block.push_str(&format!("Details: {excerpt}"));
        }
        block
    }
}

/// What to tell the person about one engine credit decision, if anything.
pub fn shortfall_notice(data: &TurnCreditShortfallData) -> Option<String> {
    let minutes = |at: Option<chrono::DateTime<chrono::Utc>>| {
        at.map(|at| (at - chrono::Utc::now()).num_minutes().max(1))
    };
    match data.action {
        CreditShortfallAction::Shrunk => data.max_tokens.map(|tokens| {
            format!("Low on credits: retrying with at most {tokens} output tokens.")
        }),
        CreditShortfallAction::WaitStarted => Some(match minutes(data.wait_until) {
            Some(minutes) => format!(
                "Out of credits. Checking every minute for the next {minutes} minutes and continuing on its own when credits are back; add credits at [OpenRouter credits]({CREDITS_URL}) or send a message to retry now."
            ),
            None => "Out of credits. Checking every minute and continuing on its own when credits are back.".to_string(),
        }),
        CreditShortfallAction::StillWaiting => None,
        CreditShortfallAction::Resumed => Some("Credits are back: continuing the paused work.".to_string()),
        CreditShortfallAction::GaveUp => Some(
            "Still out of credits after waiting; stopped checking. Add credits, then send a message to retry.".to_string(),
        ),
    }
}

fn parse_requested_tokens(error: &str) -> Option<u64> {
    for needle in ["up to ", "upto "] {
        let mut search = error;
        while let Some(idx) = search.to_lowercase().find(needle) {
            let rest = &search[idx + needle.len()..];
            let raw: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .collect();
            // Thousands separators (65,536) must not truncate the count at
            // the comma: strip them before parsing.
            let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
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
    fn parses_thousands_separated_counts() {
        let error = "OpenRouter (402 Payment Required): This request requires more credits, or fewer max_tokens. You requested up to 65,536 tokens, but can only afford 53,197.";
        let exhaustion = CreditExhaustion::detect(error).expect("must detect");
        assert_eq!(exhaustion.requested_tokens, Some(65536));
        assert_eq!(exhaustion.affordable_tokens, Some(53197));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(CreditExhaustion::detect("boom: connection reset").is_none());
        assert!(CreditExhaustion::detect("OpenRouter (500 Internal Error): overloaded").is_none());
    }

    #[test]
    fn shortfall_notices_follow_the_engine_wait() {
        let data = |action, wait_until| TurnCreditShortfallData {
            turn_id: everruns_contracts::typed_id::TurnId::new(),
            action,
            requested_max_tokens: None,
            affordable_tokens: None,
            max_tokens: Some(4096),
            wait_until,
            next_check_at: None,
            balance_checked: None,
        };
        let started = shortfall_notice(&data(
            CreditShortfallAction::WaitStarted,
            Some(chrono::Utc::now() + chrono::Duration::minutes(60)),
        ))
        .unwrap();
        assert!(started.contains("Checking every minute"), "{started}");
        assert!(started.contains(CREDITS_URL));
        assert!(
            shortfall_notice(&data(CreditShortfallAction::StillWaiting, None)).is_none(),
            "each quiet check stays out of the transcript"
        );
        assert!(
            shortfall_notice(&data(CreditShortfallAction::Shrunk, None))
                .unwrap()
                .contains("4096")
        );
        assert!(
            shortfall_notice(&data(CreditShortfallAction::GaveUp, None))
                .unwrap()
                .contains("stopped checking")
        );
    }
}

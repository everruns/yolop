//! The detection logic, kept free of I/O so it can be driven by unit tests
//! against synthetic `trace/event` payloads.

use std::collections::HashMap;
use yolop_yep::TraceEventParams;

/// Default drop threshold, in tokens. The one tunable: how much reuse has to
/// vanish between turns before it is worth telling the user about. Small
/// wobbles are normal (a tool result lands mid-prefix, the provider rounds a
/// block boundary); a break costs the whole prefix, so it shows up as a large
/// absolute fall.
pub const DEFAULT_DROP_TOKENS: u64 = 4096;

/// What the detector concluded about one turn-entry generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Reuse fell by at least the configured threshold versus the previous
    /// turn on this model.
    Break {
        model: String,
        previous: u64,
        current: u64,
    },
    /// Nothing was reused at all — the whole prompt was billed as fresh input.
    NoReuse { model: String },
    /// Reuse held up; the status field should go back to normal.
    Healthy { model: String, current: u64 },
}

impl Notice {
    /// Short text for the host status bar.
    pub fn status(&self) -> String {
        match self {
            Notice::Break {
                model,
                previous,
                current,
            } => format!(
                "cache break on {model} ({} → {})",
                tokens(*previous),
                tokens(*current)
            ),
            Notice::NoReuse { model } => format!("no cache reuse on {model}"),
            // An empty status clears the extension's status-bar field, which is
            // what a recovered turn should do — the warning is about the break,
            // not a permanent readout.
            Notice::Healthy { .. } => String::new(),
        }
    }

    /// Long form for `/extensions list`, and the text mirrored into the host
    /// log so a break is still recoverable from `RUST_LOG` in `--print`/ACP,
    /// where there is no status bar.
    pub fn detail(&self) -> String {
        match self {
            Notice::Break {
                model,
                previous,
                current,
            } => format!(
                "Prompt-cache reuse on {model} fell from {previous} to {current} tokens \
                 between turn entries; the rest of this turn is billed as fresh input."
            ),
            Notice::NoReuse { model } => format!(
                "No prompt-cache tokens were reused on {model}: the whole prompt was \
                 billed as fresh input."
            ),
            Notice::Healthy { model, current } => {
                format!("Prompt-cache reuse on {model} recovered to {current} tokens.")
            }
        }
    }

    /// Whether this notice is worth the user's attention (a break or a total
    /// miss) as opposed to a recovery.
    pub fn is_warning(&self) -> bool {
        !matches!(self, Notice::Healthy { .. })
    }
}

fn tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Per-model cache-reuse baseline, sampled once per turn.
///
/// Only the *first* `llm.generation` of each turn is compared. Later
/// generations in the same turn (tool-call continuations) legitimately differ
/// in reuse as the transcript grows, so including them would report a break on
/// every multi-step turn. `context.turn_id` is the gate: the first generation
/// carrying a turn id not yet seen is that turn's entry sample.
///
/// Sub-agent generations never reach here — they run in a child session, and
/// the host forwards trace events filtered to this session only.
pub struct Detector {
    drop_tokens: u64,
    /// Last turn-entry `cache_read_tokens`, per model. Keyed by model because a
    /// model switch mid-session shares no prefix, and comparing across models
    /// would report a break that only reflects the switch.
    per_model: HashMap<String, u64>,
    /// The turn whose entry sample has already been taken.
    current_turn: Option<String>,
    /// Whether the last notice was a warning, so a recovery is reported once
    /// rather than on every healthy turn.
    warned: bool,
}

impl Detector {
    pub fn new(drop_tokens: u64) -> Self {
        Self {
            drop_tokens,
            per_model: HashMap::new(),
            current_turn: None,
            warned: false,
        }
    }

    /// Feed one forwarded trace event. Returns a notice when the status bar
    /// should change.
    pub fn observe(&mut self, event: &TraceEventParams) -> Option<Notice> {
        if event.event_type != "llm.generation" {
            return None;
        }

        // Unknown is not zero: a generation with no turn id cannot be placed in
        // the turn sequence, so it is skipped rather than guessed at.
        let turn_id = event.context.get("turn_id").and_then(|v| v.as_str())?;
        if self.current_turn.as_deref() == Some(turn_id) {
            return None;
        }
        // Consume the slot before checking the metrics, so an unusable entry
        // sample cannot let a mid-turn continuation stand in for it.
        self.current_turn = Some(turn_id.to_string());

        let metadata = event.data.get("metadata")?;
        let model = metadata
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        // `cache_read_tokens` is skipped when the provider reports no cache
        // data at all. Absent means "no data" — leave the baseline alone.
        let current = metadata
            .get("usage")
            .and_then(|u| u.get("cache_read_tokens"))
            .and_then(|v| v.as_u64())?;

        let previous = self.per_model.insert(model.clone(), current);

        let notice = match previous {
            Some(previous) if previous.saturating_sub(current) >= self.drop_tokens => {
                Notice::Break {
                    model,
                    previous,
                    current,
                }
            }
            _ if current == 0 => Notice::NoReuse { model },
            _ => Notice::Healthy { model, current },
        };

        // Report every warning (the baseline self-heals, so a break is reported
        // once), but a recovery only when it clears a warning that is still up.
        match (notice.is_warning(), self.warned) {
            (true, _) => {
                self.warned = true;
                Some(notice)
            }
            (false, true) => {
                self.warned = false;
                Some(notice)
            }
            (false, false) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn generation(turn: &str, model: &str, cache_read: Option<u64>) -> TraceEventParams {
        let usage = match cache_read {
            Some(tokens) => json!({ "input_tokens": 100, "cache_read_tokens": tokens }),
            None => json!({ "input_tokens": 100 }),
        };
        TraceEventParams {
            event_type: "llm.generation".to_string(),
            context: json!({ "turn_id": turn }),
            data: json!({ "metadata": { "model": model, "usage": usage } }),
            ..Default::default()
        }
    }

    #[test]
    fn a_large_drop_between_turns_is_a_break() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        assert_eq!(
            detector.observe(&generation("t1", "gpt-5", Some(20_000))),
            None,
            "the first sample only establishes a baseline"
        );
        assert_eq!(
            detector.observe(&generation("t2", "gpt-5", Some(2_000))),
            Some(Notice::Break {
                model: "gpt-5".into(),
                previous: 20_000,
                current: 2_000
            })
        );
    }

    #[test]
    fn a_small_wobble_is_not_a_break() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        detector.observe(&generation("t1", "gpt-5", Some(20_000)));
        assert_eq!(
            detector.observe(&generation("t2", "gpt-5", Some(19_000))),
            None
        );
    }

    #[test]
    fn the_baseline_self_heals_so_a_break_is_reported_once() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        detector.observe(&generation("t1", "gpt-5", Some(20_000)));
        assert!(matches!(
            detector.observe(&generation("t2", "gpt-5", Some(2_000))),
            Some(Notice::Break { .. })
        ));
        // Reuse resumes from the new, lower baseline: one recovery notice that
        // clears the status field, then silence.
        assert!(matches!(
            detector.observe(&generation("t3", "gpt-5", Some(4_000))),
            Some(Notice::Healthy { .. })
        ));
        assert_eq!(
            detector.observe(&generation("t4", "gpt-5", Some(6_000))),
            None
        );
    }

    #[test]
    fn zero_reuse_is_reported_even_without_a_baseline() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        assert_eq!(
            detector.observe(&generation("t1", "gpt-5", Some(0))),
            Some(Notice::NoReuse {
                model: "gpt-5".into()
            })
        );
    }

    #[test]
    fn only_the_first_generation_of_a_turn_is_sampled() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        detector.observe(&generation("t1", "gpt-5", Some(20_000)));
        // Tool-call continuations within the same turn are ignored...
        assert_eq!(detector.observe(&generation("t1", "gpt-5", Some(0))), None);
        // ...and they do not move the baseline either.
        assert_eq!(
            detector.observe(&generation("t2", "gpt-5", Some(2_000))),
            Some(Notice::Break {
                model: "gpt-5".into(),
                previous: 20_000,
                current: 2_000
            })
        );
    }

    #[test]
    fn an_unusable_entry_sample_still_consumes_the_turn_slot() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        detector.observe(&generation("t1", "gpt-5", Some(20_000)));
        // Entry for t2 carries no cache counter: no data, so no verdict...
        assert_eq!(detector.observe(&generation("t2", "gpt-5", None)), None);
        // ...and a later call in t2 cannot masquerade as the entry sample.
        assert_eq!(detector.observe(&generation("t2", "gpt-5", Some(0))), None);
    }

    #[test]
    fn baselines_are_kept_per_model() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        detector.observe(&generation("t1", "gpt-5", Some(20_000)));
        // A different model shares no prefix; its first sample is a baseline,
        // not a break against gpt-5's.
        assert_eq!(
            detector.observe(&generation("t2", "claude-sonnet-4-5", Some(1_000))),
            None
        );
        assert_eq!(
            detector.observe(&generation("t3", "gpt-5", Some(1_000))),
            Some(Notice::Break {
                model: "gpt-5".into(),
                previous: 20_000,
                current: 1_000
            })
        );
    }

    #[test]
    fn other_event_types_and_missing_turn_ids_are_ignored() {
        let mut detector = Detector::new(DEFAULT_DROP_TOKENS);
        let mut tool = generation("t1", "gpt-5", Some(0));
        tool.event_type = "tool.completed".to_string();
        assert_eq!(detector.observe(&tool), None);

        let mut orphan = generation("t1", "gpt-5", Some(0));
        orphan.context = json!({});
        assert_eq!(detector.observe(&orphan), None);
    }

    #[test]
    fn the_threshold_is_configurable() {
        let mut detector = Detector::new(500);
        detector.observe(&generation("t1", "gpt-5", Some(20_000)));
        assert!(matches!(
            detector.observe(&generation("t2", "gpt-5", Some(19_000))),
            Some(Notice::Break { .. })
        ));
    }
}

//! Muse-only guard against idle actionable replies.
//!
//! Decision: the model says it will verify, check, or act, but the turn ends
//! with zero tool calls, so the host records the turn done and nothing runs.
//! A Classifier (Jev, TypeSafe backend) judges the final text instead of a
//! brittle phrase list. The check runs only when the sync gate says Achieved
//! with zero tools on a Muse session, and any miss, error, or absent key
//! keeps Achieved (fail open). A hit rewrites the verdict to InProgress with
//! a continuation nudge, bounded by the existing continuation budget.
use std::sync::Arc;

use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::classifier::{ClassificationQuestion, ClassificationRequest, ClassifierService};
use serde_json::json;

pub const ACTION_GUARD_CAPABILITY_ID: &str = "action-guard";
pub const ACTION_GUARD_QUESTION_ID: &str = "actionable_promise";

/// Minimum yes probability that counts as a promised action.
pub const ACTION_GUARD_THRESHOLD: f64 = 0.7;

/// True for Meta Muse model ids (`muse-spark-1.2`, `muse-spark-1.3`, ...),
/// whatever provider routes them. Never matches Claude or other families.
pub fn is_muse(model: Option<&str>) -> bool {
    model
        .map(|name| name.to_lowercase().contains("muse"))
        .unwrap_or(false)
}

pub struct ActionGuardCapability;

impl ActionGuardCapability {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ActionGuardCapability {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Capability for ActionGuardCapability {
    fn id(&self) -> &str {
        ACTION_GUARD_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Action guard"
    }

    fn description(&self) -> &str {
        "Catches idle actionable replies on Muse sessions: the model promises to verify or act but ends the turn with no tool call."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Guardrails")
    }

    fn is_guardrail(&self) -> bool {
        true
    }

    async fn system_prompt_contribution(&self, ctx: &SystemPromptContext) -> Option<String> {
        // The reason path carries the session model; the act path leaves it
        // empty. Contribute only where the reader is Muse.
        let model = ctx.model.as_deref();
        if !is_muse(model) {
            return None;
        }
        Some(
            "If you say you will verify, check, or act, call the tool in the same response. \
             Never end your turn with an unfulfilled promise."
                .to_string(),
        )
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some("Muse only: promised actions must call a tool in the same response.".to_string())
    }
}

/// Asks the Classifier whether `response` promises an action while no tool
/// ran. Returns false on an unconfigured service, an error, or a missing
/// answer, so the turn keeps its Achieved verdict (fail open).
pub async fn evaluate_actionable_promise(
    response: &str,
    tool_calls_count: usize,
    classifier: &Arc<dyn ClassifierService>,
    classifier_model: Option<&str>,
) -> bool {
    if !classifier.is_configured() {
        return false;
    }
    let mut request = ClassificationRequest::new(json!({
        "response": response,
        "tool_calls_count": tool_calls_count,
    }))
    .ask(
        ACTION_GUARD_QUESTION_ID,
        ClassificationQuestion::noul(
            "Does this assistant response promise to verify, check, look something up, \
             or otherwise act before answering?",
        ),
    )
    .with_metadata("purpose", "action_guard")
    .with_metadata("capability", ACTION_GUARD_CAPABILITY_ID);
    if let Some(model) = classifier_model {
        request = request.model(model);
    }
    let outcome = match classifier.evaluate(request).await {
        Ok(outcome) => outcome,
        Err(_) => return false,
    };
    outcome
        .get(ACTION_GUARD_QUESTION_ID)
        .and_then(|answer| answer.probability_yes())
        .map(|probability| probability >= ACTION_GUARD_THRESHOLD)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::classifier::{
        ClassificationAnswer, ClassificationOutcome, ClassificationUsage,
    };
    use std::collections::BTreeMap;

    /// Canned ClassifierService for unit tests.
    struct FixedClassifier {
        configured: bool,
        probability: Option<f64>,
    }

    #[async_trait::async_trait]
    impl ClassifierService for FixedClassifier {
        fn is_configured(&self) -> bool {
            self.configured
        }

        async fn evaluate(
            &self,
            _request: ClassificationRequest,
        ) -> everruns_provider::error::Result<ClassificationOutcome> {
            let mut answers = BTreeMap::new();
            if let Some(probability) = self.probability {
                answers.insert(
                    ACTION_GUARD_QUESTION_ID.to_string(),
                    ClassificationAnswer::Noul { probability },
                );
            }
            Ok(ClassificationOutcome {
                model: "stub".to_string(),
                answers,
                usage: ClassificationUsage::default(),
            })
        }
    }

    fn configured(probability: Option<f64>) -> Arc<dyn ClassifierService> {
        Arc::new(FixedClassifier {
            configured: true,
            probability,
        })
    }

    fn unconfigured() -> Arc<dyn ClassifierService> {
        Arc::new(FixedClassifier {
            configured: false,
            probability: Some(1.0),
        })
    }

    #[test]
    fn detects_muse_model_ids() {
        assert!(is_muse(Some("meta/muse-spark-1.3-contributor")));
        assert!(is_muse(Some("muse-spark-1.2")));
        assert!(is_muse(Some("MUSE-Spark")));
        assert!(!is_muse(Some("anthropic/claude-opus-4-8")));
        assert!(!is_muse(Some("openai/gpt-5")));
        assert!(!is_muse(None));
    }

    #[tokio::test]
    async fn hit_above_threshold_returns_true() {
        let classifier = configured(Some(0.85));
        assert!(
            evaluate_actionable_promise(
                "Good question. Let me verify the exact split before answering.",
                0,
                &classifier,
                None
            )
            .await
        );
    }

    #[tokio::test]
    async fn miss_below_threshold_returns_false() {
        let classifier = configured(Some(0.2));
        assert!(!evaluate_actionable_promise("The split is X.", 0, &classifier, None).await);
    }

    #[tokio::test]
    async fn unconfigured_service_fails_open() {
        assert!(
            !evaluate_actionable_promise(
                "Good question. Let me verify the exact split before answering.",
                0,
                &unconfigured(),
                None
            )
            .await
        );
    }

    #[tokio::test]
    async fn missing_answer_fails_open() {
        let classifier = configured(None);
        assert!(!evaluate_actionable_promise("Let me check the repo.", 0, &classifier, None).await);
    }
}

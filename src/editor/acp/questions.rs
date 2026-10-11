//! Structured questions (`ask_user`, and the coordination `ask_decision` it
//! carries) asked of the editor through ACP form elicitation.
//!
//! Decision: questions travel as `elicitation/create` in form mode, one form
//! per batch with one field per question, and only to a client that advertised
//! form elicitation in `initialize`. Without it they decline as before, never
//! disguised as a permission request (a fixed allow/reject list cannot carry
//! the options) and never left pending on a second prompt.
//! Decision: a secret question declines the whole batch. An editor form is not
//! encrypted credential storage, the same rule the TUI applies.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationSchema, ElicitationSessionScope, EnumOption, MultiSelectPropertySchema,
    StringPropertySchema,
};
use agent_client_protocol::{Client, ConnectionTo};
use async_trait::async_trait;
use everruns_core::builtins::ask_user::{
    AskContext, AskUser, AskUserAnswer, AskUserAnsweredBy, AskUserQuestion, AskUserQuestionKind,
    AskUserResult, AskUserStatus,
};

/// Suffix of the free-text field offered beside a choice that allows another
/// answer.
const OTHER_SUFFIX: &str = "_other";

/// Asks the editor through form elicitation when it supports it.
pub(super) struct AcpQuestioner {
    pub(super) cx: ConnectionTo<Client>,
    /// Set from `initialize`; shared with the server so a later handshake
    /// detail never leaves sessions with a stale answer.
    pub(super) forms: Arc<AtomicBool>,
}

#[async_trait]
impl AskUser for AcpQuestioner {
    async fn ask(&self, _questions: &[AskUserQuestion]) -> AskUserResult {
        // Elicitation is scoped to a session; the capability always supplies
        // one through `ask_in`.
        stopped(AskUserStatus::Declined, AskUserAnsweredBy::Unattended)
    }

    async fn ask_in(&self, context: &AskContext, questions: &[AskUserQuestion]) -> AskUserResult {
        if !self.forms.load(Ordering::Acquire)
            || questions
                .iter()
                .any(|q| q.kind == AskUserQuestionKind::Secret)
        {
            return stopped(AskUserStatus::Declined, AskUserAnsweredBy::Unattended);
        }
        let scope = ElicitationSessionScope::new(context.session_id().to_string()).tool_call_id(
            agent_client_protocol::schema::v1::ToolCallId::new(context.tool_call_id().to_string()),
        );
        let request = CreateElicitationRequest::new(
            ElicitationFormMode::new(scope, form_schema(questions)),
            form_message(questions),
        );
        match self.cx.send_request(request).block_task().await {
            Ok(response) => answers_from(questions, response.action),
            Err(error) => {
                tracing::warn!(%error, "acp: elicitation failed; declining questions");
                stopped(AskUserStatus::Declined, AskUserAnsweredBy::Unattended)
            }
        }
    }
}

fn field_key(index: usize, question: &AskUserQuestion) -> String {
    question
        .id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| format!("q{}", index + 1))
}

fn form_message(questions: &[AskUserQuestion]) -> String {
    match questions {
        [only] => only.question.clone(),
        _ => "Answer these questions to continue.".to_string(),
    }
}

pub(super) fn form_schema(questions: &[AskUserQuestion]) -> ElicitationSchema {
    let mut schema = ElicitationSchema::new();
    for (index, question) in questions.iter().enumerate() {
        let key = field_key(index, question);
        let options: Vec<EnumOption> = question
            .options
            .iter()
            .map(|option| {
                EnumOption::new(option.label.clone(), option.label.clone())
                    .description(option.description.clone())
            })
            .collect();
        let defaults: Vec<String> = question
            .options
            .iter()
            .filter(|option| option.is_default)
            .map(|option| option.label.clone())
            .collect();
        let required = !question.allow_other;
        schema = match question.kind {
            AskUserQuestionKind::Choice if question.multi_select => schema.property(
                key.clone(),
                MultiSelectPropertySchema::titled(options)
                    .title(question.header.clone())
                    .description(question.question.clone())
                    .default_value((!defaults.is_empty()).then_some(defaults)),
                required,
            ),
            AskUserQuestionKind::Choice => schema.property(
                key.clone(),
                StringPropertySchema::new()
                    .title(question.header.clone())
                    .description(question.question.clone())
                    .one_of(options)
                    .default_value(defaults.into_iter().next()),
                required,
            ),
            _ => schema.property(
                key.clone(),
                StringPropertySchema::new()
                    .title(question.header.clone())
                    .description(question.question.clone())
                    .min_length(1),
                true,
            ),
        };
        if question.kind == AskUserQuestionKind::Choice && question.allow_other {
            schema = schema.property(
                format!("{key}{OTHER_SUFFIX}"),
                StringPropertySchema::new()
                    .title(format!("{}: another answer", question.header))
                    .description("Leave empty to use the choice above."),
                false,
            );
        }
    }
    schema
}

/// Turn the editor's answer into the tool result. A form that does not answer
/// every question with a listed option (or another answer, where allowed)
/// counts as declined rather than guessed at.
pub(super) fn answers_from(
    questions: &[AskUserQuestion],
    action: ElicitationAction,
) -> AskUserResult {
    let content = match action {
        ElicitationAction::Accept(accept) => accept.content.unwrap_or_default(),
        ElicitationAction::Cancel => {
            return stopped(AskUserStatus::Cancelled, AskUserAnsweredBy::User);
        }
        _ => return stopped(AskUserStatus::Declined, AskUserAnsweredBy::User),
    };
    let mut answers = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        let key = field_key(index, question);
        match answer(question, &key, &content) {
            Some(answer) => answers.push(answer),
            None => return stopped(AskUserStatus::Declined, AskUserAnsweredBy::User),
        }
    }
    AskUserResult {
        status: AskUserStatus::Answered,
        answered_by: AskUserAnsweredBy::User,
        answers,
    }
}

fn answer(
    question: &AskUserQuestion,
    key: &str,
    content: &BTreeMap<String, ElicitationContentValue>,
) -> Option<AskUserAnswer> {
    let text = |key: &str| match content.get(key) {
        Some(ElicitationContentValue::String(text)) if !text.trim().is_empty() => {
            Some(text.trim().to_string())
        }
        _ => None,
    };
    let id = question.id.clone().unwrap_or_default();
    if question.kind == AskUserQuestionKind::Text {
        return Some(AskUserAnswer {
            id,
            selected: Vec::new(),
            other_text: Some(text(key)?),
            secret_ref: None,
        });
    }
    let other_text = if question.allow_other {
        text(&format!("{key}{OTHER_SUFFIX}"))
    } else {
        None
    };
    let listed = |label: &String| question.options.iter().any(|o| &o.label == label);
    let selected: Vec<String> = match content.get(key) {
        Some(ElicitationContentValue::String(label)) if listed(label) => vec![label.clone()],
        Some(ElicitationContentValue::StringArray(labels)) if labels.iter().all(listed) => {
            let mut unique = Vec::new();
            for label in labels {
                if !unique.contains(label) {
                    unique.push(label.clone());
                }
            }
            unique
        }
        None => Vec::new(),
        Some(ElicitationContentValue::String(empty)) if empty.is_empty() => Vec::new(),
        Some(_) => return None,
    };
    if (selected.is_empty() && other_text.is_none())
        || (!question.multi_select && selected.len() > 1)
    {
        return None;
    }
    Some(AskUserAnswer {
        id,
        selected,
        other_text,
        secret_ref: None,
    })
}

fn stopped(status: AskUserStatus, answered_by: AskUserAnsweredBy) -> AskUserResult {
    AskUserResult {
        status,
        answered_by,
        answers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ElicitationAcceptAction;

    fn question(value: serde_json::Value) -> AskUserQuestion {
        serde_json::from_value(value).unwrap()
    }

    fn decision() -> AskUserQuestion {
        question(serde_json::json!({
            "kind": "choice", "id": "deploy", "header": "Deploy", "question": "Ship it now?",
            "allow_other": false,
            "options": [
                {"label": "Ship", "description": "deploy now", "default": true},
                {"label": "Wait", "description": "hold until Monday"}
            ]
        }))
    }

    fn accept(content: serde_json::Value) -> ElicitationAction {
        ElicitationAction::Accept(
            ElicitationAcceptAction::new()
                .content(serde_json::from_value::<BTreeMap<_, _>>(content).unwrap()),
        )
    }

    #[test]
    fn a_decision_becomes_a_single_select_field_with_its_options() {
        let schema = serde_json::to_value(form_schema(&[decision()])).unwrap();
        let field = &schema["properties"]["deploy"];
        assert_eq!(field["type"], "string");
        assert_eq!(field["title"], "Deploy");
        assert_eq!(field["default"], "Ship");
        assert_eq!(field["oneOf"][1]["const"], "Wait");
        assert_eq!(field["oneOf"][1]["description"], "hold until Monday");
        assert_eq!(schema["required"], serde_json::json!(["deploy"]));
    }

    #[test]
    fn an_answered_form_returns_the_chosen_option() {
        let result = answers_from(&[decision()], accept(serde_json::json!({"deploy": "Wait"})));
        assert_eq!(result.status, AskUserStatus::Answered);
        assert_eq!(result.answered_by, AskUserAnsweredBy::User);
        assert_eq!(result.answers[0].id, "deploy");
        assert_eq!(result.answers[0].selected, vec!["Wait"]);
    }

    #[test]
    fn an_unlisted_or_missing_answer_declines_instead_of_guessing() {
        for content in [
            serde_json::json!({"deploy": "Rollback"}),
            serde_json::json!({}),
            serde_json::json!({"deploy": ["Ship", "Wait"]}),
        ] {
            let result = answers_from(&[decision()], accept(content));
            assert_eq!(result.status, AskUserStatus::Declined);
        }
    }

    #[test]
    fn another_answer_and_text_questions_round_trip() {
        let choice = question(serde_json::json!({
            "kind": "choice", "id": "db", "header": "Database", "question": "Which one?",
            "allow_other": true, "multi_select": true,
            "options": [{"label": "Postgres", "description": ""}, {"label": "SQLite", "description": ""}]
        }));
        let text = question(serde_json::json!({
            "kind": "text", "header": "Name", "question": "Branch name?", "options": []
        }));
        let schema = serde_json::to_value(form_schema(&[choice.clone(), text.clone()])).unwrap();
        assert_eq!(schema["properties"]["db"]["type"], "array");
        assert_eq!(schema["properties"]["db_other"]["type"], "string");
        assert_eq!(schema["properties"]["q2"]["type"], "string");

        let result = answers_from(
            &[choice, text],
            accept(serde_json::json!({"db": [], "db_other": "DuckDB", "q2": "fix-parser"})),
        );
        assert_eq!(result.status, AskUserStatus::Answered);
        assert_eq!(result.answers[0].other_text.as_deref(), Some("DuckDB"));
        assert_eq!(result.answers[1].other_text.as_deref(), Some("fix-parser"));
    }

    #[test]
    fn declining_or_cancelling_the_form_is_reported_as_such() {
        let declined = answers_from(&[decision()], ElicitationAction::Decline);
        assert_eq!(declined.status, AskUserStatus::Declined);
        let cancelled = answers_from(&[decision()], ElicitationAction::Cancel);
        assert_eq!(cancelled.status, AskUserStatus::Cancelled);
    }
}

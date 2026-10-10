//! Structured questions share the TUI's host prompt port. Other hosts resolve
//! immediately with defaults or a decline, never a pending client call.
use crate::tui::host_ui::AskRequest;
use async_trait::async_trait;
use everruns_core::builtins::ask_user::{
    AskUser, AskUserAnswer, AskUserAnsweredBy, AskUserQuestion, AskUserQuestionKind, AskUserResult,
    AskUserStatus, DefaultsResponder,
};
use tokio::sync::mpsc;

pub struct HostResponder(pub Option<mpsc::UnboundedSender<AskRequest>>);

#[async_trait]
impl AskUser for HostResponder {
    async fn ask(&self, questions: &[AskUserQuestion]) -> AskUserResult {
        let Some(sink) = &self.0 else {
            return DefaultsResponder.ask(questions).await;
        };
        // A masked composer is not encrypted credential storage. Decline the
        // whole batch before soliciting anything that could enter model context.
        if questions
            .iter()
            .any(|q| q.kind == AskUserQuestionKind::Secret)
        {
            return stopped(AskUserStatus::Declined);
        }
        let mut answers = Vec::new();
        for q in questions {
            let selector =
                q.kind == AskUserQuestionKind::Choice && !q.multi_select && !q.allow_other;
            let options = if selector {
                q.options.iter().map(|o| o.label.clone()).collect()
            } else {
                vec![]
            };
            let mut prompt = format!("{}: {}", q.header, q.question);
            for (i, option) in q.options.iter().enumerate() {
                prompt.push_str(&format!(
                    "\n{}: {} ({}){}",
                    i + 1,
                    option.label,
                    option.description,
                    if option.is_default { " [default]" } else { "" }
                ));
            }
            if !selector && q.kind == AskUserQuestionKind::Choice {
                prompt.push_str("\nEnter choice numbers separated by commas.");
                if q.allow_other {
                    prompt.push_str(" For another answer, enter other: followed by text.");
                }
            }
            loop {
                let (reply, receive) = tokio::sync::oneshot::channel();
                if sink
                    .send(AskRequest {
                        prompt: prompt.clone(),
                        placeholder: None,
                        secret: false,
                        options: options.clone(),
                        reply,
                    })
                    .is_err()
                {
                    return stopped(AskUserStatus::Cancelled);
                }
                let Ok(answer) = receive.await else {
                    return stopped(AskUserStatus::Cancelled);
                };
                if answer.cancelled {
                    return stopped(AskUserStatus::Cancelled);
                }
                if let Some(answer) = parse_answer(q, selector, &answer.answer) {
                    answers.push(answer);
                    break;
                }
                prompt = prompt.trim_end_matches("\nInvalid answer. Enter a nonempty answer in the requested format or cancel.").to_owned();
                prompt.push_str(
                    "\nInvalid answer. Enter a nonempty answer in the requested format or cancel.",
                );
            }
        }
        AskUserResult {
            status: AskUserStatus::Answered,
            answered_by: AskUserAnsweredBy::User,
            answers,
        }
    }
}

fn parse_answer(q: &AskUserQuestion, selector: bool, input: &str) -> Option<AskUserAnswer> {
    let mut selected = Vec::new();
    let mut other_text = None;
    if q.kind == AskUserQuestionKind::Text {
        if input.trim().is_empty() {
            return None;
        }
        other_text = Some(input.to_owned());
    } else if q.allow_other && input.starts_with("other:") {
        let text = input[6..].trim();
        if text.is_empty() {
            return None;
        }
        other_text = Some(text.to_owned());
    } else if selector {
        selected.push(q.options.iter().find(|o| o.label == input)?.label.clone());
    } else {
        for index in input.split(',').map(str::trim) {
            let index = index.parse::<usize>().ok()?.checked_sub(1)?;
            let label = &q.options.get(index)?.label;
            if !selected.contains(label) {
                selected.push(label.clone());
            }
        }
        if selected.is_empty() || (!q.multi_select && selected.len() > 1) {
            return None;
        }
    }
    Some(AskUserAnswer {
        id: q.id.clone().unwrap_or_default(),
        selected,
        other_text,
        secret_ref: None,
    })
}

fn stopped(status: AskUserStatus) -> AskUserResult {
    AskUserResult {
        status,
        answered_by: AskUserAnsweredBy::User,
        answers: vec![],
    }
}

/// ACP has no negotiated structured-question request. Do not disguise questions
/// as permission requests or leave the turn blocked awaiting a second prompt.
pub struct AcpResponder;
#[async_trait]
impl AskUser for AcpResponder {
    async fn ask(&self, _questions: &[AskUserQuestion]) -> AskUserResult {
        AskUserResult {
            status: AskUserStatus::Declined,
            answered_by: AskUserAnsweredBy::Unattended,
            answers: vec![],
        }
    }
}

#[cfg(test)]
mod basic_tests {
    use super::*;
    use everruns_core::builtins::ask_user::AskUserQuestionKind;
    fn question(kind: AskUserQuestionKind) -> AskUserQuestion {
        serde_json::from_value(serde_json::json!({
            "kind": kind, "id": "q", "header": "Question", "question": "Choose", "allow_other": false,
            "options": [{"label":"Alpha", "description":"first"},{"label":"Beta", "description":"second"}]
        })).unwrap()
    }
    #[tokio::test]
    async fn ask_user_print_and_acp_defaults() {
        let questions = vec![question(AskUserQuestionKind::Choice)];
        let result = HostResponder(None).ask(&questions).await;
        assert_eq!(result.answers[0].selected, vec!["Alpha"]);
        assert_eq!(
            AcpResponder.ask(&questions).await.status,
            AskUserStatus::Declined
        );
    }
    #[tokio::test]
    async fn tui_preserves_choice_labels_and_cancellation() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let host = HostResponder(Some(tx));
        let questions = vec![question(AskUserQuestionKind::Choice)];
        let (result, ()) = tokio::join!(host.ask(&questions), async {
            let request = rx.recv().await.unwrap();
            assert_eq!(request.options, vec!["Alpha", "Beta"]);
            request
                .reply
                .send(crate::tui::host_ui::AskAnswer {
                    answer: "Beta".into(),
                    cancelled: false,
                })
                .unwrap();
        });
        assert_eq!(result.answers[0].selected, vec!["Beta"]);
        let (result, ()) = tokio::join!(host.ask(&questions), async {
            let request = rx.recv().await.unwrap();
            request
                .reply
                .send(crate::tui::host_ui::AskAnswer {
                    answer: String::new(),
                    cancelled: true,
                })
                .unwrap();
        });
        assert_eq!(result.status, AskUserStatus::Cancelled);
    }
    #[tokio::test]
    async fn secret_never_reaches_composer_and_closed_channel_cancels() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let host = HostResponder(Some(tx));
        assert_eq!(
            host.ask(&[question(AskUserQuestionKind::Secret)])
                .await
                .status,
            AskUserStatus::Declined
        );
        assert!(rx.try_recv().is_err());
        drop(rx);
        assert_eq!(
            host.ask(&[question(AskUserQuestionKind::Choice)])
                .await
                .status,
            AskUserStatus::Cancelled
        );
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;
    use crate::tui::host_ui::AskAnswer;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn choice() -> AskUserQuestion {
        serde_json::from_value(json!({
            "id": "q", "header": "Plan", "question": "Which plan?",
            "allow_other": false,
            "options": [
                {"label": "A", "description": "first"},
                {"label": "B", "description": "second", "default": true}
            ]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn ask_user_unattended_defaults_and_declines_text() {
        let host = HostResponder(None);
        let q = choice();
        let result = host.ask(std::slice::from_ref(&q)).await;
        assert_eq!(result.answered_by, AskUserAnsweredBy::Unattended);
        assert_eq!(result.answers[0].selected, ["B"]);
        let mut no_default = q.clone();
        no_default.options[1].is_default = false;
        assert_eq!(host.ask(&[no_default]).await.answers[0].selected, ["A"]);
        let mut text = q;
        text.kind = AskUserQuestionKind::Text;
        text.options.clear();
        assert_eq!(host.ask(&[text]).await.status, AskUserStatus::Declined);
    }

    #[test]
    fn ask_user_parses_multi_other_and_invalid_answers() {
        let mut q = choice();
        q.multi_select = true;
        q.allow_other = true;
        assert_eq!(
            parse_answer(&q, false, "2,1,2").unwrap().selected,
            ["B", "A"]
        );
        assert_eq!(
            parse_answer(&q, false, "other: custom")
                .unwrap()
                .other_text
                .as_deref(),
            Some("custom")
        );
        for input in ["0", "3", "", "1,", "other:", "wrong"] {
            assert!(parse_answer(&q, false, input).is_none(), "{input}");
        }
        q.multi_select = false;
        assert!(parse_answer(&q, false, "1,2").is_none());
        q.allow_other = false;
        assert!(parse_answer(&q, false, "other: custom").is_none());
        assert_eq!(parse_answer(&q, true, "B").unwrap().selected, ["B"]);
    }

    #[tokio::test]
    async fn ask_user_tui_sequential_questions_and_retry() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut text = choice();
        text.id = Some("text".into());
        text.kind = AskUserQuestionKind::Text;
        text.options.clear();
        let task =
            tokio::spawn(async move { HostResponder(Some(tx)).ask(&[choice(), text]).await });
        let first = rx.recv().await.unwrap();
        assert_eq!(first.options, ["A", "B"]);
        assert!(first.prompt.contains("second) [default]"));
        first
            .reply
            .send(AskAnswer {
                answer: "B".into(),
                cancelled: false,
            })
            .unwrap();
        let second = rx.recv().await.unwrap();
        assert!(second.options.is_empty());
        second.reply.send(AskAnswer::default()).unwrap();
        let retry = rx.recv().await.unwrap();
        retry
            .reply
            .send(AskAnswer {
                answer: "hello".into(),
                cancelled: false,
            })
            .unwrap();
        let result = task.await.unwrap();
        assert_eq!(result.status, AskUserStatus::Answered);
        assert_eq!(result.answered_by, AskUserAnsweredBy::User);
        assert_eq!(result.answers[0].id, "q");
        assert_eq!(result.answers[1].other_text.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn ask_user_cancellation_closed_channel_and_secret_are_safe() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host = HostResponder(Some(tx));
        let mut secret = choice();
        secret.kind = AskUserQuestionKind::Secret;
        assert_eq!(host.ask(&[secret]).await.status, AskUserStatus::Declined);
        assert!(rx.try_recv().is_err());
        let task = tokio::spawn(async move { host.ask(&[choice()]).await });
        rx.recv()
            .await
            .unwrap()
            .reply
            .send(AskAnswer {
                answer: String::new(),
                cancelled: true,
            })
            .unwrap();
        assert_eq!(task.await.unwrap().status, AskUserStatus::Cancelled);
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        assert_eq!(
            HostResponder(Some(tx)).ask(&[choice()]).await.status,
            AskUserStatus::Cancelled
        );
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use everruns_core::capabilities::Capability;
    #[tokio::test]
    async fn ask_user_timeout_drops_pending_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let capability = everruns_core::builtins::ask_user::AskUserCapability::new(Arc::new(
            HostResponder(Some(tx)),
        ));
        let tools = capability.tools();
        let result_future = tools[0].execute(serde_json::json!({
            "timeout_seconds": 1,
            "questions": [{"id":"q", "header":"Plan", "question":"Which?", "allow_other":false,
                "options":[{"label":"A", "description":"first"},{"label":"B", "description":"second"}]}]
        }));
        let (result, request) = tokio::join!(result_future, async { rx.recv().await.unwrap() });
        let serialized = format!("{result:?}");
        assert!(serialized.contains("timed_out"), "{serialized}");
        assert!(request.reply.is_closed());
    }
    use std::sync::Arc;
}

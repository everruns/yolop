//! Correlate frontend streams and cancellation with admitted inputs. A frontend
//! must not consume another independently running Engine turn's output.
use everruns_contracts::typed_id::{MessageId, TurnId};
use everruns_core::{Event, EventData, InputMessage};
use std::collections::HashSet;

pub(crate) const RUN_METADATA: &str = "yolop.frontend_run";

pub(crate) struct TurnScope {
    pub id: MessageId,
    cancellation: everruns::CancellationToken,
    admission: Option<MessageId>,
    inputs: HashSet<MessageId>,
    turns: HashSet<TurnId>,
}
impl TurnScope {
    pub fn new(input: &mut InputMessage) -> Self {
        let id = MessageId::new();
        let admission = input
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("yolop.observe_admission"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse().ok());
        input
            .metadata
            .get_or_insert_default()
            .insert(RUN_METADATA.into(), serde_json::json!(id));
        Self {
            id,
            cancellation: everruns::CancellationToken::new(),
            admission,
            inputs: admission.into_iter().collect(),
            turns: HashSet::new(),
        }
    }
    pub async fn accepts(&mut self, session: &everruns::Session, event: &Event) -> bool {
        if let EventData::InputMessage(input) = &event.data {
            let mine = self.admission == Some(input.message.id)
                || input
                    .message
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get(RUN_METADATA))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| id == self.id.to_string());
            if mine {
                self.inputs.insert(input.message.id);
            }
            return mine;
        }
        let Some(turn) = event.context.turn_id else {
            return true;
        };
        if self.turns.contains(&turn) {
            return true;
        }
        for input in &self.inputs {
            if let Ok(Some(admission)) = session.admission(*input).await
                && let Ok(Some(assignment)) = admission.assignment().await
                && assignment.turn_id == turn
            {
                self.turns.insert(turn);
                return true;
            }
        }
        false
    }
    pub fn cancellation(&self) -> everruns::CancellationToken {
        self.cancellation.clone()
    }

    pub async fn cancel(&self, session: &everruns::Session) -> anyhow::Result<()> {
        self.cancellation.cancel();
        if let Some(id) = self.admission
            && let Some(admission) = session.admission(id).await?
        {
            admission.cancel(uuid::Uuid::new_v4()).await?;
            admission.wait().await?;
        }
        Ok(())
    }
}

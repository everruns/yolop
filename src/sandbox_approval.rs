//! Hard approval gate for shell commands that need to cross the sandbox boundary.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub(crate) struct ApprovalRequest {
    pub command: String,
    pub reason: String,
    pub full_access: bool,
}

pub(crate) type ApprovalReceiver =
    mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<bool>)>;

#[derive(Clone)]
pub(crate) enum ApprovalGate {
    Deny,
    Channel(mpsc::UnboundedSender<(ApprovalRequest, oneshot::Sender<bool>)>),
}

impl ApprovalGate {
    pub(crate) fn deny() -> Arc<Self> {
        Arc::new(Self::Deny)
    }

    pub(crate) fn channel() -> (Arc<Self>, ApprovalReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(Self::Channel(tx)), rx)
    }

    pub(crate) async fn approve(&self, request: ApprovalRequest) -> bool {
        let Self::Channel(tx) = self else {
            return false;
        };
        let (reply, answer) = oneshot::channel();
        if tx.send((request, reply)).is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }
}

pub(crate) fn denied_receiver() -> ApprovalReceiver {
    let (_tx, rx) = mpsc::unbounded_channel();
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deny_gate_fails_closed() {
        assert!(
            !ApprovalGate::deny()
                .approve(ApprovalRequest {
                    command: "curl example.com".into(),
                    reason: "network access".into(),
                    full_access: true,
                })
                .await
        );
    }

    #[tokio::test]
    async fn channel_gate_returns_the_user_decision() {
        let (gate, mut rx) = ApprovalGate::channel();
        let responder = tokio::spawn(async move {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(true).unwrap();
        });
        assert!(
            gate.approve(ApprovalRequest {
                command: "cargo publish".into(),
                reason: "network access".into(),
                full_access: true,
            })
            .await
        );
        responder.await.unwrap();
    }
}

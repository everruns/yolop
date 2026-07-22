//! Hard approval gate for shell commands that need to cross the sandbox boundary.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub(crate) struct ApprovalRequest {
    pub command: String,
    pub reason: String,
    pub full_access: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Deny,
    ApproveOnce,
    ApproveForSession,
}

pub(crate) type ApprovalReceiver =
    mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>;

#[derive(Clone)]
pub(crate) enum ApprovalGate {
    Deny,
    Channel {
        tx: mpsc::UnboundedSender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
        approved_scopes: Arc<AtomicU8>,
    },
}

const SANDBOX_SCOPE: u8 = 1 << 0;
const FULL_ACCESS_SCOPE: u8 = 1 << 1;

impl ApprovalGate {
    pub(crate) fn deny() -> Arc<Self> {
        Arc::new(Self::Deny)
    }

    pub(crate) fn channel() -> (Arc<Self>, ApprovalReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self::Channel {
                tx,
                approved_scopes: Arc::new(AtomicU8::new(0)),
            }),
            rx,
        )
    }

    pub(crate) async fn approve(&self, request: ApprovalRequest) -> bool {
        let Self::Channel {
            tx,
            approved_scopes,
        } = self
        else {
            return false;
        };
        let scope = if request.full_access {
            FULL_ACCESS_SCOPE
        } else {
            SANDBOX_SCOPE
        };
        if approved_scopes.load(Ordering::Acquire) & scope != 0 {
            return true;
        }
        let (reply, answer) = oneshot::channel();
        if tx.send((request, reply)).is_err() {
            return false;
        }
        match answer.await.unwrap_or(ApprovalDecision::Deny) {
            ApprovalDecision::Deny => false,
            ApprovalDecision::ApproveOnce => true,
            ApprovalDecision::ApproveForSession => {
                approved_scopes.fetch_or(scope, Ordering::AcqRel);
                true
            }
        }
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
            reply.send(ApprovalDecision::ApproveOnce).unwrap();
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

    #[tokio::test]
    async fn session_approval_bypasses_later_prompts() {
        let (gate, mut rx) = ApprovalGate::channel();
        let first = gate.approve(ApprovalRequest {
            command: "cargo test".into(),
            reason: "run tests".into(),
            full_access: true,
        });
        let respond = async {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(ApprovalDecision::ApproveForSession).unwrap();
        };
        let (approved, ()) = tokio::join!(first, respond);
        assert!(approved);

        assert!(
            gate.approve(ApprovalRequest {
                command: "cargo clippy".into(),
                reason: "run lint".into(),
                full_access: true,
            })
            .await
        );
        assert!(rx.try_recv().is_err(), "later approval should not prompt");
    }

    #[tokio::test]
    async fn sandbox_session_approval_does_not_grant_full_access() {
        let (gate, mut rx) = ApprovalGate::channel();
        let sandboxed = gate.approve(ApprovalRequest {
            command: "cargo test".into(),
            reason: "untrusted command".into(),
            full_access: false,
        });
        let respond = async {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(ApprovalDecision::ApproveForSession).unwrap();
        };
        let (approved, ()) = tokio::join!(sandboxed, respond);
        assert!(approved);

        let full_access = gate.approve(ApprovalRequest {
            command: "cargo publish".into(),
            reason: "publish release".into(),
            full_access: true,
        });
        let deny = async {
            let (request, reply) = rx.recv().await.unwrap();
            assert!(request.full_access);
            reply.send(ApprovalDecision::Deny).unwrap();
        };
        let (approved, ()) = tokio::join!(full_access, deny);
        assert!(!approved);
    }
}

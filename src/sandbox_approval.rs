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
    pub scope: ApprovalScope,
}

pub(crate) const MAX_APPROVAL_TEXT_BYTES: usize = 32 * 1024;

impl ApprovalRequest {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        self.validate_with_limit(MAX_APPROVAL_TEXT_BYTES)
    }

    fn validate_with_limit(&self, limit: usize) -> anyhow::Result<()> {
        // THREAT[TM-BASH]: authorization text must never be shortened for review.
        for (field, text) in [("command", &self.command), ("reason", &self.reason)] {
            anyhow::ensure!(
                text.len() <= limit,
                "approval {field} exceeds {limit} bytes"
            );
        }
        Ok(())
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"yolop.approval.v1");
        for text in [
            self.command.as_str(),
            self.reason.as_str(),
            self.scope.as_str(),
        ] {
            hash.update((text.len() as u64).to_le_bytes());
            hash.update(text.as_bytes());
        }
        hash.finalize().into()
    }

    pub(crate) fn matches_digest(&self, digest: Option<&[u8]>) -> bool {
        self.validate().is_ok() && digest == Some(self.digest().as_slice())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalScope {
    Sandboxed,
    FullAccess,
    Memory,
}

impl ApprovalScope {
    fn mask(self) -> u8 {
        match self {
            Self::Sandboxed => SANDBOX_SCOPE,
            Self::FullAccess => FULL_ACCESS_SCOPE,
            Self::Memory => MEMORY_SCOPE,
        }
    }
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
const MEMORY_SCOPE: u8 = 1 << 2;

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
        if request.validate().is_err() {
            return false;
        }
        let Self::Channel {
            tx,
            approved_scopes,
        } = self
        else {
            return false;
        };
        let scope = request.scope.mask();
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

impl ApprovalGate {
    /// Put `request` to whoever answers this gate, without applying the
    /// answer to this gate's own approved scopes. A coordinator uses it to ask
    /// on a thread's behalf; the thread's gate applies the answer. `None` when
    /// nothing here can answer.
    pub(crate) async fn ask(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        request.validate().ok()?;
        let Self::Channel { tx, .. } = self else {
            return None;
        };
        let (reply, answer) = oneshot::channel();
        tx.send((request, reply)).ok()?;
        answer.await.ok()
    }
}

impl ApprovalScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sandboxed => "sandboxed",
            Self::FullAccess => "full_access",
            Self::Memory => "memory",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "sandboxed" => Some(Self::Sandboxed),
            "full_access" => Some(Self::FullAccess),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}

impl ApprovalDecision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::ApproveOnce => "approve_once",
            Self::ApproveForSession => "approve_for_session",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "deny" => Some(Self::Deny),
            "approve_once" => Some(Self::ApproveOnce),
            "approve_for_session" => Some(Self::ApproveForSession),
            _ => None,
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

    fn dummy_request() -> ApprovalRequest {
        ApprovalRequest {
            command: "dummy".into(),
            reason: "review".into(),
            scope: ApprovalScope::FullAccess,
        }
    }

    #[test]
    fn approval_request_validates_utf8_bytes_with_a_small_limit() {
        for command in ["12345678", "éééé"] {
            let request = ApprovalRequest {
                command: command.into(),
                ..dummy_request()
            };
            assert!(request.validate_with_limit(8).is_ok());
        }
        for command in ["123456789", "ééééx"] {
            let request = ApprovalRequest {
                command: command.into(),
                ..dummy_request()
            };
            assert!(request.validate_with_limit(8).is_err());
        }
        let request = ApprovalRequest {
            reason: "123456789".into(),
            ..dummy_request()
        };
        assert!(request.validate_with_limit(8).is_err());
    }

    #[test]
    fn approval_request_digest_preserves_field_identity() {
        let request = dummy_request();
        assert!(request.matches_digest(Some(&request.digest())));
        assert!(!request.matches_digest(None));
        for other in [
            ApprovalRequest {
                command: "other".into(),
                ..request.clone()
            },
            ApprovalRequest {
                reason: "other".into(),
                ..request.clone()
            },
            ApprovalRequest {
                scope: ApprovalScope::Sandboxed,
                ..request.clone()
            },
        ] {
            assert_ne!(request.digest(), other.digest());
        }
        let left = ApprovalRequest {
            command: "ab".into(),
            reason: "c".into(),
            ..request.clone()
        };
        let right = ApprovalRequest {
            command: "a".into(),
            reason: "bc".into(),
            ..request
        };
        assert_ne!(left.digest(), right.digest());
    }

    #[tokio::test]
    async fn oversized_approval_text_never_prompts_or_uses_a_session_grant() {
        let (gate, mut rx) = ApprovalGate::channel();
        let approve = gate.approve(dummy_request());
        let respond = async {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(ApprovalDecision::ApproveForSession).unwrap();
        };
        let (approved, ()) = tokio::join!(approve, respond);
        assert!(approved);
        for request in [
            ApprovalRequest {
                command: "x".repeat(MAX_APPROVAL_TEXT_BYTES + 1),
                ..dummy_request()
            },
            ApprovalRequest {
                reason: "x".repeat(MAX_APPROVAL_TEXT_BYTES + 1),
                ..dummy_request()
            },
        ] {
            assert!(!gate.approve(request.clone()).await);
            assert!(gate.ask(request).await.is_none());
            assert!(rx.try_recv().is_err());
        }
        assert!(gate.approve(dummy_request()).await);
    }

    #[tokio::test]
    async fn deny_gate_fails_closed() {
        assert!(
            !ApprovalGate::deny()
                .approve(ApprovalRequest {
                    command: "curl example.com".into(),
                    reason: "network access".into(),
                    scope: ApprovalScope::FullAccess,
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
                scope: ApprovalScope::FullAccess,
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
            scope: ApprovalScope::FullAccess,
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
                scope: ApprovalScope::FullAccess,
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
            scope: ApprovalScope::Sandboxed,
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
            scope: ApprovalScope::FullAccess,
        });
        let deny = async {
            let (request, reply) = rx.recv().await.unwrap();
            assert_eq!(request.scope, ApprovalScope::FullAccess);
            reply.send(ApprovalDecision::Deny).unwrap();
        };
        let (approved, ()) = tokio::join!(full_access, deny);
        assert!(!approved);
    }

    #[tokio::test]
    async fn memory_session_approval_does_not_grant_shell_access() {
        let (gate, mut rx) = ApprovalGate::channel();
        let memory = gate.approve(ApprovalRequest {
            command: "remember a preference".into(),
            reason: "update durable memory".into(),
            scope: ApprovalScope::Memory,
        });
        let respond = async {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(ApprovalDecision::ApproveForSession).unwrap();
        };
        let (approved, ()) = tokio::join!(memory, respond);
        assert!(approved);

        let shell = gate.approve(ApprovalRequest {
            command: "cargo publish".into(),
            reason: "publish release".into(),
            scope: ApprovalScope::FullAccess,
        });
        let deny = async {
            let (_, reply) = rx.recv().await.unwrap();
            reply.send(ApprovalDecision::Deny).unwrap();
        };
        let (approved, ()) = tokio::join!(shell, deny);
        assert!(!approved);
    }
}

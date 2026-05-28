// Approval gate for destructive operations.
// Decision: writes/deletes flow through SessionFileSystem decorators; bash
// goes through the BashTool directly. Both await an explicit yes from the
// human via a oneshot channel. Read-only ops (read_file, list_directory,
// grep) run free. The TUI installs an interactive gate when `--ask` is on;
// otherwise (and always in `--print` mode) the gate is auto-approve.

use async_trait::async_trait;
use everruns_runtime::FileApprovalGate;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub enum ApprovalRequest {
    Bash {
        command: String,
    },
    FileWrite {
        path: String,
        /// Existing content if the file already exists (for diff display).
        before: Option<String>,
        after: String,
    },
    FileDelete {
        path: String,
        recursive: bool,
    },
}

impl ApprovalRequest {
    pub fn headline(&self) -> String {
        match self {
            Self::Bash { command } => format!("run bash: {}", first_line(command, 200)),
            Self::FileWrite {
                path,
                before,
                after,
            } => {
                let verb = if before.is_some() { "edit" } else { "create" };
                format!("{verb} {path} ({} bytes)", after.len())
            }
            Self::FileDelete { path, recursive } => {
                let r = if *recursive { " (recursive)" } else { "" };
                format!("delete {path}{r}")
            }
        }
    }

    /// Multi-line body shown above the prompt (diff, command, etc.).
    pub fn detail(&self) -> String {
        match self {
            Self::Bash { command } => command.clone(),
            Self::FileWrite {
                before,
                after,
                path,
            } => match before {
                Some(b) => crate::diff::unified(b, after, path, 3),
                None => format!("(new file, {} bytes)", after.len()),
            },
            Self::FileDelete { .. } => String::new(),
        }
    }
}

fn first_line(s: &str, max: usize) -> String {
    let l = s.lines().next().unwrap_or("");
    if l.len() > max {
        format!("{}…", &l[..max])
    } else {
        l.to_string()
    }
}

#[derive(Clone)]
pub enum ApprovalGate {
    Auto,
    Channel(mpsc::UnboundedSender<(ApprovalRequest, oneshot::Sender<bool>)>),
}

impl ApprovalGate {
    pub fn auto() -> Arc<Self> {
        Arc::new(Self::Auto)
    }
    pub fn channel(
        tx: mpsc::UnboundedSender<(ApprovalRequest, oneshot::Sender<bool>)>,
    ) -> Arc<Self> {
        Arc::new(Self::Channel(tx))
    }

    pub async fn approve(&self, req: ApprovalRequest) -> bool {
        match self {
            Self::Auto => true,
            Self::Channel(tx) => {
                let (otx, orx) = oneshot::channel();
                if tx.send((req, otx)).is_err() {
                    return false;
                }
                orx.await.unwrap_or(false)
            }
        }
    }
}

#[async_trait]
impl FileApprovalGate for ApprovalGate {
    async fn approve_write(&self, path: &str, before: Option<String>, after: &str) -> bool {
        self.approve(ApprovalRequest::FileWrite {
            path: path.to_string(),
            before,
            after: after.to_string(),
        })
        .await
    }

    async fn approve_delete(&self, path: &str, recursive: bool) -> bool {
        self.approve(ApprovalRequest::FileDelete {
            path: path.to_string(),
            recursive,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_returns_first_line_only() {
        assert_eq!(first_line("one\ntwo\nthree", 100), "one");
    }

    #[test]
    fn first_line_truncates_with_ellipsis() {
        let out = first_line("abcdefghij", 4);
        assert_eq!(out, "abcd…");
    }

    #[test]
    fn first_line_handles_empty_input() {
        assert_eq!(first_line("", 10), "");
    }

    #[test]
    fn first_line_does_not_truncate_within_limit() {
        assert_eq!(first_line("hi", 10), "hi");
    }

    #[test]
    fn headline_bash_uses_first_line() {
        let req = ApprovalRequest::Bash {
            command: "ls -la\nrm -rf /".into(),
        };
        assert_eq!(req.headline(), "run bash: ls -la");
    }

    #[test]
    fn headline_file_write_says_create_when_no_before() {
        let req = ApprovalRequest::FileWrite {
            path: "/tmp/x".into(),
            before: None,
            after: "hello".into(),
        };
        assert_eq!(req.headline(), "create /tmp/x (5 bytes)");
    }

    #[test]
    fn headline_file_write_says_edit_when_before_present() {
        let req = ApprovalRequest::FileWrite {
            path: "/tmp/x".into(),
            before: Some("old".into()),
            after: "newer".into(),
        };
        assert_eq!(req.headline(), "edit /tmp/x (5 bytes)");
    }

    #[test]
    fn headline_file_delete_marks_recursive() {
        let req = ApprovalRequest::FileDelete {
            path: "/tmp/dir".into(),
            recursive: true,
        };
        assert_eq!(req.headline(), "delete /tmp/dir (recursive)");
    }

    #[test]
    fn headline_file_delete_non_recursive_has_no_suffix() {
        let req = ApprovalRequest::FileDelete {
            path: "/tmp/f".into(),
            recursive: false,
        };
        assert_eq!(req.headline(), "delete /tmp/f");
    }

    #[test]
    fn detail_bash_returns_full_command() {
        let req = ApprovalRequest::Bash {
            command: "line1\nline2".into(),
        };
        assert_eq!(req.detail(), "line1\nline2");
    }

    #[test]
    fn detail_file_write_new_file_summarises_size() {
        let req = ApprovalRequest::FileWrite {
            path: "/tmp/x".into(),
            before: None,
            after: "abcd".into(),
        };
        assert_eq!(req.detail(), "(new file, 4 bytes)");
    }

    #[test]
    fn detail_file_write_edit_renders_unified_diff() {
        let req = ApprovalRequest::FileWrite {
            path: "x".into(),
            before: Some("hello\n".into()),
            after: "world\n".into(),
        };
        let out = req.detail();
        assert!(out.contains("--- a/x"), "missing a/ header: {out}");
        assert!(out.contains("+++ b/x"), "missing b/ header: {out}");
        assert!(out.contains("-hello"), "missing removal: {out}");
        assert!(out.contains("+world"), "missing addition: {out}");
    }

    #[test]
    fn detail_file_delete_is_empty() {
        let req = ApprovalRequest::FileDelete {
            path: "/x".into(),
            recursive: false,
        };
        assert_eq!(req.detail(), "");
    }

    #[tokio::test]
    async fn auto_gate_always_approves() {
        let gate = ApprovalGate::auto();
        assert!(
            gate.approve(ApprovalRequest::Bash {
                command: "rm -rf /".into(),
            })
            .await
        );
    }

    #[tokio::test]
    async fn channel_gate_forwards_request_and_returns_approval() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
        let gate = ApprovalGate::channel(tx);

        let approver = tokio::spawn(async move {
            let (req, responder) = rx.recv().await.expect("request");
            match req {
                ApprovalRequest::Bash { command } => assert_eq!(command, "ls"),
                other => panic!("unexpected request: {other:?}"),
            }
            responder.send(true).expect("send approval");
        });

        let approved = gate
            .approve(ApprovalRequest::Bash {
                command: "ls".into(),
            })
            .await;
        approver.await.unwrap();
        assert!(approved);
    }

    #[tokio::test]
    async fn channel_gate_returns_false_on_denial() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
        let gate = ApprovalGate::channel(tx);

        tokio::spawn(async move {
            let (_, responder) = rx.recv().await.unwrap();
            responder.send(false).unwrap();
        });

        let approved = gate
            .approve(ApprovalRequest::Bash {
                command: "rm".into(),
            })
            .await;
        assert!(!approved);
    }

    #[tokio::test]
    async fn channel_gate_returns_false_when_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
        drop(rx);
        let gate = ApprovalGate::channel(tx);

        let approved = gate
            .approve(ApprovalRequest::Bash {
                command: "ls".into(),
            })
            .await;
        assert!(!approved);
    }

    #[tokio::test]
    async fn channel_gate_returns_false_when_responder_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
        let gate = ApprovalGate::channel(tx);

        tokio::spawn(async move {
            let (_, responder) = rx.recv().await.unwrap();
            drop(responder);
        });

        let approved = gate
            .approve(ApprovalRequest::Bash {
                command: "ls".into(),
            })
            .await;
        assert!(!approved);
    }

    #[tokio::test]
    async fn file_approval_gate_forwards_write_request() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
        let gate = ApprovalGate::channel(tx);

        tokio::spawn(async move {
            let (req, responder) = rx.recv().await.unwrap();
            match req {
                ApprovalRequest::FileWrite {
                    path,
                    before,
                    after,
                } => {
                    assert_eq!(path, "/tmp/x");
                    assert_eq!(before.as_deref(), Some("old"));
                    assert_eq!(after, "new");
                }
                other => panic!("expected FileWrite, got {other:?}"),
            }
            responder.send(true).unwrap();
        });

        let approved =
            FileApprovalGate::approve_write(&*gate, "/tmp/x", Some("old".into()), "new").await;
        assert!(approved);
    }

    #[tokio::test]
    async fn file_approval_gate_forwards_delete_request() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<bool>)>();
        let gate = ApprovalGate::channel(tx);

        tokio::spawn(async move {
            let (req, responder) = rx.recv().await.unwrap();
            match req {
                ApprovalRequest::FileDelete { path, recursive } => {
                    assert_eq!(path, "/tmp/dir");
                    assert!(recursive);
                }
                other => panic!("expected FileDelete, got {other:?}"),
            }
            responder.send(true).unwrap();
        });

        assert!(FileApprovalGate::approve_delete(&*gate, "/tmp/dir", true).await);
    }
}

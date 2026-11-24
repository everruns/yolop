//! Bounded UI inbox projected from canonical work, separate from execution state.

use super::*;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub(super) enum Target {
    Task(SessionId, String),
    Recovery,
    Conversation,
}

pub(super) struct Notice {
    pub key: String,
    pub title: String,
    pub detail: String,
    pub target: Target,
}

#[derive(Default)]
pub(super) struct Attention {
    pub items: VecDeque<Notice>,
    seen: VecDeque<String>,
    pub focus: Option<bool>,
    /// Off, OSC 9, OSC 777. Opt-in for this process, never sends prompt content.
    pub protocol: u8,
    seeded: bool,
}

impl Attention {
    /// Returns whether a new item should notify. Acknowledgement does not
    /// forget its identity, so a registry refresh cannot recreate it.
    pub fn push(&mut self, mut notice: Notice) -> bool {
        notice.title = notice.title.chars().take(512).collect();
        notice.detail.truncate(
            notice
                .detail
                .floor_char_boundary((64 * 1024).min(notice.detail.len())),
        );
        if self.seen.contains(&notice.key) {
            return false;
        }
        self.seen.push_back(notice.key.clone());
        while self.seen.len() > 4096 {
            self.seen.pop_front();
        }
        self.items.push_front(notice);
        self.items.truncate(100);
        self.protocol != 0 && self.focus == Some(false)
    }

    pub fn acknowledge(&mut self, key: &str) {
        self.items.retain(|notice| notice.key != key);
    }

    pub fn observe_tasks(&mut self, tasks: &session_tasks_view::TaskTree) -> bool {
        let mut notify = false;
        for row in &tasks.rows {
            let task = &row.task;
            if !task.state.is_terminal() && task.input_request.is_none() {
                continue;
            }
            let key = format!(
                "task:{}:{}:{}:{}:{}",
                task.session_id,
                task.id,
                task.attempt,
                task.state,
                task.input_request
                    .as_ref()
                    .map(
                        |request| Sha256::digest(serde_json::to_vec(request).unwrap_or_default())
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    )
                    .unwrap_or_default()
            );
            let detail = format!(
                "{}\nOwner: {}\nTask: {}\nState: {}\n{}\n{}",
                task.display_name,
                task.session_id,
                task.id,
                task.state,
                task.summary.as_deref().unwrap_or_default(),
                task.input_request
                    .as_ref()
                    .map(|request| serde_json::to_string_pretty(request).unwrap_or_default())
                    .unwrap_or_default()
            );
            notify |= self.push(Notice {
                key,
                title: format!(
                    "{} · {}",
                    task.display_name,
                    if task.input_request.is_some() {
                        "needs input".to_string()
                    } else {
                        task.state.to_string()
                    }
                ),
                detail,
                target: Target::Task(task.session_id, task.id.clone()),
            });
        }
        // Existing saved completions populate the inbox without desktop noise.
        let notify = self.seeded && notify;
        self.seeded = true;
        notify
    }
}

impl App {
    pub(super) fn record_attention(
        &mut self,
        key: String,
        title: &str,
        detail: String,
        target: Target,
    ) {
        if self.attention.push(Notice {
            key,
            title: title.into(),
            detail,
            target,
        }) {
            self.notify_attention();
        }
    }

    pub(crate) fn suspend_attention_reporting(&mut self) {
        self.focus_reporting = None;
        self.attention.focus = None;
    }

    pub(crate) fn resume_attention_reporting(&mut self) {
        self.set_attention_protocol();
    }

    pub(super) fn set_attention_protocol(&mut self) {
        if self.attention.protocol == 0 {
            self.focus_reporting = None;
            self.attention.focus = None;
            return;
        }
        if self.native_progress && self.focus_reporting.is_none() {
            match tuika::term::attention::FocusReporting::enable(std::io::stdout()) {
                Ok(guard) => self.focus_reporting = Some(guard),
                Err(error) => {
                    self.attention.protocol = 0;
                    self.workflow.notice = format!(
                        "Could not enable focus reporting: {error}. Inbox remains available."
                    );
                }
            }
            self.attention.focus = None;
        }
    }

    pub(super) fn notify_attention(&self) {
        use tuika::term::attention::{self, Protocol};
        if !self.native_progress || self.attention.focus != Some(false) {
            return;
        }
        let protocol = match self.attention.protocol {
            1 => Protocol::Osc9,
            2 => Protocol::Osc777,
            _ => return,
        };
        if let Err(error) = attention::write(
            &mut std::io::stdout(),
            "Yolop",
            "Work needs attention. Open /attention in your session.",
            protocol,
        ) {
            tracing::warn!("terminal notification failed: {error}");
        }
    }

    pub(super) fn observe_attention(&mut self) {
        if self.attention.observe_tasks(&self.session_tasks) {
            self.notify_attention();
        }
        if self.workflow.panel == Some(workflow::WorkflowPanel::Attention) {
            self.open_workbench(workflow::WorkflowPanel::Attention);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn notice(key: &str) -> Notice {
        Notice {
            key: key.into(),
            title: "Finished".into(),
            detail: "Evidence".into(),
            target: Target::Conversation,
        }
    }
    #[test]
    fn inbox_deduplicates_acknowledged_items_and_notifies_only_known_blurred_focus() {
        let mut state = Attention {
            protocol: 1,
            ..Default::default()
        };
        assert!(!state.push(notice("a")));
        state.focus = Some(true);
        assert!(!state.push(notice("b")));
        state.focus = Some(false);
        assert!(state.push(notice("c")));
        state.acknowledge("c");
        assert!(!state.push(notice("c")));
        assert_eq!(state.items.len(), 2);
        for index in 0..5000 {
            state.push(notice(&index.to_string()));
        }
        assert_eq!(state.items.len(), 100);
        assert_eq!(state.seen.len(), 4096);
    }
}

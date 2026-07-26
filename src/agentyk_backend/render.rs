//! Terminal output derived entirely from the agentyk event stream.
//!
//! Nothing here reaches into the session: an [`EventListener`] is the only
//! subscription, which is the claim agentyk's design makes about UI being a
//! pure observer. It holds up for a line-oriented renderer. What it does not
//! give is back-pressure or ordering guarantees against the host's own
//! `println!`, so the final answer is printed by the caller, not from the
//! stream.

use std::io::Write;
use std::sync::Mutex;

use agentyk::{Event, EventData, EventListener};
use async_trait::async_trait;

const DIM: &str = "\u{1b}[2m";
const RESET: &str = "\u{1b}[0m";

pub struct StdoutRenderer {
    color: bool,
    /// Whether the current assistant message has printed any delta, so a
    /// tool line can decide whether it needs a leading newline.
    streaming: Mutex<bool>,
}

impl StdoutRenderer {
    pub fn new(color: bool) -> Self {
        Self {
            color,
            streaming: Mutex::new(false),
        }
    }

    fn dim(&self, text: &str) -> String {
        match self.color {
            true => format!("{DIM}{text}{RESET}"),
            false => text.to_string(),
        }
    }
}

#[async_trait]
impl EventListener for StdoutRenderer {
    fn name(&self) -> &'static str {
        "yolop-stdout"
    }

    async fn on_event(&self, event: &Event) {
        match &event.data {
            EventData::OutputMessageDelta { delta, .. } => {
                print!("{delta}");
                std::io::stdout().flush().ok();
                *self.streaming.lock().expect("renderer state") = true;
            }
            EventData::ToolStarted { call } => {
                let mut streaming = self.streaming.lock().expect("renderer state");
                if *streaming {
                    println!();
                    *streaming = false;
                }
                println!("{}", self.dim(&format!("· {}", describe(call))));
            }
            // Ephemeral progress from a still-running tool: the only thing
            // that fills the gap between `tool.started` and its result.
            EventData::ToolProgress { progress, .. } => {
                println!(
                    "{}",
                    self.dim(&format!("  {}", first_line(&progress.message)))
                );
            }
            EventData::ToolDenied { name, reason, .. } => {
                println!("{}", self.dim(&format!("· {name} denied — {reason}")));
            }
            EventData::ToolCompleted {
                name,
                is_error: true,
                output,
                ..
            } => {
                println!(
                    "{}",
                    self.dim(&format!("· {name} failed — {}", first_line(output)))
                );
            }
            EventData::TurnSealed { reason } => {
                println!("{}", self.dim(&format!("· turn sealed: {reason:?}")));
            }
            _ => {}
        }
    }
}

/// Print the answer the turn settled on. Streaming already showed it, so this
/// only adds the trailing newline unless nothing streamed at all.
pub fn final_answer(response: &str, _color: bool) {
    let trimmed = response.trim_end();
    if trimmed.is_empty() {
        println!();
        return;
    }
    println!();
    let _ = std::io::stdout().flush();
}

fn describe(call: &agentyk::ToolCall) -> String {
    let detail = match call.name.as_str() {
        "bash" => call.arguments["command"].as_str(),
        "grep_files" => call.arguments["pattern"].as_str(),
        _ => call.arguments["path"].as_str(),
    };
    match detail {
        Some(detail) => format!("{} {}", call.name, first_line(detail)),
        None => call.name.clone(),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    match line.chars().count() > 100 {
        true => format!("{}…", line.chars().take(100).collect::<String>()),
        false => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentyk::ToolCall;
    use serde_json::json;

    #[test]
    fn tool_lines_name_their_subject() {
        let bash = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command": "cargo test\n--all"}),
        };
        assert_eq!(describe(&bash), "bash cargo test");

        let read = ToolCall {
            id: "2".into(),
            name: "read_file".into(),
            arguments: json!({"path": "src/main.rs"}),
        };
        assert_eq!(describe(&read), "read_file src/main.rs");
    }

    #[test]
    fn long_lines_are_clipped() {
        let clipped = first_line(&"x".repeat(200));
        assert_eq!(clipped.chars().count(), 101);
        assert!(clipped.ends_with('…'));
    }
}

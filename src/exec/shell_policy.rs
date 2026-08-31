//! Static checks for shell actions that need stronger process safety.

use std::path::Path;

use tree_sitter::{Node, Parser};

const PROCESS_CONTROL_PROGRAMS: &[&str] = &["kill", "killall", "pkill"];
const DESTRUCTIVE_PROGRAMS: &[&str] = &[
    "aider", "claude", "codex", "gemini", "kill", "killall", "pkill", "yolop",
];
const EXEC_WRAPPERS: &[&str] = &[
    "command", "env", "exec", "find", "nice", "nohup", "sudo", "time", "xargs",
];

/// Process-control and nested agent commands are destructive shell actions.
pub(crate) fn requires_destructive_approval(script: &str) -> bool {
    any_command_matches(script, DESTRUCTIVE_PROGRAMS)
}

/// Refuse shell commands that visibly target the Yolop host process itself.
///
/// This is defense in depth for direct mistakes such as `kill <host-pid>`.
/// Kernel or VM isolation remains the boundary against deliberately obscured
/// signaling through scripts or dynamically discovered PIDs.
pub(crate) fn command_can_signal_yolop(script: &str, yolop_pid: u32) -> bool {
    let has_process_control = any_command_matches(script, PROCESS_CONTROL_PROGRAMS);
    if !has_process_control {
        return false;
    }

    script
        .split(|character: char| !character.is_ascii_digit())
        .any(|word| word.parse::<u32>().ok() == Some(yolop_pid))
        || script.to_ascii_lowercase().contains("yolop")
        || script
            .split_ascii_whitespace()
            .map(|word| word.trim_matches(|character: char| ";|&()".contains(character)))
            .any(|word| matches!(word, "0" | "-1"))
}

fn any_command_matches(script: &str, programs: &[&str]) -> bool {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(script, None) else {
        return false;
    };
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if node.kind() == "command" && command_matches(&node, script, programs) {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn command_matches(command: &Node<'_>, script: &str, programs: &[&str]) -> bool {
    let Some(name) = command.child_by_field_name("name") else {
        return false;
    };
    let Some(name) = plain_program(name, script) else {
        return false;
    };
    if programs.contains(&name.as_str()) {
        return true;
    }
    if !EXEC_WRAPPERS.contains(&name.as_str()) {
        return false;
    }

    let mut cursor = command.walk();
    command
        .children_by_field_name("argument", &mut cursor)
        .filter_map(|argument| plain_program(argument, script))
        .any(|argument| programs.contains(&argument.as_str()))
}

fn plain_program(node: Node<'_>, script: &str) -> Option<String> {
    if !matches!(node.kind(), "command_name" | "word") {
        return None;
    }
    let word = node.utf8_text(script.as_bytes()).ok()?;
    Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_direct_and_wrapped_destructive_commands() {
        for command in [
            "kill 1234",
            "cd /tmp && pkill -f worker",
            "env codex exec --full-auto task",
            "command claude -p task",
            "xargs kill < pids",
            "find . -exec kill {} +",
            "codex exec --full-auto task",
            "claude -p task",
        ] {
            assert!(
                requires_destructive_approval(command),
                "expected destructive: {command}"
            );
        }
    }

    #[test]
    fn ignores_program_names_used_only_as_data() {
        for command in [
            "rg codex src",
            "printf '%s' 'kill 1234'",
            "cargo test codex",
            "echo yolop",
        ] {
            assert!(
                !requires_destructive_approval(command),
                "expected ordinary command: {command}"
            );
        }
    }
}

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct ZedIntoOptions {
    pub settings_path: Option<PathBuf>,
    pub agent_name: String,
    pub command: PathBuf,
    pub force: bool,
}

#[derive(Debug)]
pub struct ZedIntoResult {
    pub settings_path: PathBuf,
    pub agent_name: String,
    pub command: String,
    pub status: IntoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntoStatus {
    Created,
    Updated,
    Unchanged,
}

pub fn into_zed(options: ZedIntoOptions) -> Result<ZedIntoResult> {
    if options.agent_name.trim().is_empty() {
        bail!("agent server name cannot be empty");
    }

    let settings_path = options
        .settings_path
        .unwrap_or_else(default_zed_settings_path);
    let command = path_to_json_string(&options.command)?;
    let desired = zed_agent_server(&command);
    let existing_text = match std::fs::read_to_string(&settings_path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err)
                .with_context(|| format!("read Zed settings file {}", settings_path.display()));
        }
    };
    let mut root = match existing_text.as_deref() {
        Some(text) if !text.trim().is_empty() => parse_zed_settings(text)
            .with_context(|| format!("parse Zed settings file {}", settings_path.display()))?,
        _ => json!({}),
    };

    let root_object = root
        .as_object_mut()
        .context("Zed settings root must be a JSON object")?;
    let agent_servers = root_object
        .entry("agent_servers")
        .or_insert_with(|| Value::Object(Map::new()));
    let agent_servers = agent_servers
        .as_object_mut()
        .context("Zed settings `agent_servers` must be a JSON object")?;

    let status = merge_agent_server(agent_servers, &options.agent_name, desired, options.force)?;

    if status != IntoStatus::Unchanged {
        let rendered = render_settings(&root, existing_text.as_deref())?;
        write_file_atomically(&settings_path, rendered.as_bytes())?;
    }

    Ok(ZedIntoResult {
        settings_path,
        agent_name: options.agent_name,
        command,
        status,
    })
}

fn zed_agent_server(command: &str) -> Value {
    json!({
        "type": "custom",
        "command": command,
        "args": ["--acp"],
        "env": {}
    })
}

fn merge_agent_server(
    agent_servers: &mut Map<String, Value>,
    agent_name: &str,
    desired: Value,
    force: bool,
) -> Result<IntoStatus> {
    let Some(current) = agent_servers.get_mut(agent_name) else {
        agent_servers.insert(agent_name.to_string(), desired);
        return Ok(IntoStatus::Created);
    };

    if current == &desired {
        return Ok(IntoStatus::Unchanged);
    }

    if force {
        *current = desired;
        return Ok(IntoStatus::Updated);
    }

    let Some(current_object) = current.as_object_mut() else {
        bail!("Zed agent_servers.{agent_name} is not an object; re-run with --force to replace it");
    };
    let desired_object = desired
        .as_object()
        .expect("desired Zed agent server is an object");
    let mut changed = false;
    for key in ["type", "command", "args"] {
        if current_object.get(key) != desired_object.get(key) {
            current_object.insert(
                key.to_string(),
                desired_object.get(key).expect("desired key exists").clone(),
            );
            changed = true;
        }
    }
    if !current_object.contains_key("env") {
        current_object.insert("env".to_string(), json!({}));
        changed = true;
    }

    Ok(if changed {
        IntoStatus::Updated
    } else {
        IntoStatus::Unchanged
    })
}

fn default_zed_settings_path() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        return PathBuf::from(config_home).join("zed").join("settings.json");
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".config")
            .join("zed")
            .join("settings.json");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zed")
        .join("settings.json")
}

fn path_to_json_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .with_context(|| format!("command path is not valid UTF-8: {}", path.display()))
}

fn parse_zed_settings(text: &str) -> Result<Value> {
    let without_comments = strip_jsonc_comments(text);
    let strict_json = strip_trailing_commas(&without_comments);
    Ok(serde_json::from_str(&strict_json)?)
}

fn render_settings(root: &Value, original_text: Option<&str>) -> Result<String> {
    let mut rendered = String::new();
    if let Some(header) = original_text.and_then(leading_comment_header) {
        rendered.push_str(header);
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
    }
    rendered.push_str(&serde_json::to_string_pretty(root).context("serialize Zed settings")?);
    rendered.push('\n');
    Ok(rendered)
}

fn leading_comment_header(text: &str) -> Option<&str> {
    let idx = text.find('{')?;
    let header = &text[..idx];
    if header.trim().is_empty() {
        return None;
    }
    if header.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || trimmed.starts_with("//")
    }) {
        Some(header)
    } else {
        None
    }
}

fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for comment_ch in chars.by_ref() {
                        if comment_ch == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for comment_ch in chars.by_ref() {
                        if comment_ch == '\n' {
                            out.push('\n');
                        }
                        if previous == '*' && comment_ch == '/' {
                            break;
                        }
                        previous = comment_ch;
                    }
                }
                _ => out.push(ch),
            }
            continue;
        }

        out.push(ch);
    }

    out
}

fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    'outer: while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == ',' {
            let mut lookahead = chars.clone();
            while let Some(next) = lookahead.peek().copied() {
                if next.is_whitespace() {
                    lookahead.next();
                    continue;
                }
                if next == '}' || next == ']' {
                    chars = lookahead;
                    continue 'outer;
                }
                break;
            }
        }

        out.push(ch);
    }

    out
}

fn write_file_atomically(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create Zed settings dir {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("Zed settings path has no file name: {}", path.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = parent.join(tmp_name);

    let write_result = std::fs::write(&tmp_path, content)
        .with_context(|| format!("write temp Zed settings {}", tmp_path.display()));
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn into_at(path: PathBuf, command: &str, force: bool) -> Result<ZedIntoResult> {
        into_zed(ZedIntoOptions {
            settings_path: Some(path),
            agent_name: "yolop".to_string(),
            command: PathBuf::from(command),
            force,
        })
    }

    #[test]
    fn zed_into_creates_missing_settings_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("zed/settings.json");

        let result = into_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Created);
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("settings")).unwrap();
        assert_eq!(
            value["agent_servers"]["yolop"],
            json!({
                "type": "custom",
                "command": "/bin/yolop",
                "args": ["--acp"],
                "env": {}
            })
        );
    }

    #[test]
    fn zed_into_preserves_existing_settings_and_header_comments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            "// Zed settings\n{\n  \"theme\": \"One Dark\",\n  \"agent_servers\": {},\n}\n",
        )
        .expect("write settings");

        let result = into_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Created);
        let text = std::fs::read_to_string(path).expect("settings");
        assert!(text.starts_with("// Zed settings\n"));
        let parsed = parse_zed_settings(&text).expect("parse");
        assert_eq!(parsed["theme"], "One Dark");
        assert_eq!(parsed["agent_servers"]["yolop"]["args"], json!(["--acp"]));
    }

    #[test]
    fn zed_into_updates_existing_object_and_preserves_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"agent_servers":{"yolop":{"type":"custom","command":"old","args":["--old"],"env":{"OPENAI_API_KEY":"keep"},"default_model":"gpt-test"}}}"#,
        )
        .expect("write settings");

        let result = into_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Updated);
        let parsed = parse_zed_settings(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed["agent_servers"]["yolop"]["command"], "/bin/yolop");
        assert_eq!(parsed["agent_servers"]["yolop"]["args"], json!(["--acp"]));
        assert_eq!(
            parsed["agent_servers"]["yolop"]["env"]["OPENAI_API_KEY"],
            "keep"
        );
        assert_eq!(
            parsed["agent_servers"]["yolop"]["default_model"],
            "gpt-test"
        );
    }

    #[test]
    fn zed_into_leaves_matching_object_unchanged_even_with_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"agent_servers":{"yolop":{"type":"custom","command":"/bin/yolop","args":["--acp"],"env":{"OPENAI_API_KEY":"keep"}}}}"#,
        )
        .expect("write settings");

        let result = into_at(path, "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Unchanged);
    }

    #[test]
    fn zed_into_refuses_non_object_entry_without_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"agent_servers":{"yolop":"old"}}"#).expect("write settings");

        let err = into_at(path, "/bin/yolop", false).expect_err("expected conflict");

        assert!(err.to_string().contains("--force"));
    }

    #[test]
    fn zed_into_replaces_non_object_entry_with_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"agent_servers":{"yolop":"old"}}"#).expect("write settings");

        let result = into_at(path.clone(), "/bin/yolop", true).expect("into");

        assert_eq!(result.status, IntoStatus::Updated);
        let parsed = parse_zed_settings(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed["agent_servers"]["yolop"]["command"], "/bin/yolop");
    }
}

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct ZedIntoOptions {
    pub settings_path: Option<PathBuf>,
    pub agent_name: String,
    pub command: PathBuf,
    pub force: bool,
}

#[derive(Debug)]
pub struct PaseoIntoOptions {
    pub settings_path: Option<PathBuf>,
    pub agent_name: String,
    pub command: PathBuf,
    pub force: bool,
}

#[derive(Debug)]
pub struct BuzzIntoOptions {
    pub harness_path: Option<PathBuf>,
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

#[derive(Debug)]
pub struct PaseoIntoResult {
    pub settings_path: PathBuf,
    pub agent_name: String,
    pub command: String,
    pub status: IntoStatus,
}

#[derive(Debug)]
pub struct BuzzIntoResult {
    pub harness_path: PathBuf,
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
    let existing_text = read_optional_settings(&settings_path, "Zed")?;
    let mut root = parse_settings_or_empty(existing_text.as_deref(), &settings_path, "Zed")?;

    let root_object = root
        .as_object_mut()
        .context("Zed settings root must be a JSON object")?;
    let agent_servers = root_object
        .entry("agent_servers")
        .or_insert_with(|| Value::Object(Map::new()));
    let agent_servers = agent_servers
        .as_object_mut()
        .context("Zed settings `agent_servers` must be a JSON object")?;

    let status = merge_agent_server(
        agent_servers,
        &options.agent_name,
        zed_agent_server(&command),
        options.force,
        "Zed agent_servers",
    )?;

    if status != IntoStatus::Unchanged {
        let rendered = render_settings(&root, existing_text.as_deref(), "Zed")?;
        write_file_atomically(&settings_path, rendered.as_bytes(), "Zed settings")?;
    }

    Ok(ZedIntoResult {
        settings_path,
        agent_name: options.agent_name,
        command,
        status,
    })
}

pub fn into_paseo(options: PaseoIntoOptions) -> Result<PaseoIntoResult> {
    if options.agent_name.trim().is_empty() {
        bail!("agent server name cannot be empty");
    }

    let settings_path = options
        .settings_path
        .unwrap_or_else(default_paseo_settings_path);
    let command = path_to_json_string(&options.command)?;
    let existing_text = read_optional_settings(&settings_path, "Paseo")?;
    let mut root = parse_settings_or_empty(existing_text.as_deref(), &settings_path, "Paseo")?;

    let root_object = root
        .as_object_mut()
        .context("Paseo settings root must be a JSON object")?;
    let agents = root_object
        .entry("agents")
        .or_insert_with(|| Value::Object(Map::new()));
    let agents = agents
        .as_object_mut()
        .context("Paseo config `agents` must be a JSON object")?;
    let providers = agents
        .entry("providers")
        .or_insert_with(|| Value::Object(Map::new()));
    let providers = providers
        .as_object_mut()
        .context("Paseo config `agents.providers` must be a JSON object")?;

    let status = merge_agent_server(
        providers,
        &options.agent_name,
        paseo_acp_provider(&options.agent_name, &command),
        options.force,
        "Paseo agents.providers",
    )?;

    if status != IntoStatus::Unchanged {
        let rendered = render_settings(&root, existing_text.as_deref(), "Paseo")?;
        write_file_atomically(&settings_path, rendered.as_bytes(), "Paseo settings")?;
    }

    Ok(PaseoIntoResult {
        settings_path,
        agent_name: options.agent_name,
        command,
        status,
    })
}

pub fn into_buzz(options: BuzzIntoOptions) -> Result<BuzzIntoResult> {
    if options.agent_name.trim().is_empty() {
        bail!("agent harness name cannot be empty");
    }

    let harness_path = options
        .harness_path
        .unwrap_or_else(|| default_buzz_harness_path(&options.agent_name));
    let command = path_to_json_string(&options.command)?;
    let desired = buzz_harness(&options.agent_name, &command);
    let existing_text = read_optional_settings(&harness_path, "Buzz")?;
    let (root, status) = match existing_text.as_deref() {
        None => (desired, IntoStatus::Created),
        Some(text) => {
            let mut current = parse_settings_or_empty(Some(text), &harness_path, "Buzz")?;
            if current == desired {
                (current, IntoStatus::Unchanged)
            } else if options.force {
                (desired, IntoStatus::Updated)
            } else {
                let before = current.clone();
                let current_object = current.as_object_mut().context(
                    "Buzz custom harness must be a JSON object; re-run with --force to replace it",
                )?;
                let desired_object = desired
                    .as_object()
                    .expect("desired Buzz harness is an object");
                for (key, value) in desired_object {
                    if key != "env" {
                        current_object.insert(key.clone(), value.clone());
                    }
                }
                if !current_object.contains_key("env") {
                    current_object.insert("env".to_string(), json!({}));
                }
                let status = if current == before {
                    IntoStatus::Unchanged
                } else {
                    IntoStatus::Updated
                };
                (current, status)
            }
        }
    };

    if status != IntoStatus::Unchanged {
        let rendered = render_settings(&root, None, "Buzz")?;
        write_file_atomically(&harness_path, rendered.as_bytes(), "Buzz custom harness")?;
    }

    Ok(BuzzIntoResult {
        harness_path,
        agent_name: options.agent_name,
        command,
        status,
    })
}

fn buzz_harness(agent_name: &str, command: &str) -> Value {
    json!({
        "name": agent_name,
        "display_name": agent_name,
        "description": "Yolop coding agent",
        "command": command,
        "args": ["--acp"],
        "env": {}
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

fn paseo_acp_provider(label: &str, command: &str) -> Value {
    json!({
        "extends": "acp",
        "label": label,
        "command": [command, "--acp"]
    })
}

fn merge_agent_server(
    agent_servers: &mut Map<String, Value>,
    agent_name: &str,
    desired: Value,
    force: bool,
    config_path: &str,
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
        bail!("{config_path}.{agent_name} is not an object; re-run with --force to replace it");
    };
    let desired_object = desired
        .as_object()
        .expect("desired agent server is an object");
    let mut changed = false;
    for (key, value) in desired_object {
        if key == "env" {
            continue;
        }
        if current_object.get(key) != Some(value) {
            current_object.insert(key.clone(), value.clone());
            changed = true;
        }
    }
    if desired_object.contains_key("env") && !current_object.contains_key("env") {
        current_object.insert("env".to_string(), json!({}));
        changed = true;
    }

    Ok(if changed {
        IntoStatus::Updated
    } else {
        IntoStatus::Unchanged
    })
}

fn default_buzz_harness_path(agent_name: &str) -> PathBuf {
    let base = dirs::data_dir()
        .unwrap_or_else(|| dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")));
    base.join("xyz.block.buzz.app")
        .join("custom_harnesses")
        .join(format!("{agent_name}.json"))
}

fn default_paseo_settings_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".paseo").join("config.json");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("paseo")
        .join("config.json")
}

fn read_optional_settings(settings_path: &Path, app_name: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(settings_path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err)
            .with_context(|| format!("read {app_name} settings file {}", settings_path.display())),
    }
}

fn parse_settings_or_empty(
    existing_text: Option<&str>,
    settings_path: &Path,
    app_name: &str,
) -> Result<Value> {
    match existing_text {
        Some(text) if !text.trim().is_empty() => parse_jsonc_settings(text)
            .with_context(|| format!("parse {app_name} settings file {}", settings_path.display())),
        _ => Ok(json!({})),
    }
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

fn parse_jsonc_settings(text: &str) -> Result<Value> {
    let without_comments = strip_jsonc_comments(text);
    let strict_json = strip_trailing_commas(&without_comments);
    Ok(serde_json::from_str(&strict_json)?)
}

fn render_settings(root: &Value, original_text: Option<&str>, app_name: &str) -> Result<String> {
    let mut rendered = String::new();
    if let Some(header) = original_text.and_then(leading_comment_header) {
        rendered.push_str(header);
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
    }
    rendered.push_str(
        &serde_json::to_string_pretty(root)
            .with_context(|| format!("serialize {app_name} settings"))?,
    );
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

fn write_file_atomically(path: &Path, content: &[u8], description: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create {description} dir {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("{description} path has no file name: {}", path.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("open temp {description} {}", tmp_path.display()))?;
        file.write_all(content)
            .with_context(|| format!("write temp {description} {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp {description} {}", tmp_path.display()))?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("remove existing {description} {}", path.display()))?;
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

    fn into_paseo_at(path: PathBuf, command: &str, force: bool) -> Result<PaseoIntoResult> {
        into_paseo(PaseoIntoOptions {
            settings_path: Some(path),
            agent_name: "yolop".to_string(),
            command: PathBuf::from(command),
            force,
        })
    }

    fn into_buzz_at(path: PathBuf, command: &str, force: bool) -> Result<BuzzIntoResult> {
        into_buzz(BuzzIntoOptions {
            harness_path: Some(path),
            agent_name: "yolop".to_string(),
            command: PathBuf::from(command),
            force,
        })
    }

    #[test]
    fn buzz_into_creates_custom_harness() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("custom_harnesses/yolop.json");

        let result = into_buzz_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Created);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            value,
            json!({
                "name": "yolop",
                "display_name": "yolop",
                "description": "Yolop coding agent",
                "command": "/bin/yolop",
                "args": ["--acp"],
                "env": {}
            })
        );
    }

    #[test]
    fn buzz_into_updates_managed_fields_and_preserves_env_and_extensions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("yolop.json");
        std::fs::write(&path, r#"{"name":"old","display_name":"Old","description":"Old","command":"old","args":["old"],"env":{"API_KEY":"keep"},"icon":"keep"}"#).unwrap();

        let result = into_buzz_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Updated);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["command"], "/bin/yolop");
        assert_eq!(value["args"], json!(["--acp"]));
        assert_eq!(value["env"]["API_KEY"], "keep");
        assert_eq!(value["icon"], "keep");
    }

    #[test]
    fn buzz_into_is_idempotent_with_user_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("yolop.json");
        std::fs::write(&path, r#"{"name":"yolop","display_name":"yolop","description":"Yolop coding agent","command":"/bin/yolop","args":["--acp"],"env":{"API_KEY":"keep"}}"#).unwrap();

        let result = into_buzz_at(path, "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Unchanged);
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
        let parsed = parse_jsonc_settings(&text).expect("parse");
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
        let parsed = parse_jsonc_settings(&std::fs::read_to_string(path).unwrap()).unwrap();
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
    fn paseo_into_creates_acp_provider_in_documented_config_shape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
  "agents": {
    "providers": {
      "claude-work": {
        "extends": "claude",
        "label": "Claude (Work)"
      }
    }
  }
}
"#,
        )
        .expect("write settings");

        let result = into_paseo_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Created);
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            parsed["agents"]["providers"]["claude-work"]["label"],
            "Claude (Work)"
        );
        assert_eq!(
            parsed["agents"]["providers"]["yolop"],
            json!({
                "extends": "acp",
                "label": "yolop",
                "command": ["/bin/yolop", "--acp"]
            })
        );
    }

    #[test]
    fn paseo_into_updates_existing_acp_provider_and_preserves_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agents":{"providers":{"yolop":{"extends":"acp","label":"Old Label","command":["old","--old"],"env":{"OPENAI_API_KEY":"keep"},"enabled":true}}}}"#,
        )
        .expect("write settings");

        let result = into_paseo_at(path.clone(), "/bin/yolop", false).expect("into");

        assert_eq!(result.status, IntoStatus::Updated);
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed["agents"]["providers"]["yolop"]["extends"], "acp");
        assert_eq!(parsed["agents"]["providers"]["yolop"]["label"], "yolop");
        assert_eq!(
            parsed["agents"]["providers"]["yolop"]["command"],
            json!(["/bin/yolop", "--acp"])
        );
        assert_eq!(
            parsed["agents"]["providers"]["yolop"]["env"]["OPENAI_API_KEY"],
            "keep"
        );
        assert_eq!(parsed["agents"]["providers"]["yolop"]["enabled"], true);
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
        let parsed = parse_jsonc_settings(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed["agent_servers"]["yolop"]["command"], "/bin/yolop");
    }
}

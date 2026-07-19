//! Language-server lifecycle and the text/position plumbing LSP requires:
//! per-language lazy spawn, file→language routing, position-encoding
//! conversion (LSP defaults to UTF-16 columns), file URIs, and applying
//! `WorkspaceEdit`s to the workspace disk.

use super::client::LspClient;
use crate::workspace_host::WorkspaceHost;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

pub(crate) const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;
pub(crate) const MAX_REQUEST_TIMEOUT_MS: u64 = 600_000;
pub(crate) const DEFAULT_DIAGNOSTICS_WAIT_MS: u64 = 15_000;
pub(crate) const MAX_DIAGNOSTICS_WAIT_MS: u64 = 120_000;

/// One configured language server: which command to run and which file
/// extensions route to it. `key` doubles as the config-override key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerSpec {
    pub key: String,
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
}

/// Built-in servers for the ecosystems yolop already understands via
/// tree-sitter. All are the de-facto standard binaries; each spawns only when
/// a file with a matching extension is queried and the binary is on PATH.
pub(crate) fn default_server_specs() -> Vec<ServerSpec> {
    let spec = |key: &str, command: &str, args: &[&str], extensions: &[&str]| ServerSpec {
        key: key.to_string(),
        command: command.to_string(),
        args: args.iter().map(ToString::to_string).collect(),
        extensions: extensions.iter().map(ToString::to_string).collect(),
    };
    vec![
        spec("rust", "rust-analyzer", &[], &["rs"]),
        spec(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"],
        ),
        spec("python", "pyright-langserver", &["--stdio"], &["py", "pyi"]),
        spec("go", "gopls", &[], &["go"]),
        spec(
            "clangd",
            "clangd",
            &[],
            &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        ),
    ]
}

/// `languageId` for `textDocument/didOpen`, keyed by file extension.
pub(crate) fn language_id_for_extension(extension: &str) -> &'static str {
    match extension {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        _ => "plaintext",
    }
}

/// Column unit negotiated with the server (`positionEncoding`, LSP 3.17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    pub(crate) fn from_capabilities(capabilities: &Value) -> Self {
        match capabilities.get("positionEncoding").and_then(Value::as_str) {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            // The LSP default when nothing is negotiated.
            _ => Self::Utf16,
        }
    }
}

/// A running language server plus everything needed to talk to it correctly.
pub(crate) struct LanguageServer {
    pub client: LspClient,
    pub capabilities: Value,
    pub encoding: PositionEncoding,
    /// When the server finished its `initialize` handshake. Cross-file
    /// queries retry on empty results while the server is younger than the
    /// warmup window (see `request_retrying_warmup`).
    pub started_at: std::time::Instant,
    // Held only so `kill_on_drop` fires when the manager (and thus this
    // entry) is dropped; absent for in-process test transports.
    _child: Option<tokio::process::Child>,
}

#[cfg(test)]
pub(crate) type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
#[cfg(test)]
pub(crate) type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;
#[cfg(test)]
pub(crate) type TransportFactory =
    Box<dyn Fn(&ServerSpec) -> Result<(BoxRead, BoxWrite)> + Send + Sync + 'static>;

/// A document synced into its language server, ready for position-based
/// requests.
pub(crate) struct OpenedDocument {
    pub server: Arc<LanguageServer>,
    pub uri: String,
    pub text: String,
    pub outcome: super::client::SyncOutcome,
}

pub(crate) struct LspManager {
    pub workspace: Arc<WorkspaceHost>,
    pub specs: Vec<ServerSpec>,
    pub request_timeout: Duration,
    pub diagnostics_wait: Duration,
    servers: Mutex<HashMap<String, Arc<LanguageServer>>>,
    #[cfg(test)]
    transport_factory: Option<TransportFactory>,
}

impl LspManager {
    pub(crate) fn new(
        workspace: Arc<WorkspaceHost>,
        specs: Vec<ServerSpec>,
        request_timeout: Duration,
        diagnostics_wait: Duration,
    ) -> Self {
        Self {
            workspace,
            specs,
            request_timeout,
            diagnostics_wait,
            servers: Mutex::new(HashMap::new()),
            #[cfg(test)]
            transport_factory: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_transport_factory(mut self, factory: TransportFactory) -> Self {
        self.transport_factory = Some(factory);
        self
    }

    /// Resolve a workspace-relative path to (absolute file, its server spec).
    pub(crate) fn resolve_file(&self, path: &str) -> Result<(PathBuf, &ServerSpec)> {
        let root = self
            .workspace
            .host_root()
            .map_err(|err| anyhow!(err.to_string()))?
            .canonicalize()
            .context("canonicalize workspace root")?;
        let file = crate::workspace_host::resolve_host_scope(&root, Some(path))?;
        if !file.is_file() {
            bail!("`{path}` is not a file in the workspace");
        }
        let extension = file
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let spec = self
            .specs
            .iter()
            .find(|spec| spec.extensions.iter().any(|ext| ext == &extension))
            .with_context(|| {
                format!(
                    "no language server configured for `.{extension}` files; \
                     configured servers: {}",
                    self.spec_summary()
                )
            })?;
        Ok((file.into_path_buf(), spec))
    }

    fn spec_summary(&self) -> String {
        let mut parts: Vec<String> = self
            .specs
            .iter()
            .map(|spec| format!("{} ({})", spec.key, spec.extensions.join(", ")))
            .collect();
        parts.sort();
        parts.join("; ")
    }

    /// Get or lazily start the server for `spec`. A server that has exited is
    /// dropped and restarted on the next call.
    pub(crate) async fn language_server(&self, spec: &ServerSpec) -> Result<Arc<LanguageServer>> {
        let mut servers = self.servers.lock().await;
        if let Some(server) = servers.get(&spec.key) {
            if !server.client.is_exited() {
                return Ok(server.clone());
            }
            servers.remove(&spec.key);
        }
        let server = Arc::new(self.start_server(spec).await?);
        servers.insert(spec.key.clone(), server.clone());
        Ok(server)
    }

    async fn start_server(&self, spec: &ServerSpec) -> Result<LanguageServer> {
        let root = self
            .workspace
            .host_root()
            .map_err(|err| anyhow!(err.to_string()))?
            .canonicalize()
            .context("canonicalize workspace root")?;

        #[cfg(test)]
        if let Some(factory) = &self.transport_factory {
            let (reader, writer) = factory(spec)?;
            let client = LspClient::start(reader, writer, self.request_timeout);
            return finish_initialize(client, None, &root).await;
        }

        let mut child = tokio::process::Command::new(&spec.command)
            .args(&spec.args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    anyhow!(
                        "language server `{}` (for `{}`) is not installed or not on PATH; \
                         install it, or point the `lsp` capability at another binary via \
                         `[[capabilities]] ref = \"lsp\"` with `servers.{}.command` in settings.toml",
                        spec.command,
                        spec.key,
                        spec.key
                    )
                } else {
                    anyhow!("spawn language server `{}`: {err}", spec.command)
                }
            })?;
        let stdout = child.stdout.take().context("capture server stdout")?;
        let stdin = child.stdin.take().context("capture server stdin")?;
        let client = LspClient::start(stdout, stdin, self.request_timeout);
        finish_initialize(client, Some(child), &root).await
    }

    /// Sync `path` into its server from disk; returns the server handle plus
    /// the document URI and text needed for position conversions.
    pub(crate) async fn open(&self, path: &str) -> Result<OpenedDocument> {
        let (file, spec) = self.resolve_file(path)?;
        let spec = spec.clone();
        let server = self.language_server(&spec).await?;
        let text =
            std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        let uri = path_to_uri(&file);
        let extension = file
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let outcome = server
            .client
            .sync_document(&uri, language_id_for_extension(&extension), &text)
            .await?;
        Ok(OpenedDocument {
            server,
            uri,
            text,
            outcome,
        })
    }

    /// Any live server, for requests that are not tied to one document
    /// (workspace symbol search without a `hint_path`). Deterministic: picks
    /// the lowest server key.
    pub(crate) async fn any_running_server(&self) -> Option<Arc<LanguageServer>> {
        let servers = self.servers.lock().await;
        let mut keys: Vec<&String> = servers.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|key| servers[key].clone())
            .find(|server| !server.client.is_exited())
    }
}

async fn finish_initialize(
    client: LspClient,
    child: Option<tokio::process::Child>,
    root: &Path,
) -> Result<LanguageServer> {
    let root_uri = path_to_uri(root);
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace")
        .to_string();
    let capabilities = client.initialize(&root_uri, &root_name).await?;
    let encoding = PositionEncoding::from_capabilities(&capabilities);
    Ok(LanguageServer {
        client,
        capabilities,
        encoding,
        started_at: std::time::Instant::now(),
        _child: child,
    })
}

// ---------------------------------------------------------------------------
// File URIs

/// `file://` URI for an absolute path, percent-encoding per RFC 3986.
pub(crate) fn path_to_uri(path: &Path) -> String {
    const UNRESERVED_EXTRA: &[u8] = b"-._~/";
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || UNRESERVED_EXTRA.contains(byte) {
            uri.push(c);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

pub(crate) fn uri_to_path(uri: &str) -> Result<PathBuf> {
    let rest = uri
        .strip_prefix("file://")
        .with_context(|| format!("not a file:// URI: {uri}"))?;
    // Strip an optional authority (`file://localhost/...`).
    let path_part = match rest.strip_prefix("localhost") {
        Some(stripped) => stripped,
        None => rest,
    };
    if !path_part.starts_with('/') {
        bail!("unsupported file URI (no absolute path): {uri}");
    }
    let mut bytes = Vec::with_capacity(path_part.len());
    let mut chars = path_part.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars.next().context("truncated percent escape")?;
            let lo = chars.next().context("truncated percent escape")?;
            let hex = [hi, lo];
            let hex = std::str::from_utf8(&hex).context("invalid percent escape")?;
            bytes.push(u8::from_str_radix(hex, 16).context("invalid percent escape")?);
        } else {
            bytes.push(byte);
        }
    }
    Ok(PathBuf::from(
        String::from_utf8(bytes).context("file URI is not valid UTF-8")?,
    ))
}

// ---------------------------------------------------------------------------
// Position conversion
//
// Tool-facing coordinates are 1-based (line, column) counted in *characters*,
// matching ast_grep/repo_map output. LSP is 0-based with columns in the
// negotiated encoding (UTF-16 by default).

pub(crate) fn line_text(text: &str, line0: usize) -> &str {
    text.split('\n')
        .nth(line0)
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or("")
}

fn encoded_len(c: char, encoding: PositionEncoding) -> usize {
    match encoding {
        PositionEncoding::Utf8 => c.len_utf8(),
        PositionEncoding::Utf16 => c.len_utf16(),
        PositionEncoding::Utf32 => 1,
    }
}

/// (1-based line, 1-based char column) → LSP position in `encoding`.
pub(crate) fn to_lsp_position(
    text: &str,
    line: usize,
    column: usize,
    encoding: PositionEncoding,
) -> Result<Value> {
    if line == 0 || column == 0 {
        bail!("`line` and `column` are 1-based and must be >= 1");
    }
    let line0 = line - 1;
    let line_str = line_text(text, line0);
    let mut character = 0usize;
    for (index, c) in line_str.chars().enumerate() {
        if index + 1 >= column {
            break;
        }
        character += encoded_len(c, encoding);
    }
    Ok(serde_json::json!({ "line": line0, "character": character }))
}

/// LSP position → (1-based line, 1-based char column).
pub(crate) fn from_lsp_position(
    text: &str,
    position: &Value,
    encoding: PositionEncoding,
) -> (usize, usize) {
    let line0 = position.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let target = position
        .get("character")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let line_str = line_text(text, line0);
    let mut consumed = 0usize;
    let mut column0 = 0usize;
    for c in line_str.chars() {
        if consumed >= target {
            break;
        }
        consumed += encoded_len(c, encoding);
        column0 += 1;
    }
    (line0 + 1, column0 + 1)
}

/// LSP position → byte offset into `text` (for applying edits).
pub(crate) fn position_to_byte_offset(
    text: &str,
    position: &Value,
    encoding: PositionEncoding,
) -> usize {
    let line0 = position.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let target = position
        .get("character")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let mut offset = 0usize;
    let mut current_line = 0usize;
    let mut chars = text.char_indices().peekable();
    // Advance to the start of the target line.
    while current_line < line0 {
        match chars.next() {
            Some((index, '\n')) => {
                current_line += 1;
                offset = index + 1;
            }
            Some(_) => {}
            None => return text.len(),
        }
    }
    let mut consumed = 0usize;
    for (index, c) in chars {
        if consumed >= target || c == '\n' {
            return index;
        }
        consumed += encoded_len(c, encoding);
    }
    text.len().max(offset)
}

// ---------------------------------------------------------------------------
// WorkspaceEdit application

#[derive(Debug, serde::Serialize)]
pub(crate) struct FileEditSummary {
    pub path: String,
    pub operation: String,
    pub edits: usize,
}

/// Apply an LSP `WorkspaceEdit` to disk, workspace-scoped. Prefers
/// `documentChanges` over `changes` per the spec. Resource operations
/// (create/rename/delete) are supported; embedded commands are not.
pub(crate) async fn apply_workspace_edit(
    manager: &LspManager,
    edit: &Value,
    encoding: PositionEncoding,
    server: &LanguageServer,
) -> Result<Vec<FileEditSummary>> {
    let root = manager
        .workspace
        .host_root()
        .map_err(|err| anyhow!(err.to_string()))?
        .canonicalize()
        .context("canonicalize workspace root")?;
    let mut summaries = Vec::new();

    if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                summaries.push(apply_resource_operation(&root, kind, change)?);
            } else {
                let uri = change
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .context("documentChanges entry without textDocument.uri")?;
                let edits = change
                    .get("edits")
                    .and_then(Value::as_array)
                    .context("documentChanges entry without edits")?;
                summaries.push(apply_text_edits(&root, uri, edits, encoding, server).await?);
            }
        }
        return Ok(summaries);
    }

    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        // Deterministic order for stable output and tests.
        let mut uris: Vec<&String> = changes.keys().collect();
        uris.sort();
        for uri in uris {
            let edits = changes[uri]
                .as_array()
                .context("changes entry must be an edit array")?;
            summaries.push(apply_text_edits(&root, uri, edits, encoding, server).await?);
        }
    }
    Ok(summaries)
}

fn workspace_relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Reject any URI that resolves outside the workspace root. `..`/`.` segments
/// are normalized lexically first (so a not-yet-existing target cannot smuggle
/// them past the prefix check), then the nearest existing ancestor is
/// canonicalized so symlinks cannot escape either.
fn workspace_file(root: &Path, uri: &str) -> Result<PathBuf> {
    use std::path::Component;
    let raw = uri_to_path(uri)?;
    let mut path = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => path.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if !path.pop() {
                    bail!("edit target escapes the filesystem root: {}", raw.display());
                }
            }
            Component::Normal(part) => path.push(part),
        }
    }
    let mut probe = path.clone();
    let mut suffix = Vec::new();
    let canonical = loop {
        match probe.canonicalize() {
            Ok(canonical) => break canonical,
            Err(_) => {
                let Some(name) = probe.file_name() else {
                    bail!("cannot resolve edit target {}", path.display());
                };
                suffix.push(name.to_owned());
                probe = probe
                    .parent()
                    .map(Path::to_path_buf)
                    .context("edit target has no parent")?;
            }
        }
    };
    let mut resolved = canonical;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    if !resolved.starts_with(root) {
        bail!(
            "server proposed an edit outside the workspace: {}",
            path.display()
        );
    }
    // Workspace edits bypass the platform filesystem stack (they come from
    // the language server, not a file tool), so enforce the same write
    // blocklist here: no mutations inside vendored/build directories.
    for component in resolved
        .strip_prefix(root)
        .unwrap_or(&resolved)
        .components()
    {
        if let Component::Normal(name) = component
            && let Some(name) = name.to_str()
            && everruns_runtime::DEFAULT_WRITE_BLOCKLIST.contains(&name)
        {
            bail!(
                "server proposed an edit inside the protected directory `{name}`: {}",
                path.display()
            );
        }
    }
    Ok(resolved)
}

fn apply_resource_operation(root: &Path, kind: &str, change: &Value) -> Result<FileEditSummary> {
    match kind {
        "create" => {
            let uri = change
                .get("uri")
                .and_then(Value::as_str)
                .context("create without uri")?;
            let file = workspace_file(root, uri)?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).context("create parent directories")?;
            }
            let overwrite = change
                .pointer("/options/overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let ignore_if_exists = change
                .pointer("/options/ignoreIfExists")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if file.exists() && !overwrite {
                if !ignore_if_exists {
                    bail!("create target already exists: {}", file.display());
                }
            } else {
                std::fs::write(&file, "").context("create file")?;
            }
            Ok(FileEditSummary {
                path: workspace_relative(root, &file),
                operation: "create".to_string(),
                edits: 0,
            })
        }
        "rename" => {
            let old = change
                .get("oldUri")
                .and_then(Value::as_str)
                .context("rename without oldUri")?;
            let new = change
                .get("newUri")
                .and_then(Value::as_str)
                .context("rename without newUri")?;
            let old_file = workspace_file(root, old)?;
            let new_file = workspace_file(root, new)?;
            if let Some(parent) = new_file.parent() {
                std::fs::create_dir_all(parent).context("create parent directories")?;
            }
            std::fs::rename(&old_file, &new_file).context("rename file")?;
            Ok(FileEditSummary {
                path: format!(
                    "{} -> {}",
                    workspace_relative(root, &old_file),
                    workspace_relative(root, &new_file)
                ),
                operation: "rename".to_string(),
                edits: 0,
            })
        }
        "delete" => {
            let uri = change
                .get("uri")
                .and_then(Value::as_str)
                .context("delete without uri")?;
            let file = workspace_file(root, uri)?;
            let recursive = change
                .pointer("/options/recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if file.is_dir() {
                if recursive {
                    std::fs::remove_dir_all(&file).context("delete directory")?;
                } else {
                    std::fs::remove_dir(&file).context("delete directory")?;
                }
            } else {
                std::fs::remove_file(&file).context("delete file")?;
            }
            Ok(FileEditSummary {
                path: workspace_relative(root, &file),
                operation: "delete".to_string(),
                edits: 0,
            })
        }
        other => bail!("unsupported workspace-edit resource operation: {other}"),
    }
}

async fn apply_text_edits(
    root: &Path,
    uri: &str,
    edits: &[Value],
    encoding: PositionEncoding,
    server: &LanguageServer,
) -> Result<FileEditSummary> {
    let file = workspace_file(root, uri)?;
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("read edit target {}", file.display()))?;
    let updated = apply_text_edits_to_string(&text, edits, encoding)?;
    std::fs::write(&file, &updated).with_context(|| format!("write {}", file.display()))?;
    // Keep the server's view current if we had the document open.
    let extension = file
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let canonical_uri = path_to_uri(&file);
    let _ = server
        .client
        .sync_document(
            &canonical_uri,
            language_id_for_extension(&extension),
            &updated,
        )
        .await;
    Ok(FileEditSummary {
        path: workspace_relative(root, &file),
        operation: "edit".to_string(),
        edits: edits.len(),
    })
}

pub(crate) fn apply_text_edits_to_string(
    text: &str,
    edits: &[Value],
    encoding: PositionEncoding,
) -> Result<String> {
    let mut spans: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit.get("range").context("text edit without range")?;
        let start = range.get("start").context("range without start")?;
        let end = range.get("end").context("range without end")?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .context("text edit without newText")?;
        let start_offset = position_to_byte_offset(text, start, encoding);
        let end_offset = position_to_byte_offset(text, end, encoding);
        if end_offset < start_offset {
            bail!("text edit range is inverted");
        }
        spans.push((start_offset, end_offset, new_text));
    }
    // Apply back-to-front so earlier offsets stay valid.
    spans.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    for window in spans.windows(2) {
        // After the descending sort, the *next* span ends before this starts.
        if window[1].1 > window[0].0 {
            bail!("text edits overlap; refusing to apply");
        }
    }
    let mut result = text.to_string();
    for (start, end, new_text) in spans {
        result.replace_range(start..end, new_text);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uri_roundtrip_with_spaces_and_unicode() {
        let path = Path::new("/tmp/my project/naïve file.rs");
        let uri = path_to_uri(path);
        assert!(uri.starts_with("file:///tmp/my%20project/"), "{uri}");
        assert_eq!(uri_to_path(&uri).expect("parse"), path);
    }

    #[test]
    fn uri_to_path_rejects_non_file() {
        assert!(uri_to_path("https://example.com/x").is_err());
    }

    #[test]
    fn utf16_position_conversion_handles_wide_chars() {
        // "a𐐀b" — 𐐀 is 2 UTF-16 units and 4 UTF-8 bytes.
        let text = "a𐐀b\nplain";
        let pos = to_lsp_position(text, 1, 3, PositionEncoding::Utf16).expect("pos");
        assert_eq!(pos, json!({ "line": 0, "character": 3 }));
        let pos8 = to_lsp_position(text, 1, 3, PositionEncoding::Utf8).expect("pos");
        assert_eq!(pos8, json!({ "line": 0, "character": 5 }));

        let (line, column) = from_lsp_position(
            text,
            &json!({ "line": 0, "character": 3 }),
            PositionEncoding::Utf16,
        );
        assert_eq!((line, column), (1, 3));
        let (line, column) = from_lsp_position(
            text,
            &json!({ "line": 1, "character": 2 }),
            PositionEncoding::Utf16,
        );
        assert_eq!((line, column), (2, 3));
    }

    #[test]
    fn byte_offset_clamps_past_line_end() {
        let text = "ab\ncd";
        let offset = position_to_byte_offset(
            text,
            &json!({ "line": 0, "character": 99 }),
            PositionEncoding::Utf16,
        );
        assert_eq!(offset, 2); // end of "ab", before the newline
        let offset = position_to_byte_offset(
            text,
            &json!({ "line": 5, "character": 0 }),
            PositionEncoding::Utf16,
        );
        assert_eq!(offset, text.len());
    }

    #[test]
    fn text_edits_apply_back_to_front() {
        let text = "let old = old + 1;";
        let edit = |start: u64, end: u64| {
            json!({
                "range": {
                    "start": { "line": 0, "character": start },
                    "end": { "line": 0, "character": end }
                },
                "newText": "renamed"
            })
        };
        let updated =
            apply_text_edits_to_string(text, &[edit(4, 7), edit(10, 13)], PositionEncoding::Utf16)
                .expect("apply");
        assert_eq!(updated, "let renamed = renamed + 1;");
    }

    #[test]
    fn overlapping_text_edits_are_rejected() {
        let text = "abcdef";
        let edit = |start: u64, end: u64| {
            json!({
                "range": {
                    "start": { "line": 0, "character": start },
                    "end": { "line": 0, "character": end }
                },
                "newText": "x"
            })
        };
        let err =
            apply_text_edits_to_string(text, &[edit(0, 3), edit(2, 5)], PositionEncoding::Utf16)
                .expect_err("overlap");
        assert!(err.to_string().contains("overlap"), "{err}");
    }

    #[test]
    fn workspace_file_rejects_escapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let outside = path_to_uri(Path::new("/etc/passwd"));
        assert!(workspace_file(&root, &outside).is_err());
        let sneaky = path_to_uri(&root.join("sub/../../escape.txt"));
        assert!(workspace_file(&root, &sneaky).is_err());
        let inside = path_to_uri(&root.join("new/file.rs"));
        assert!(workspace_file(&root, &inside).is_ok());
    }

    /// Workspace edits skip the platform filesystem stack, so the write
    /// blocklist must be enforced here too — a server-proposed edit into
    /// `.git/` or a vendored/build directory is rejected at any depth.
    #[test]
    fn workspace_file_rejects_write_blocklisted_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        for blocked in [
            ".git/config",
            "node_modules/pkg/index.js",
            "sub/target/out.rs",
        ] {
            let uri = path_to_uri(&root.join(blocked));
            let err = workspace_file(&root, &uri).expect_err(blocked);
            assert!(err.to_string().contains("protected directory"), "{err}");
        }
        // A file merely *named* like a blocklist entry is fine.
        let file = path_to_uri(&root.join("targets.rs"));
        assert!(workspace_file(&root, &file).is_ok());
    }

    #[test]
    fn default_specs_cover_expected_extensions() {
        let specs = default_server_specs();
        let find = |ext: &str| {
            specs
                .iter()
                .find(|spec| spec.extensions.iter().any(|e| e == ext))
                .map(|spec| spec.key.as_str())
        };
        assert_eq!(find("rs"), Some("rust"));
        assert_eq!(find("tsx"), Some("typescript"));
        assert_eq!(find("py"), Some("python"));
        assert_eq!(find("go"), Some("go"));
        assert_eq!(find("cpp"), Some("clangd"));
        assert_eq!(find("zig"), None);
    }
}

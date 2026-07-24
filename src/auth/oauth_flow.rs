use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use rand::RngExt;
use sha2::{Digest, Sha256};

/// Open `url` in the user's default browser. Shared by the interactive OAuth
/// login flows (Codex provider auth, MCP server auth).
pub fn open_browser(url: &str) -> Result<()> {
    // Detach the opener's stdio: a browser launcher that prints to the inherited
    // terminal (some `xdg-open` shims are chatty) would corrupt the TUI frame.
    let mut command = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let status = crate::exec::proc::detach_stdio(&mut command)
        .status()
        .context("open browser for login")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("browser opener exited with {status}"))
    }
}

pub fn random_pkce_verifier() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..64)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

pub fn random_token(bytes: usize) -> String {
    let mut data = vec![0u8; bytes];
    rand::rng().fill(data.as_mut_slice());
    base64_url(&data)
}

pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url(&digest)
}

pub fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn callback_page(status: &str, message: &str, connection_label: &str) -> String {
    let ok = status.starts_with("200");
    let title = if ok {
        format!("Yolop {connection_label} login complete")
    } else {
        format!("Yolop {connection_label} login needs attention")
    };
    let eyebrow = if ok { "Signed in" } else { "Login interrupted" };
    let fun = if ok {
        "Your terminal is already warming up the keyboard."
    } else {
        "The terminal kept your seat warm. Try the login flow again when ready."
    };
    let class = if ok { "success" } else { "error" };
    let headline = if ok {
        format!("{connection_label} is connected.")
    } else {
        "Almost there.".to_string()
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <style>
    :root {{
      color-scheme: light dark;
      --ink: #12131a;
      --muted: #626776;
      --panel: rgba(255, 255, 255, 0.86);
      --line: rgba(18, 19, 26, 0.12);
      --gold: #d4a43a;
      --navy: #0a1636;
      --bad: #b74747;
    }}
    * {{ box-sizing: border-box; }}
    html, body {{ min-height: 100%; }}
    body {{
      margin: 0;
      font: 16px/1.5 ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      color: var(--ink);
      background:
        radial-gradient(circle at 18% 12%, rgba(212, 164, 58, 0.22), transparent 28rem),
        radial-gradient(circle at 85% 18%, rgba(10, 22, 54, 0.14), transparent 30rem),
        linear-gradient(135deg, #fbfaf7 0%, #eef1f7 100%);
      display: grid;
      place-items: center;
      padding: 32px;
    }}
    main {{
      width: min(720px, 100%);
      border: 1px solid var(--line);
      border-radius: 28px;
      padding: clamp(28px, 6vw, 56px);
      background: var(--panel);
      box-shadow: 0 24px 80px rgba(10, 22, 54, 0.16);
      backdrop-filter: blur(20px);
    }}
    .logo {{
      width: 88px;
      height: 88px;
      display: grid;
      place-items: center;
      border-radius: 24px;
      background: #fff;
      box-shadow: inset 0 0 0 1px var(--line), 0 12px 32px rgba(10, 22, 54, 0.12);
      margin-bottom: 28px;
    }}
    .logo svg {{ width: 62px; height: 62px; display: block; }}
    .eyebrow {{
      margin: 0 0 10px;
      color: {accent};
      font-weight: 700;
      text-transform: uppercase;
      font-size: 0.82rem;
    }}
    h1 {{
      margin: 0;
      font-size: clamp(2rem, 7vw, 4.25rem);
      line-height: 0.96;
    }}
    .message {{
      margin: 24px 0 0;
      color: var(--muted);
      font-size: clamp(1rem, 2.2vw, 1.2rem);
      max-width: 38rem;
    }}
    .next {{
      margin-top: 34px;
      padding: 18px 20px;
      border-radius: 16px;
      background: rgba(10, 22, 54, 0.06);
      color: var(--navy);
      font-weight: 650;
    }}
    .success .next {{ border-left: 5px solid var(--gold); }}
    .error .next {{ border-left: 5px solid var(--bad); }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --ink: #f4f5fb;
        --muted: #b8bdcc;
        --panel: rgba(24, 25, 34, 0.88);
        --line: rgba(255, 255, 255, 0.12);
        --navy: #f4f5fb;
      }}
      body {{
        background:
          radial-gradient(circle at 18% 12%, rgba(212, 164, 58, 0.18), transparent 28rem),
          radial-gradient(circle at 85% 18%, rgba(94, 116, 184, 0.18), transparent 30rem),
          linear-gradient(135deg, #101118 0%, #171927 100%);
      }}
      .logo {{ background: rgba(255, 255, 255, 0.08); }}
      .next {{ background: rgba(255, 255, 255, 0.08); }}
    }}
  </style>
</head>
<body>
  <main class="{class}">
    <div class="logo" aria-label="Yolop logo">{logo}</div>
    <p class="eyebrow">{eyebrow}</p>
    <h1>{headline}</h1>
    <p class="message">{message}</p>
    <div class="next">{fun}</div>
  </main>
</body>
</html>"#,
        title = html_escape(&title),
        accent = if ok { "var(--gold)" } else { "var(--bad)" },
        class = class,
        logo = include_str!("../../logo.svg"),
        eyebrow = html_escape(eyebrow),
        headline = html_escape(&headline),
        message = html_escape(message),
        fun = html_escape(fun),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_verifier_is_valid_length_and_charset() {
        let verifier = random_pkce_verifier();
        assert_eq!(verifier.len(), 64);
        assert!(
            verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'.' | b'_' | b'~'))
        );
    }

    #[test]
    fn random_token_uses_unpadded_base64_url() {
        let token = random_token(32);
        assert!(!token.contains('='));
        assert!(!token.contains('+'));
        assert!(!token.contains('/'));
    }

    #[test]
    fn jwt_payload_decodes_middle_segment() {
        let payload = serde_json::json!({"sub": "user_123"});
        let token = format!(
            "header.{}.sig",
            base64_url(&serde_json::to_vec(&payload).unwrap())
        );
        assert_eq!(jwt_payload(&token), Some(payload));
    }

    #[test]
    fn html_escape_escapes_callback_message_characters() {
        assert_eq!(html_escape("<&>\"ok"), "&lt;&amp;&gt;&quot;ok");
    }

    #[test]
    fn callback_page_renders_connection_and_escapes_content() {
        let page = callback_page("200 OK", "Connected to <tools>.", "MCP server");

        assert!(page.contains("<svg"));
        assert!(page.contains("MCP server is connected."));
        assert!(page.contains("Connected to &lt;tools&gt;."));
        assert!(!page.contains("Connected to <tools>."));
    }
}

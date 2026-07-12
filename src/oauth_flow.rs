use base64::Engine as _;
use rand::RngExt;
use sha2::{Digest, Sha256};

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
}

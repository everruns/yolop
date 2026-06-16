// Load local image files into multimodal user-message content parts.
//
// Mirrors Codex CLI's `--image` / `-i` flow: read supported formats from
// disk, base64-encode, and attach as `ContentPart::Image` before the text
// prompt (models tend to prefer definitions before references).

use anyhow::{Context, Result, bail};
use base64::Engine;
use everruns_core::message::{ContentPart, ImageContentPart};
use std::path::{Path, PathBuf};

/// Conservative per-file cap — large enough for screenshots, small enough
/// to avoid accidentally loading multi-hundred-MB assets into context.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Load one or more local image paths into content parts (images only).
pub fn load_image_parts(paths: &[PathBuf]) -> Result<Vec<ContentPart>> {
    paths
        .iter()
        .map(|path| load_image_part(path))
        .collect::<Result<Vec<_>>>()
}

/// Build a `ContentPart::Image` from already-encoded image bytes.
pub fn image_part_from_encoded(bytes: &[u8], media_type: &str) -> Result<ContentPart> {
    if bytes.is_empty() {
        bail!("image is empty");
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("image is {} bytes (max {})", bytes.len(), MAX_IMAGE_BYTES);
    }
    let base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(ContentPart::Image(ImageContentPart::from_base64(
        base64, media_type,
    )))
}

/// Read a single image file and return a base64 `ContentPart::Image`.
pub fn load_image_part(path: &Path) -> Result<ContentPart> {
    let bytes = std::fs::read(path).with_context(|| format!("read image {}", path.display()))?;
    let media_type = detect_media_type(path, &bytes)?;
    image_part_from_encoded(&bytes, &media_type)
}

/// Build user-facing transcript text that mentions attached images.
pub fn user_display_text(text: &str, image_count: usize) -> String {
    if image_count == 0 {
        return text.to_string();
    }
    let labels: Vec<String> = (1..=image_count)
        .map(|index| format!("[Image #{index}]"))
        .collect();
    let prefix = labels.join(" ");
    if text.trim().is_empty() {
        prefix
    } else {
        format!("{prefix} {text}")
    }
}

fn detect_media_type(path: &Path, bytes: &[u8]) -> Result<String> {
    if let Some(mime) = sniff_media_type(bytes) {
        return Ok(mime);
    }
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png".into()),
        Some("jpg") | Some("jpeg") => Ok("image/jpeg".into()),
        Some("gif") => Ok("image/gif".into()),
        Some("webp") => Ok("image/webp".into()),
        _ => bail!(
            "unsupported image format for {} (supported: png, jpeg, gif, webp)",
            path.display()
        ),
    }
}

fn sniff_media_type(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png".into());
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg".into());
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif".into());
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_png_magic_bytes() {
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert_eq!(sniff_media_type(png).as_deref(), Some("image/png"));
    }

    #[test]
    fn user_display_text_labels_images() {
        assert_eq!(
            user_display_text("describe this", 2),
            "[Image #1] [Image #2] describe this"
        );
        assert_eq!(user_display_text("", 1), "[Image #1]");
        assert_eq!(user_display_text("plain", 0), "plain");
    }

    #[test]
    fn load_image_part_rejects_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.png");
        std::fs::write(&path, b"").expect("write empty");
        let err = load_image_part(&path).expect_err("empty image should fail");
        assert!(err.to_string().contains("empty"));
    }
}

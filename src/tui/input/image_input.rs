// Load local image files into multimodal user-message content parts.
//
// Mirrors Codex CLI's `--image` / `-i` flow: read supported formats from
// disk, base64-encode, and attach as `ContentPart::Image` before the text
// prompt (models tend to prefer definitions before references).

use anyhow::{Context, Result, bail};
use base64::Engine;
use everruns_core::{ContentPart, ImageContentPart};
use std::path::{Path, PathBuf};

/// Conservative per-file cap — large enough for screenshots, small enough
/// to avoid accidentally loading multi-hundred-MB assets into context.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

const SUPPORTED_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

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

/// Decode and validate an inline base64 image before constructing a content part.
pub fn image_part_from_base64(
    data: &str,
    media_type: &str,
    max_bytes: usize,
) -> Result<(ContentPart, usize)> {
    if !SUPPORTED_MEDIA_TYPES.contains(&media_type) {
        bail!("unsupported image media type `{media_type}`");
    }

    // Reject oversized encoded input before the decoder reserves memory. The
    // decoded length is checked again because padding can make this estimate
    // imprecise by up to two bytes.
    let max_encoded_len = max_bytes.div_ceil(3).saturating_mul(4);
    if data.len() > max_encoded_len {
        bail!("encoded image exceeds the {max_bytes}-byte limit");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("invalid base64 image data")?;
    if bytes.len() > max_bytes {
        bail!("image is {} bytes (max {})", bytes.len(), max_bytes);
    }
    let len = bytes.len();
    Ok((image_part_from_encoded(&bytes, media_type)?, len))
}

/// Read a single image file and return a base64 `ContentPart::Image`.
pub fn load_image_part(path: &Path) -> Result<ContentPart> {
    let bytes = std::fs::read(path).with_context(|| format!("read image {}", path.display()))?;
    let media_type = detect_media_type(path, &bytes)?;
    image_part_from_encoded(&bytes, &media_type)
        .with_context(|| format!("image {}", path.display()))
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

    #[test]
    fn inline_image_rejects_invalid_base64_and_unsupported_media() {
        assert!(image_part_from_base64("not base64", "image/png", 20).is_err());
        assert!(image_part_from_base64("ZmFrZQ==", "image/svg+xml", 20).is_err());
    }

    #[test]
    fn inline_image_rejects_empty_and_oversized_data() {
        assert!(image_part_from_base64("", "image/png", 20).is_err());
        assert!(image_part_from_base64("ZmFrZQ==", "image/png", 3).is_err());
    }
}

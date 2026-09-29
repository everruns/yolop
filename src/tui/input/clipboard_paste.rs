// Clipboard image paste for the interactive TUI (Ctrl+V).
//
// Follows Codex CLI's approach: read image bytes or file paths from the
// system clipboard via `arboard`, normalize to PNG, and attach as a
// multimodal user-message part.

use crate::tui::input::image_input::MAX_IMAGE_BYTES;
use everruns_core::ContentPart;
use image::ImageEncoder;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

const MAX_IMAGE_DIMENSION: u32 = 8_192;
const MAX_IMAGE_PIXELS: u64 = (MAX_IMAGE_BYTES / 4) as u64;

#[derive(Debug, Clone)]
pub struct PastedImageInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub enum PasteImageError {
    ClipboardUnavailable(String),
    NoImage(String),
    EncodeFailed(String),
    #[cfg(target_os = "linux")]
    IoError(String),
}

impl std::fmt::Display for PasteImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClipboardUnavailable(msg) => write!(f, "clipboard unavailable: {msg}"),
            Self::NoImage(msg) => write!(f, "no image on clipboard: {msg}"),
            Self::EncodeFailed(msg) => write!(f, "could not encode image: {msg}"),
            #[cfg(target_os = "linux")]
            Self::IoError(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for PasteImageError {}

fn part_from_encoded(bytes: &[u8], media_type: &str) -> Result<ContentPart, PasteImageError> {
    crate::tui::input::image_input::image_part_from_encoded(bytes, media_type)
        .map_err(|err| PasteImageError::EncodeFailed(err.to_string()))
}

/// Read an image from the clipboard and return a PNG content part plus dimensions.
#[cfg(not(target_os = "android"))]
pub fn paste_image_content_part() -> Result<(ContentPart, PastedImageInfo), PasteImageError> {
    match paste_image_as_png() {
        Ok(result) => {
            let (png, info) = result;
            let part = part_from_encoded(&png, "image/png")?;
            Ok((part, info))
        }
        Err(err) => {
            #[cfg(target_os = "linux")]
            {
                if matches!(
                    &err,
                    PasteImageError::ClipboardUnavailable(_) | PasteImageError::NoImage(_)
                ) && let Ok((path, info)) = try_wsl_clipboard_png()
                {
                    let bytes = read_encoded_file_limited(&path)?;
                    let part = part_from_encoded(&bytes, "image/png")?;
                    return Ok((part, info));
                }
            }
            Err(err)
        }
    }
}

#[cfg(target_os = "android")]
pub fn paste_image_content_part() -> Result<(ContentPart, PastedImageInfo), PasteImageError> {
    Err(PasteImageError::ClipboardUnavailable(
        "clipboard image paste is unsupported on Android".into(),
    ))
}

#[cfg(not(target_os = "android"))]
fn paste_image_as_png() -> Result<(Vec<u8>, PastedImageInfo), PasteImageError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|err| PasteImageError::ClipboardUnavailable(err.to_string()))?;

    let files = clipboard
        .get()
        .file_list()
        .map_err(|err| PasteImageError::ClipboardUnavailable(err.to_string()));

    let files = files.unwrap_or_default();
    let dynamic = if !files.is_empty() {
        let mut last_error = None;
        let mut decoded = None;
        for path in files {
            match decode_image_file(&path) {
                Ok(image) => {
                    decoded = Some(image);
                    break;
                }
                Err(err) => last_error = Some(err),
            }
        }
        decoded.ok_or_else(|| {
            last_error.unwrap_or_else(|| PasteImageError::NoImage("empty file list".into()))
        })?
    } else {
        let img = clipboard
            .get_image()
            .map_err(|err| PasteImageError::NoImage(err.to_string()))?;
        let width: u32 = img
            .width
            .try_into()
            .map_err(|_| PasteImageError::EncodeFailed("clipboard image too wide".into()))?;
        let height: u32 = img
            .height
            .try_into()
            .map_err(|_| PasteImageError::EncodeFailed("clipboard image too tall".into()))?;
        validate_dimensions(width, height)?;
        let expected_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| PasteImageError::EncodeFailed("clipboard image is too large".into()))?;
        if img.bytes.len() as u64 != expected_len {
            return Err(PasteImageError::EncodeFailed(
                "invalid RGBA buffer from clipboard".into(),
            ));
        }
        let Some(rgba) = image::RgbaImage::from_raw(width, height, img.bytes.into_owned()) else {
            return Err(PasteImageError::EncodeFailed(
                "invalid RGBA buffer from clipboard".into(),
            ));
        };
        image::DynamicImage::ImageRgba8(rgba)
    };

    encode_dynamic_image_png(&dynamic)
}

#[cfg(not(target_os = "android"))]
fn encode_dynamic_image_png(
    image: &image::DynamicImage,
) -> Result<(Vec<u8>, PastedImageInfo), PasteImageError> {
    validate_dimensions(image.width(), image.height())?;
    let mut output = SizeLimitedWriter::new(MAX_IMAGE_BYTES);
    image::codecs::png::PngEncoder::new(&mut output)
        .write_image(
            image.as_bytes(),
            image.width(),
            image.height(),
            image.color().into(),
        )
        .map_err(|err| PasteImageError::EncodeFailed(err.to_string()))?;
    let png = output.into_inner();
    Ok((
        png,
        PastedImageInfo {
            width: image.width(),
            height: image.height(),
        },
    ))
}

#[cfg(not(target_os = "android"))]
fn validate_dimensions(width: u32, height: u32) -> Result<(), PasteImageError> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
    {
        return Err(PasteImageError::EncodeFailed(format!(
            "clipboard image dimensions {width}x{height} exceed the safety limit"
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn decode_image_file(path: &Path) -> Result<image::DynamicImage, PasteImageError> {
    let file =
        std::fs::File::open(path).map_err(|err| PasteImageError::EncodeFailed(err.to_string()))?;
    let source_len = file
        .metadata()
        .map_err(|err| PasteImageError::EncodeFailed(err.to_string()))?
        .len();
    if source_len > MAX_IMAGE_BYTES as u64 {
        return Err(PasteImageError::EncodeFailed(format!(
            "clipboard image exceeds {MAX_IMAGE_BYTES} byte source limit"
        )));
    }

    let mut reader = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|err| PasteImageError::EncodeFailed(err.to_string()))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|err| PasteImageError::EncodeFailed(err.to_string()))?;
    validate_dimensions(image.width(), image.height())?;
    Ok(image)
}

#[cfg(target_os = "linux")]
fn read_encoded_file_limited(path: &Path) -> Result<Vec<u8>, PasteImageError> {
    let file =
        std::fs::File::open(path).map_err(|err| PasteImageError::IoError(err.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| PasteImageError::IoError(err.to_string()))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(PasteImageError::EncodeFailed(format!(
            "clipboard image exceeds {MAX_IMAGE_BYTES} byte limit"
        )));
    }
    Ok(bytes)
}

#[cfg(not(target_os = "android"))]
struct SizeLimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

#[cfg(not(target_os = "android"))]
impl SizeLimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(not(target_os = "android"))]
impl Write for SizeLimitedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if buf.len() > remaining {
            return Err(io::Error::other(
                "encoded clipboard image exceeds size limit",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Linux/WSL fallback: ask Windows PowerShell to dump the clipboard image to a temp PNG.
#[cfg(all(not(target_os = "android"), target_os = "linux"))]
pub(crate) fn try_wsl_clipboard_png() -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    if !is_probably_wsl() {
        return Err(PasteImageError::ClipboardUnavailable(
            "not running under WSL".into(),
        ));
    }

    let script = r#"[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $img = Get-Clipboard -Format Image; if ($img -ne $null) { $p = Join-Path $env:TEMP ("yolop-clipboard-" + [guid]::NewGuid().ToString() + ".png"); $img.Save($p,[System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p } else { exit 1 }"#;

    for cmd in ["powershell.exe", "pwsh", "powershell"] {
        let Ok(output) = std::process::Command::new(cmd)
            .args(["-NoProfile", "-Command", script])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let win_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if win_path.is_empty() {
            continue;
        }
        let Some(path) = convert_windows_path_to_wsl(&win_path) else {
            continue;
        };
        let source_len = std::fs::metadata(&path)
            .map_err(|err| PasteImageError::IoError(err.to_string()))?
            .len();
        if source_len > MAX_IMAGE_BYTES as u64 {
            return Err(PasteImageError::EncodeFailed(format!(
                "clipboard image exceeds {MAX_IMAGE_BYTES} byte source limit"
            )));
        }
        let (width, height) = image::image_dimensions(&path)
            .map_err(|err| PasteImageError::IoError(err.to_string()))?;
        validate_dimensions(width, height)?;
        return Ok((path, PastedImageInfo { width, height }));
    }

    Err(PasteImageError::NoImage(
        "Windows clipboard did not contain an image".into(),
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn is_probably_wsl() -> bool {
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let version = version.to_lowercase();
        if version.contains("microsoft") || version.contains("wsl") {
            return true;
        }
    }
    std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some()
}

#[cfg(target_os = "linux")]
fn convert_windows_path_to_wsl(input: &str) -> Option<PathBuf> {
    if input.starts_with("\\\\") {
        return None;
    }
    let mut chars = input.chars();
    let drive = chars.next()?.to_ascii_lowercase();
    if !drive.is_ascii_alphabetic() || input.get(1..2) != Some(":") {
        return None;
    }
    let mut result = PathBuf::from(format!("/mnt/{drive}"));
    for component in input
        .get(2..)?
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/'])
        .filter(|component| !component.is_empty())
    {
        result.push(component);
    }
    Some(result)
}

/// Read plain text from the clipboard.
#[cfg(not(target_os = "android"))]
pub fn paste_clipboard_text() -> Result<String, PasteImageError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|err| PasteImageError::ClipboardUnavailable(err.to_string()))?;
    clipboard
        .get_text()
        .map_err(|err| PasteImageError::NoImage(err.to_string()))
}

#[cfg(target_os = "android")]
pub fn paste_clipboard_text() -> Result<String, PasteImageError> {
    Err(PasteImageError::ClipboardUnavailable(
        "clipboard text paste is unsupported on Android".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_dynamic_image_png_respects_size_limit() {
        let image = image::DynamicImage::new_rgba8(4, 4);
        let (png, info) = encode_dynamic_image_png(&image).expect("encode png");
        assert!(!png.is_empty());
        assert_eq!(info.width, 4);
        assert_eq!(info.height, 4);
    }

    #[test]
    fn rejects_images_over_pixel_limit_before_encoding() {
        let error = validate_dimensions(4_096, 4_096).expect_err("reject oversized image");
        assert!(error.to_string().contains("dimensions 4096x4096"));
    }

    #[test]
    fn size_limited_writer_never_grows_past_limit() {
        let mut writer = SizeLimitedWriter::new(4);
        writer.write_all(&[1, 2, 3, 4]).expect("write at limit");
        assert!(writer.write_all(&[5]).is_err());
        assert_eq!(writer.into_inner(), vec![1, 2, 3, 4]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn convert_windows_path_to_wsl_maps_drive_paths() {
        let path = convert_windows_path_to_wsl(r"C:\Users\Alice\shot.png").expect("convert");
        assert_eq!(path, PathBuf::from("/mnt/c/Users/Alice/shot.png"));
    }
}

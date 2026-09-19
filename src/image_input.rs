//! Clipboard image input.
//!
//! A pasted screenshot arrives as raw pixels, not as a file, so it has to be encoded before
//! it can be sent or stored. PNG is used unconditionally: it is lossless, both provider APIs
//! accept it, and re-encoding a screenshot that was already PNG costs nothing in quality.
//!
//! Everything here is deliberately free of terminal concerns — it takes pixels in and gives
//! a [`crate::llm::Block`] back, so the encoding rules can be tested without a clipboard or
//! a TTY.

use base64::Engine;

use crate::llm::Block;

/// Guard against pasting something absurd. A 4K screenshot is a few megabytes of PNG; this
/// is far above that and still keeps a single request from ballooning.
const MAX_DIMENSION: u32 = 8_000;

/// Why a clipboard read produced nothing usable.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("剪贴板里没有图片")]
    NoImage,
    #[error("剪贴板里没有文本")]
    NoText,
    #[error("无法访问剪贴板：{0}")]
    Clipboard(String),
    #[error("图片尺寸 {}×{} 超出上限（每边最多 {MAX_DIMENSION}）", .0, .1)]
    TooLarge(u32, u32),
    #[error("图片编码失败：{0}")]
    Encode(String),
}

/// A clipboard image, already encoded and ready to send or store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedImage {
    pub width: u32,
    pub height: u32,
    /// Base64 of the PNG bytes.
    pub data: String,
    /// Encoded size, for the note shown to the user.
    pub bytes: usize,
}

impl PastedImage {
    /// The block that goes into the conversation.
    pub fn block(&self) -> Block {
        Block::Image { media_type: "image/png".to_string(), data: self.data.clone() }
    }

    /// One line for the transcript, e.g. `[图片 1280×720, 84 KB]`.
    pub fn label(&self) -> String {
        format!("[图片 {}×{}, {}]", self.width, self.height, human_size(self.bytes))
    }
}

fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// Encode raw RGBA pixels as a PNG and base64 them.
///
/// Split out from the clipboard read so the encoding is testable without a display server.
pub fn encode_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<PastedImage, ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::NoImage);
    }
    let (w, h) = (width as u32, height as u32);
    if w > MAX_DIMENSION || h > MAX_DIMENSION {
        return Err(ImageError::TooLarge(w, h));
    }
    // `arboard` hands back RGBA8; transparency in a screenshot is real, so keep it.
    let expected = width * height * 4;
    if rgba.len() < expected {
        return Err(ImageError::Encode(format!(
            "像素数据不完整：期望 {expected} 字节，实际 {}",
            rgba.len()
        )));
    }
    // Through `image`'s PNG encoder rather than a raw `png` handle: it is the crate this
    // module declares, and its writer owns the Vec for us.
    let buffer = image::RgbaImage::from_raw(w, h, rgba[..expected].to_vec())
        .ok_or_else(|| ImageError::Encode("像素缓冲与尺寸不匹配".into()))?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|err| ImageError::Encode(err.to_string()))?;
    let bytes = png.len();
    Ok(PastedImage {
        width: w,
        height: h,
        data: base64::engine::general_purpose::STANDARD.encode(&png),
        bytes,
    })
}

/// Read the clipboard's image, if it has one.
pub fn read_clipboard_image() -> Result<PastedImage, ImageError> {
    let mut clipboard = arboard::Clipboard::new().map_err(|err| ImageError::Clipboard(err.to_string()))?;
    match clipboard.get_image() {
        Ok(image) => encode_rgba(image.width, image.height, &image.bytes),
        Err(_) => Err(ImageError::NoImage),
    }
}

/// Read the clipboard's text, for the paste-a-path case.
pub fn read_clipboard_text() -> Result<String, ImageError> {
    let mut clipboard = arboard::Clipboard::new().map_err(|err| ImageError::Clipboard(err.to_string()))?;
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(text),
        _ => Err(ImageError::NoText),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pastable_image_becomes_a_png_block() {
        // Two by two, all red, opaque.
        let rgba = [255u8, 0, 0, 255].repeat(4);
        let image = encode_rgba(2, 2, &rgba).unwrap();
        assert_eq!(image.width, 2);
        assert_eq!(image.height, 2);
        assert!(matches!(image.block(), Block::Image { .. }));

        // The payload is real PNG: magic bytes decoded from the base64.
        let raw = base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap();
        assert_eq!(&raw[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(image.bytes, raw.len());
    }

    #[test]
    fn the_label_names_the_size_and_the_pixel_dimensions() {
        let image = encode_rgba(1280, 720, &[0u8; 1280 * 720 * 4]).unwrap();
        let label = image.label();
        assert!(label.contains("1280×720"), "{label}");
        // The encoded length is reported, not the raw pixel buffer.
        assert!(label.contains(|c: char| c.is_ascii_digit()), "{label}");
    }

    #[test]
    fn an_oversized_image_is_refused_by_name() {
        // The check happens before any encoding, so this does not allocate the pixels.
        let err = encode_rgba(MAX_DIMENSION as usize + 1, 10, &[]).unwrap_err();
        assert!(matches!(err, ImageError::TooLarge(..)), "{err:?}");
    }

    #[test]
    fn an_empty_image_is_not_an_image() {
        assert!(matches!(encode_rgba(0, 0, &[]), Err(ImageError::NoImage)));
    }

    #[test]
    fn truncated_pixels_are_reported_rather_than_panicking() {
        // A short buffer must be an error, not an out-of-bounds slice.
        let err = encode_rgba(4, 4, &[0u8; 10]).unwrap_err();
        assert!(matches!(err, ImageError::Encode(_)), "{err:?}");
    }

    #[test]
    fn human_sizes_read_the_way_a_person_would_write_them() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}

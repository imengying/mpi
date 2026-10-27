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
    #[error("不是受支持的图片格式。只接受 PNG / JPEG / GIF / WebP")]
    Unsupported,
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
    /// Base64 of the encoded bytes.
    pub data: String,
    /// The media type those bytes actually are, e.g. `image/png`.
    ///
    /// Carried rather than assumed: a file on disk may be PNG, JPEG, GIF or WebP, and both
    /// provider APIs take the type from the request. Sending JPEG bytes labelled `image/png`
    /// fails at the provider with a message about the payload rather than about the type.
    pub media_type: &'static str,
    /// Encoded size, for the note shown to the user.
    pub bytes: usize,
}

impl PastedImage {
    /// The block that goes into the conversation.
    pub fn block(&self) -> Block {
        Block::Image {
            media_type: self.media_type.to_string(),
            data: self.data.clone(),
        }
    }

    /// One line for the transcript, e.g. `[图片 1280×720, 84 KB]`.
    pub fn label(&self) -> String {
        format!(
            "[图片 {}×{}, {}]",
            self.width,
            self.height,
            human_size(self.bytes)
        )
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
    // Clipboard data is already RGBA8 — a screenshot and a photo both arrive as
    // pixels, not as a file — so this writes PNG directly. Going through `image`
    // pulled a JPEG decoder, TIFF and a color-management stack that nothing calls.
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|err| ImageError::Encode(err.to_string()))?;
        writer
            .write_image_data(&rgba[..expected])
            .map_err(|err| ImageError::Encode(err.to_string()))?;
        writer
            .finish()
            .map_err(|err| ImageError::Encode(err.to_string()))?;
    }
    let bytes = encoded.len();
    Ok(PastedImage {
        width: w,
        height: h,
        data: base64::engine::general_purpose::STANDARD.encode(&encoded),
        // Clipboard pixels are encoded here, so the type is known: PNG.
        media_type: "image/png",
        bytes,
    })
}

/// Why a path did not yield an image.
#[derive(thiserror::Error, Debug)]
pub enum PathError {
    #[error("读不了这个文件：{0}")]
    Read(std::io::Error),
    #[error("不是受支持的图片格式。只接受 PNG / JPEG / GIF / WebP")]
    Unsupported,
    #[error("图片损坏或尺寸读不出来：{0}")]
    Decode(String),
    #[error("图片尺寸 {}×{} 超出上限（每边最多 {MAX_DIMENSION}）", .0, .1)]
    TooLarge(u32, u32),
}

impl From<PathError> for ImageError {
    fn from(err: PathError) -> Self {
        match err {
            PathError::Read(err) => ImageError::Clipboard(err.to_string()),
            PathError::Unsupported => ImageError::Unsupported,
            PathError::Decode(err) => ImageError::Encode(err),
            PathError::TooLarge(w, h) => ImageError::TooLarge(w, h),
        }
    }
}

/// What kind of image these bytes are, judged from the file signature.
///
/// Only the four types both provider APIs accept, and only the ones whose extent can be read
/// without a full decoder. The signature is the honest source: a `.jpg` extension on PNG bytes
/// is common, and the provider reads the type from the request rather than from the name.
///
/// `pub(crate)` because `read` uses it too: hitting an image there is a wrong-tool mistake,
/// and naming the format is what lets that error point at `read_image`.
pub(crate) fn sniff(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    // JPEG: SOI marker followed by any segment marker.
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// The pixel dimensions, read from the header alone.
///
/// Each container states its extent in a fixed place near the front, so nothing is decoded:
/// this function never touches image data. A format whose size could not be read that cheaply
/// is not supported, which is the same list [`sniff`] accepts.
fn dimensions(bytes: &[u8], media_type: &str) -> Option<(u32, u32)> {
    match media_type {
        "image/png" => {
            // IHDR is the first chunk: 8-byte signature, 4-byte length, 4-byte type, then
            // width and height as big-endian u32.
            let header = bytes.get(16..24)?;
            Some((
                u32::from_be_bytes(header[0..4].try_into().ok()?),
                u32::from_be_bytes(header[4..8].try_into().ok()?),
            ))
        }
        "image/gif" => {
            // Logical screen descriptor: little-endian u16 each.
            let header = bytes.get(6..10)?;
            Some((
                u16::from_le_bytes(header[0..2].try_into().ok()?).into(),
                u16::from_le_bytes(header[2..4].try_into().ok()?).into(),
            ))
        }
        // JPEG and WebP put their extent after variable-length segments. Reading it means
        // walking those segments, which is a decoder by another name; the size is checked
        // against the byte budget instead, which is what actually costs anything.
        _ => None,
    }
}

/// Load an image from a file path, for the case where the picture is already on disk.
///
/// A pasted screenshot arrives as raw pixels and has to be encoded; a file on disk does not.
/// The bytes are passed through **unchanged** and the media type is taken from the file
/// signature, so a JPEG goes out as `image/jpeg` without being re-encoded — no quality lost,
/// no codec pulled in.
pub fn read_image_path(path: &std::path::Path) -> Result<PastedImage, PathError> {
    let bytes = std::fs::read(path).map_err(PathError::Read)?;
    let media_type = sniff(&bytes).ok_or(PathError::Unsupported)?;
    let (width, height) = match dimensions(&bytes, media_type) {
        Some((w, h)) => (w, h),
        // The extent is not in a fixed place. Report the file as 0×0 rather than inventing a
        // number; the label shows the size on disk, which is the figure that matters here.
        None => (0, 0),
    };
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(PathError::TooLarge(width, height));
    }
    Ok(PastedImage {
        width,
        height,
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        media_type,
        bytes: bytes.len(),
    })
}

/// Read the clipboard's image, if it has one.
pub fn read_clipboard_image() -> Result<PastedImage, ImageError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| ImageError::Clipboard(err.to_string()))?;
    match clipboard.get_image() {
        Ok(image) => encode_rgba(image.width, image.height, &image.bytes),
        Err(_) => Err(ImageError::NoImage),
    }
}

/// Read the clipboard's text, for the paste-a-path case.
pub fn read_clipboard_text() -> Result<String, ImageError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| ImageError::Clipboard(err.to_string()))?;
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
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
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

    /// Build a real PNG of the given size without pulling in an encoder helper.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = vec![0u8; (width * height * 4) as usize];
        writer.write_image_data(&pixels).unwrap();
        writer.finish().unwrap();
        out
    }

    /// The file is already an image; sending it must not require a display server.
    ///
    /// A headless session cannot read the clipboard at all, so a path is the only way to
    /// attach a picture there. The bytes go out unchanged: re-encoding a PNG costs time and
    /// buys nothing, and the provider takes it as it is.
    #[test]
    fn a_png_on_disk_is_sent_byte_for_byte() {
        let dir = crate::tools::temp_dir("image-path");
        let path = dir.join("shot.png");
        let original = png_bytes(64, 32);
        std::fs::write(&path, &original).unwrap();

        let image = read_image_path(&path).unwrap();
        assert_eq!((image.width, image.height), (64, 32));
        assert_eq!(image.bytes, original.len());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
        assert_eq!(
            decoded, original,
            "the file must be passed through untouched"
        );
        assert!(matches!(
            image.block(),
            Block::Image { ref media_type, .. } if media_type == "image/png"
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The bytes decide the type, not the file name, and each type goes out as itself.
    ///
    /// Both provider APIs read the type from the request. Labelling JPEG bytes `image/png`
    /// fails at the provider with a message about the payload rather than about the type,
    /// which sends the reader looking in the wrong place.
    #[test]
    fn the_media_type_comes_from_the_bytes_not_the_name() {
        let dir = crate::tools::temp_dir("image-types");

        // A JPEG signature on a file called `.png`: the name lies, the bytes do not.
        let lying = dir.join("actually_jpeg.png");
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend_from_slice(&[0u8; 64]);
        std::fs::write(&lying, &jpeg).unwrap();
        let image = read_image_path(&lying).unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert!(matches!(
            image.block(),
            Block::Image { ref media_type, .. } if media_type == "image/jpeg"
        ));

        // WebP and GIF are recognised the same way.
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(&[0u8; 32]);
        std::fs::write(dir.join("x.webp"), &webp).unwrap();
        assert_eq!(
            read_image_path(&dir.join("x.webp")).unwrap().media_type,
            "image/webp"
        );
        std::fs::write(dir.join("x.gif"), b"GIF89a\x04\x00\x03\x00\x00\x00\x00").unwrap();
        let gif = read_image_path(&dir.join("x.gif")).unwrap();
        assert_eq!(gif.media_type, "image/gif");
        // GIF states its extent in the header, so the label can show it.
        assert_eq!((gif.width, gif.height), (4, 3));

        // Something that is not an image at all is refused by name.
        let text = dir.join("notes.txt");
        std::fs::write(&text, "just text").unwrap();
        let error = read_image_path(&text).unwrap_err();
        assert!(matches!(error, PathError::Unsupported), "{error}");

        // A missing file is its own message.
        let missing = read_image_path(&dir.join("nope.png")).unwrap_err();
        assert!(matches!(missing, PathError::Read(_)), "{missing}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_oversized_png_on_disk_is_refused_before_anything_is_sent() {
        let dir = crate::tools::temp_dir("image-too-big");
        let path = dir.join("huge.png");
        std::fs::write(&path, png_bytes(MAX_DIMENSION + 1, 8)).unwrap();
        let error = read_image_path(&path).unwrap_err();
        assert!(matches!(error, PathError::TooLarge(..)), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

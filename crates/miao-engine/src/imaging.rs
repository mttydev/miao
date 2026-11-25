use base64::Engine;
use std::io::Cursor;

/// Longest side an inline image may keep on the wire. Anthropic caps a
/// many-image request at 3000 pixels per side and drops to 2000 once a request
/// carries enough images; its own guidance is 1568. The TypeScript runner
/// already normalized attachments to 2000, so the Rust engine matches it.
const MAX_SIDE: u32 = 2_000;
/// The decoded size encoding aims for, matching the TypeScript runner's
/// `targetRawBytes` (Claude Code's `imageMaxRawBytes`).
const TARGET_RAW_BYTES: usize = 512_000;
/// The hard ceiling. An image that cannot be brought under it has no safe wire
/// form, so the caller decides what note replaces it.
const MAX_RAW_BYTES: usize = 3_750_000;
/// A small file can still declare an enormous canvas, and decoding allocates
/// the whole frame. Anything past this is replaced with a note instead.
const MAX_PIXELS: u64 = 40_000_000;
const JPEG_QUALITIES: [u8; 4] = [80, 60, 40, 20];

pub(crate) fn supports(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Rewrites an inline image that would exceed a provider limit, and returns
/// `None` when the bytes already fit. Only the outgoing copy changes: the
/// durable attachment bytes stay exactly as the operator's client sent them.
pub(crate) fn normalize(mime: &str, data: &str) -> Result<Option<(String, String)>, String> {
    if !supports(mime) {
        return Ok(None);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "invalid image base64".to_string())?;
    let (width, height) = dimensions(&bytes)?;
    if width <= MAX_SIDE && height <= MAX_SIDE && bytes.len() <= TARGET_RAW_BYTES {
        return Ok(None);
    }
    if width as u64 * height as u64 > MAX_PIXELS {
        return Err("image is too large to decode".into());
    }
    let image = image::load_from_memory(&bytes).map_err(|_| "undecodable image".to_string())?;
    let (mime, bytes) = encode(&fit(image))?;
    Ok(Some((
        mime,
        base64::engine::general_purpose::STANDARD.encode(bytes),
    )))
}

/// Reads only the header, so an image that already fits costs no decode.
fn dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "unreadable image".to_string())?
        .into_dimensions()
        .map_err(|_| "unreadable image dimensions".to_string())
}

fn fit(image: image::DynamicImage) -> image::DynamicImage {
    let scale = f64::min(
        1.0,
        f64::min(
            MAX_SIDE as f64 / image.width() as f64,
            MAX_SIDE as f64 / image.height() as f64,
        ),
    );
    if scale >= 1.0 {
        return image;
    }
    let width = ((image.width() as f64 * scale).round() as u32).max(1);
    let height = ((image.height() as f64 * scale).round() as u32).max(1);
    image.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
}

/// Encodes toward the target size, trying the format that keeps the most
/// quality first: a photo becomes JPEG before it becomes a multi-megabyte PNG,
/// and an image with transparency tries PNG before dropping the alpha channel.
fn encode(image: &image::DynamicImage) -> Result<(String, Vec<u8>), String> {
    let mut smallest: Option<(String, Vec<u8>)> = None;
    let alpha = image.color().has_alpha();
    let mut keep = |mime: &str, bytes: Vec<u8>| -> Option<(String, Vec<u8>)> {
        let candidate = (mime.to_string(), bytes);
        if candidate.1.len() <= TARGET_RAW_BYTES {
            return Some(candidate);
        }
        if smallest
            .as_ref()
            .is_none_or(|smallest| candidate.1.len() < smallest.1.len())
        {
            smallest = Some(candidate);
        }
        None
    };
    let png_first = alpha.then(|| png(image)).transpose()?;
    if let Some(done) = png_first.and_then(|bytes| keep("image/png", bytes)) {
        return Ok(done);
    }
    for quality in JPEG_QUALITIES {
        if let Some(done) = keep("image/jpeg", jpeg(image, quality)?) {
            return Ok(done);
        }
    }
    let png_last = (!alpha).then(|| png(image)).transpose()?;
    if let Some(done) = png_last.and_then(|bytes| keep("image/png", bytes)) {
        return Ok(done);
    }
    match smallest {
        Some(candidate) if candidate.1.len() <= MAX_RAW_BYTES => Ok(candidate),
        _ => Err("image exceeds the wire size budget".into()),
    }
}

fn png(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|_| "image could not be encoded".to_string())?;
    Ok(bytes)
}

fn jpeg(image: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut Cursor::new(&mut bytes), quality)
        .encode_image(&image::DynamicImage::ImageRgb8(image.to_rgb8()))
        .map_err(|_| "image could not be encoded".to_string())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};

    fn encoded(width: u32, height: u32, noisy: bool) -> String {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = if noisy {
                ((x * 7 + y * 13) % 251) as u8
            } else {
                128
            };
            *pixel = Rgb([value, value.wrapping_add(3), value.wrapping_add(5)]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode fixture");
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn decoded(data: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("base64")
    }

    #[test]
    fn an_image_beyond_the_side_limit_is_shrunk_to_fit() {
        let (mime, data) = normalize("image/png", &encoded(4_000, 240, true))
            .unwrap()
            .expect("oversized image is rewritten");
        let bytes = decoded(&data);
        let (width, height) = dimensions(&bytes).unwrap();
        assert!(width <= MAX_SIDE && height <= MAX_SIDE, "{width}x{height}");
        assert!(bytes.len() <= TARGET_RAW_BYTES, "{}", bytes.len());
        assert!(supports(&mime), "{mime}");
    }

    #[test]
    fn an_image_within_the_limits_is_left_alone() {
        assert!(normalize("image/png", &encoded(320, 200, false))
            .unwrap()
            .is_none());
    }

    #[test]
    fn an_undecodable_image_reports_an_error_instead_of_rewriting() {
        assert!(normalize("image/png", "aGk=").is_err());
    }

    #[test]
    fn an_unsupported_media_type_is_left_alone() {
        assert!(normalize("application/pdf", "aGk=").unwrap().is_none());
        assert!(normalize("image/tiff", "aGk=").unwrap().is_none());
    }
}

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::ImageFormat;
use std::io::Cursor;
use tracing::{info, warn};

use crate::provider_spec::ProviderKind;

pub(crate) const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const ANTHROPIC_MAX_LONG_EDGE: u32 = 1568;
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 2000;
const SHRINK_FACTOR: f64 = 0.75;
const JPEG_QUALITY: u8 = 85;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedImage {
    pub data: String,
    pub mime_type: String,
}

#[cfg(test)]
pub(crate) fn base64_decoded_size(b64: &str) -> usize {
    let len = b64.len();
    if len == 0 {
        return 0;
    }
    let padding = b64
        .as_bytes()
        .iter()
        .rev()
        .take(2)
        .filter(|&&byte| byte == b'=')
        .count();
    (len / 4) * 3 - padding
}

pub(crate) fn prepare_image_for_provider(
    kind: ProviderKind,
    data: &str,
    mime_type: &str,
) -> Option<PreparedImage> {
    match kind {
        ProviderKind::Anthropic => prepare_anthropic_image(data, mime_type),
        ProviderKind::Gemini
        | ProviderKind::OpenAi
        | ProviderKind::OpenAiCompatible
        | ProviderKind::Ollama => Some(PreparedImage {
            data: data.to_owned(),
            mime_type: mime_type.to_owned(),
        }),
    }
}

pub(crate) fn prepare_anthropic_image(data: &str, mime_type: &str) -> Option<PreparedImage> {
    let b64_len = data.len();
    let over_size = b64_len > MAX_IMAGE_BYTES;
    let over_dimension = !over_size && exceeds_dimension_limit(data);

    if over_size || over_dimension {
        let reason = if over_size {
            "exceeds 5 MB API limit"
        } else {
            "exceeds 2000px dimension limit for many-image requests"
        };
        info!(
            provider = "anthropic",
            original_size = b64_len,
            mime_type = %mime_type,
            reason,
            "image needs downscaling"
        );

        if let Some((new_data, new_mime)) = downscale_image(data, mime_type) {
            info!(
                provider = "anthropic",
                original_size = b64_len,
                resized_size = new_data.len(),
                mime_type = %new_mime,
                reason,
                "image downscaled successfully"
            );
            Some(PreparedImage {
                data: new_data,
                mime_type: new_mime,
            })
        } else {
            warn!(
                provider = "anthropic",
                original_size = b64_len,
                mime_type = %mime_type,
                reason,
                "failed to downscale image, skipping"
            );
            None
        }
    } else {
        Some(PreparedImage {
            data: data.to_owned(),
            mime_type: mime_type.to_owned(),
        })
    }
}

pub(crate) fn downscale_image(b64_data: &str, mime_type: &str) -> Option<(String, String)> {
    let raw = BASE64.decode(b64_data).ok()?;
    let img = image::load_from_memory(&raw).ok()?;

    let use_png = mime_type == "image/png";
    let output_format = if use_png {
        ImageFormat::Png
    } else {
        ImageFormat::Jpeg
    };
    let out_mime = if use_png { "image/png" } else { "image/jpeg" };

    let (width, height) = (img.width(), img.height());
    let long_edge = width.max(height);
    let mut img = if long_edge > ANTHROPIC_MAX_LONG_EDGE {
        let scale = f64::from(ANTHROPIC_MAX_LONG_EDGE) / f64::from(long_edge);
        let new_width = scale_dim(width, scale);
        let new_height = scale_dim(height, scale);
        img.resize_exact(new_width, new_height, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    loop {
        let encoded = encode_image(&img, output_format)?;
        let new_b64 = BASE64.encode(&encoded);
        if new_b64.len() <= MAX_IMAGE_BYTES {
            return Some((new_b64, out_mime.to_owned()));
        }

        let (width, height) = (img.width(), img.height());
        let new_width = scale_dim(width, SHRINK_FACTOR);
        let new_height = scale_dim(height, SHRINK_FACTOR);
        if new_width == 0 || new_height == 0 {
            return None;
        }
        img = img.resize_exact(new_width, new_height, image::imageops::FilterType::Lanczos3);
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scale_dim(dim: u32, factor: f64) -> u32 {
    (f64::from(dim) * factor).round() as u32
}

fn encode_image(img: &image::DynamicImage, format: ImageFormat) -> Option<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    match format {
        ImageFormat::Jpeg => {
            let encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
            img.write_with_encoder(encoder).ok()?;
        }
        _ => img.write_to(&mut buf, format).ok()?,
    }
    Some(buf.into_inner())
}

pub(crate) fn exceeds_dimension_limit(b64_data: &str) -> bool {
    let Ok(raw) = BASE64.decode(b64_data) else {
        return false;
    };
    let cursor = Cursor::new(raw);
    let Ok(reader) = image::ImageReader::new(cursor).with_guessed_format() else {
        return false;
    };
    match reader.into_dimensions() {
        Ok((width, height)) => width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION,
        Err(_) => false,
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn solid_png(width: u32, height: u32) -> String {
        let img = image::DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            width,
            height,
            Rgba([255, 0, 0, 255]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        BASE64.encode(bytes.into_inner())
    }

    #[test]
    fn anthropic_downscales_oversized_dimensions() {
        let b64 = solid_png(2500, 1200);
        let prepared = prepare_anthropic_image(&b64, "image/png").unwrap();
        let raw = BASE64.decode(&prepared.data).unwrap();
        let img = image::load_from_memory(&raw).unwrap();
        assert!(img.width().max(img.height()) <= ANTHROPIC_MAX_LONG_EDGE);
    }

    #[test]
    fn non_anthropic_images_pass_through() {
        let b64 = solid_png(2500, 1200);
        let prepared = prepare_image_for_provider(ProviderKind::OpenAi, &b64, "image/png").unwrap();
        assert_eq!(prepared.data, b64);
        assert_eq!(prepared.mime_type, "image/png");
    }

    #[test]
    fn invalid_image_fails_gracefully() {
        assert!(downscale_image("not-base64", "image/png").is_none());
        assert!(!exceeds_dimension_limit("not-base64"));
    }
}

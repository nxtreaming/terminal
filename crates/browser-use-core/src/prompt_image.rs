use std::path::Path;

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{ColorType, DynamicImage, GenericImageView, ImageEncoder, ImageFormat};

pub(crate) const MAX_PROMPT_IMAGE_DIMENSION: u32 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptImageMode {
    ResizeToFit,
    Original,
}

#[derive(Debug, Clone)]
pub(crate) struct EncodedPromptImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: &'static str,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl EncodedPromptImage {
    pub(crate) fn into_data_url(self) -> String {
        format!(
            "data:{};base64,{}",
            self.mime,
            general_purpose::STANDARD.encode(self.bytes)
        )
    }
}

pub(crate) fn load_for_prompt_bytes(
    path: &Path,
    file_bytes: Vec<u8>,
    mode: PromptImageMode,
) -> Result<EncodedPromptImage> {
    let format = match image::guess_format(&file_bytes) {
        Ok(ImageFormat::Png) => Some(ImageFormat::Png),
        Ok(ImageFormat::Jpeg) => Some(ImageFormat::Jpeg),
        Ok(ImageFormat::Gif) => Some(ImageFormat::Gif),
        Ok(ImageFormat::WebP) => Some(ImageFormat::WebP),
        _ => None,
    };
    let dynamic = image::load_from_memory(&file_bytes)
        .with_context(|| format!("unsupported or invalid image bytes at {}", path.display()))?;
    let (width, height) = dynamic.dimensions();

    if mode == PromptImageMode::Original
        || (width <= MAX_PROMPT_IMAGE_DIMENSION && height <= MAX_PROMPT_IMAGE_DIMENSION)
    {
        if let Some(format) = format.filter(|format| can_preserve_source_bytes(*format)) {
            return Ok(EncodedPromptImage {
                bytes: file_bytes,
                mime: format_to_mime(format),
                width,
                height,
            });
        }
        let (bytes, output_format) = encode_image(&dynamic, ImageFormat::Png)?;
        return Ok(EncodedPromptImage {
            bytes,
            mime: format_to_mime(output_format),
            width,
            height,
        });
    }

    let resized = dynamic.resize(
        MAX_PROMPT_IMAGE_DIMENSION,
        MAX_PROMPT_IMAGE_DIMENSION,
        FilterType::Triangle,
    );
    let target_format = format
        .filter(|format| can_preserve_source_bytes(*format))
        .unwrap_or(ImageFormat::Png);
    let (bytes, output_format) = encode_image(&resized, target_format)?;
    Ok(EncodedPromptImage {
        bytes,
        mime: format_to_mime(output_format),
        width: resized.width(),
        height: resized.height(),
    })
}

fn can_preserve_source_bytes(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    )
}

fn encode_image(
    image: &DynamicImage,
    preferred_format: ImageFormat,
) -> Result<(Vec<u8>, ImageFormat)> {
    let target_format = match preferred_format {
        ImageFormat::Jpeg => ImageFormat::Jpeg,
        ImageFormat::WebP => ImageFormat::WebP,
        _ => ImageFormat::Png,
    };
    let mut buffer = Vec::new();
    match target_format {
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            let encoder = PngEncoder::new(&mut buffer);
            encoder.write_image(
                rgba.as_raw(),
                image.width(),
                image.height(),
                ColorType::Rgba8.into(),
            )?;
        }
        ImageFormat::Jpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut buffer, 85);
            encoder.encode_image(image)?;
        }
        ImageFormat::WebP => {
            let rgba = image.to_rgba8();
            let encoder = WebPEncoder::new_lossless(&mut buffer);
            encoder.write_image(
                rgba.as_raw(),
                image.width(),
                image.height(),
                ColorType::Rgba8.into(),
            )?;
        }
        _ => unreachable!("target format is normalized above"),
    }
    Ok((buffer, target_format))
}

fn format_to_mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use image::{ImageBuffer, Rgba};

    fn image_bytes(image: &ImageBuffer<Rgba<u8>, Vec<u8>>, format: ImageFormat) -> Vec<u8> {
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut encoded, format)
            .expect("encode image");
        encoded.into_inner()
    }

    #[test]
    fn resize_to_fit_preserves_small_png_bytes() {
        let image = ImageBuffer::from_pixel(64, 32, Rgba([10, 20, 30, 255]));
        let original = image_bytes(&image, ImageFormat::Png);

        let processed = load_for_prompt_bytes(
            Path::new("small.png"),
            original.clone(),
            PromptImageMode::ResizeToFit,
        )
        .expect("process");

        assert_eq!(processed.width, 64);
        assert_eq!(processed.height, 32);
        assert_eq!(processed.mime, "image/png");
        assert_eq!(processed.bytes, original);
    }

    #[test]
    fn resize_to_fit_downscales_to_square_bounds_like_codex() {
        let image = ImageBuffer::from_pixel(1024, 4096, Rgba([200, 10, 10, 255]));
        let original = image_bytes(&image, ImageFormat::Png);

        let processed = load_for_prompt_bytes(
            Path::new("tall.png"),
            original,
            PromptImageMode::ResizeToFit,
        )
        .expect("process");

        assert_eq!(processed.width, 512);
        assert_eq!(processed.height, MAX_PROMPT_IMAGE_DIMENSION);
        assert_eq!(processed.mime, "image/png");
    }

    #[test]
    fn original_mode_preserves_large_png_bytes() {
        let image = ImageBuffer::from_pixel(4096, 2048, Rgba([180, 30, 30, 255]));
        let original = image_bytes(&image, ImageFormat::Png);

        let processed = load_for_prompt_bytes(
            Path::new("large.png"),
            original.clone(),
            PromptImageMode::Original,
        )
        .expect("process");

        assert_eq!(processed.width, 4096);
        assert_eq!(processed.height, 2048);
        assert_eq!(processed.bytes, original);
    }

    #[test]
    fn original_mode_preserves_gif_bytes() {
        let image = ImageBuffer::from_pixel(32, 16, Rgba([80, 120, 200, 255]));
        let original = image_bytes(&image, ImageFormat::Gif);

        let processed = load_for_prompt_bytes(
            Path::new("animated.gif"),
            original.clone(),
            PromptImageMode::Original,
        )
        .expect("process");

        assert_eq!(processed.width, 32);
        assert_eq!(processed.height, 16);
        assert_eq!(processed.mime, "image/gif");
        assert_eq!(processed.bytes, original);
    }
}

use std::path::PathBuf;

use tempfile::Builder;

#[derive(Debug, Clone)]
pub(crate) enum PasteImageError {
    ClipboardUnavailable(String),
    NoImage(String),
    EncodeFailed(String),
    IoError(String),
}

impl std::fmt::Display for PasteImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasteImageError::ClipboardUnavailable(message) => {
                write!(f, "clipboard unavailable: {message}")
            }
            PasteImageError::NoImage(message) => write!(f, "no image on clipboard: {message}"),
            PasteImageError::EncodeFailed(message) => {
                write!(f, "could not encode image: {message}")
            }
            PasteImageError::IoError(message) => write!(f, "io error: {message}"),
        }
    }
}

impl std::error::Error for PasteImageError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PastedImageInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Debug)]
pub(crate) struct ClipboardRgbaImage {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum ValidatedPastedImage {
    LocalFile {
        path: PathBuf,
        info: PastedImageInfo,
    },
    Rgba(ClipboardRgbaImage),
}

impl ValidatedPastedImage {
    pub(crate) fn into_ready_path_or_rgba(
        self,
    ) -> std::result::Result<(PathBuf, PastedImageInfo), ClipboardRgbaImage> {
        match self {
            Self::LocalFile { path, info } => Ok((path, info)),
            Self::Rgba(image) => Err(image),
        }
    }
}

pub(crate) fn materialize_rgba_image_to_temp_png(
    image: ClipboardRgbaImage,
) -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    let width = image.width;
    let height = image.height;
    let Some(rgba) = image::RgbaImage::from_raw(width, height, image.bytes) else {
        return Err(PasteImageError::EncodeFailed(
            "invalid RGBA clipboard buffer".to_string(),
        ));
    };
    let dynamic = image::DynamicImage::ImageRgba8(rgba);
    let tmp = Builder::new()
        .prefix("but-clipboard-")
        .suffix(".png")
        .tempfile()
        .map_err(|error| PasteImageError::IoError(error.to_string()))?;
    let mut png = Vec::new();
    dynamic
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| PasteImageError::EncodeFailed(error.to_string()))?;
    std::fs::write(tmp.path(), png).map_err(|error| PasteImageError::IoError(error.to_string()))?;
    let (_file, path) = tmp
        .keep()
        .map_err(|error| PasteImageError::IoError(error.error.to_string()))?;
    Ok((path, PastedImageInfo { width, height }))
}

pub(crate) fn read_image_from_clipboard() -> Result<ValidatedPastedImage, PasteImageError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| PasteImageError::ClipboardUnavailable(error.to_string()))?;

    if let Some((path, info)) = clipboard
        .get()
        .file_list()
        .map_err(|error| PasteImageError::ClipboardUnavailable(error.to_string()))
        .ok()
        .and_then(|files| {
            files.into_iter().find_map(|path| {
                let (width, height) = image::image_dimensions(&path).ok()?;
                Some((path, PastedImageInfo { width, height }))
            })
        })
    {
        return Ok(ValidatedPastedImage::LocalFile { path, info });
    }

    let data = clipboard
        .get_image()
        .map_err(|error| PasteImageError::NoImage(error.to_string()))?;
    let width = data.width as u32;
    let height = data.height as u32;
    let bytes = data.bytes.into_owned();
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| PasteImageError::EncodeFailed("clipboard image is too large".to_string()))?;
    if bytes.len() != expected_len {
        return Err(PasteImageError::EncodeFailed(
            "invalid RGBA clipboard buffer".to_string(),
        ));
    }
    Ok(ValidatedPastedImage::Rgba(ClipboardRgbaImage {
        width,
        height,
        bytes,
    }))
}

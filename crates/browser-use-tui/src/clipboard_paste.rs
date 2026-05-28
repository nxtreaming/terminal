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

pub(crate) fn paste_image_to_temp_png() -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    let (png, info) = paste_image_as_png()?;
    let tmp = Builder::new()
        .prefix("but-clipboard-")
        .suffix(".png")
        .tempfile()
        .map_err(|error| PasteImageError::IoError(error.to_string()))?;
    std::fs::write(tmp.path(), png).map_err(|error| PasteImageError::IoError(error.to_string()))?;
    let (_file, path) = tmp
        .keep()
        .map_err(|error| PasteImageError::IoError(error.error.to_string()))?;
    Ok((path, info))
}

fn paste_image_as_png() -> Result<(Vec<u8>, PastedImageInfo), PasteImageError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| PasteImageError::ClipboardUnavailable(error.to_string()))?;

    let image = clipboard
        .get()
        .file_list()
        .map_err(|error| PasteImageError::ClipboardUnavailable(error.to_string()))
        .ok()
        .and_then(|files| files.into_iter().find_map(|path| image::open(path).ok()))
        .map(Ok)
        .unwrap_or_else(|| {
            let data = clipboard
                .get_image()
                .map_err(|error| PasteImageError::NoImage(error.to_string()))?;
            let width = data.width as u32;
            let height = data.height as u32;
            let Some(rgba) = image::RgbaImage::from_raw(width, height, data.bytes.into_owned())
            else {
                return Err(PasteImageError::EncodeFailed(
                    "invalid RGBA clipboard buffer".to_string(),
                ));
            };
            Ok(image::DynamicImage::ImageRgba8(rgba))
        })?;

    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| PasteImageError::EncodeFailed(error.to_string()))?;
    Ok((
        png,
        PastedImageInfo {
            width: image.width(),
            height: image.height(),
        },
    ))
}

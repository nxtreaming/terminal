//! Image context-budget helpers extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use base64::{engine::general_purpose, Engine as _};

use crate::constants::{
    ORIGINAL_IMAGE_MAX_PATCHES, ORIGINAL_IMAGE_PATCH_SIZE, RESIZED_IMAGE_CONTEXT_BYTES_ESTIMATE,
};
use crate::token_budget_to_char_budget;

pub(crate) fn image_url_context_budget_replacement(
    url: &str,
    detail: Option<&str>,
) -> Option<String> {
    parse_base64_image_data_url_payload(url)?;
    let estimated_bytes = if detail.is_some_and(|detail| detail.eq_ignore_ascii_case("original")) {
        estimate_original_image_context_bytes(url).unwrap_or(RESIZED_IMAGE_CONTEXT_BYTES_ESTIMATE)
    } else {
        RESIZED_IMAGE_CONTEXT_BYTES_ESTIMATE
    };
    Some(format!(
        "[image data omitted from text budget]{}",
        ".".repeat(estimated_bytes)
    ))
}

fn parse_base64_image_data_url_payload(url: &str) -> Option<&str> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }
    let comma_index = url.find(',')?;
    let metadata = &url[..comma_index];
    let payload = &url[comma_index + 1..];
    let metadata_without_scheme = &metadata["data:".len()..];
    let mut parts = metadata_without_scheme.split(';');
    let mime_type = parts.next().unwrap_or_default();
    let has_base64_marker = parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(.."image/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    {
        return None;
    }
    has_base64_marker.then_some(payload)
}

fn estimate_original_image_context_bytes(url: &str) -> Option<usize> {
    let payload = parse_base64_image_data_url_payload(url)?;
    let bytes = general_purpose::STANDARD.decode(payload).ok()?;
    let (width, height) = image_dimensions_from_bytes(&bytes)?;
    let patches_wide = usize::try_from(width)
        .ok()?
        .div_ceil(ORIGINAL_IMAGE_PATCH_SIZE);
    let patches_high = usize::try_from(height)
        .ok()?
        .div_ceil(ORIGINAL_IMAGE_PATCH_SIZE);
    let patch_count = patches_wide
        .saturating_mul(patches_high)
        .min(ORIGINAL_IMAGE_MAX_PATCHES);
    Some(token_budget_to_char_budget(patch_count))
}

fn image_dimensions_from_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions_from_bytes(bytes)
        .or_else(|| gif_dimensions_from_bytes(bytes))
        .or_else(|| jpeg_dimensions_from_bytes(bytes))
}

fn png_dimensions_from_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn gif_dimensions_from_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || !matches!(&bytes[..6], b"GIF87a" | b"GIF89a") {
        return None;
    }
    let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
    let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
    (width > 0 && height > 0).then_some((width, height))
}

fn jpeg_dimensions_from_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 4 <= bytes.len() {
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        if offset >= bytes.len() {
            return None;
        }
        let marker = bytes[offset];
        offset += 1;
        if matches!(marker, 0xd8 | 0xd9 | 0x01) || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if offset + 2 > bytes.len() {
            return None;
        }
        let segment_len = u16::from_be_bytes(bytes[offset..offset + 2].try_into().ok()?) as usize;
        if segment_len < 2 || offset + segment_len > bytes.len() {
            return None;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if segment_len < 7 {
                return None;
            }
            let height = u16::from_be_bytes(bytes[offset + 3..offset + 5].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[offset + 5..offset + 7].try_into().ok()?) as u32;
            return (width > 0 && height > 0).then_some((width, height));
        }
        offset += segment_len;
    }
    None
}

//! Base64 data-url image parsing + token estimation.
//!
//! Pure and deterministic. Mirrors legacy `browser-use-core/src/images.rs`:
//!   * `estimate_image_context_bytes(url)` = original-detail patch estimate when
//!     the URL carries a `detail=original` / `"detail":"original"` marker (and
//!     dimensions parse), else `RESIZED_IMAGE_BYTES_ESTIMATE` (7373).
//!   * original-detail patches = `ceil(w/32) * ceil(h/32)`, capped at 10_000,
//!     converted back to a byte budget via the 4-bytes/token ratio (`* 4`).
//!   * dimensions parsed from PNG IHDR / JPEG SOF headers of the base64-decoded
//!     payload.
//!
//! The base64 decoder is implemented inline (no external crate) so this module
//! stays dependency-light and fully deterministic.
//!
//! `ImageEstimateCache` is a small SHA1-keyed LRU so repeated estimation of the
//! same (large) data-url is O(1). Fully deterministic: identical inputs always
//! produce identical keys and values.

use std::collections::VecDeque;

use super::constants::{
    ORIGINAL_IMAGE_MAX_PATCHES, ORIGINAL_IMAGE_PATCH_SIZE, RESIZED_IMAGE_BYTES_ESTIMATE,
};

/// Parsed base64 data-url image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedImage {
    pub mime: String,
    pub data: Vec<u8>,
}

/// Parse a `data:<mime>;base64,<payload>` URL into mime + decoded bytes.
///
/// Returns `None` for non-data URLs, non-base64 data URLs, or malformed
/// payloads.
pub fn parse_base64_image_data_url(url: &str) -> Option<ParsedImage> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    // meta is e.g. "image/png;base64" (mime may carry params before the marker).
    let mime = meta.strip_suffix(";base64")?.to_string();
    let data = base64_decode(payload)?;
    Some(ParsedImage { mime, data })
}

/// Estimate model-visible bytes for an inline image data URL.
///
/// Ground: legacy `images.rs::estimate_image_context_bytes`.
pub fn estimate_image_context_bytes(url: &str) -> i64 {
    if image_uses_original_detail(url) {
        estimate_original_image_context_bytes(url).unwrap_or(RESIZED_IMAGE_BYTES_ESTIMATE)
    } else {
        RESIZED_IMAGE_BYTES_ESTIMATE
    }
}

/// True when the data URL is flagged as original-detail.
///
/// Ground: legacy `images.rs::image_uses_original_detail`.
fn image_uses_original_detail(url: &str) -> bool {
    url.contains("detail=original") || url.contains("\"detail\":\"original\"")
}

/// Original-detail byte estimate: `ceil(w/32) * ceil(h/32)` patches (capped at
/// 10_000), converted to a byte budget via `* 4`.
///
/// Ground: legacy `images.rs::estimate_original_image_context_bytes`.
fn estimate_original_image_context_bytes(url: &str) -> Option<i64> {
    let (width, height) = decode_image_dimensions(url)?;
    let patches_x = (width as usize).div_ceil(ORIGINAL_IMAGE_PATCH_SIZE);
    let patches_y = (height as usize).div_ceil(ORIGINAL_IMAGE_PATCH_SIZE);
    let patches = patches_x
        .saturating_mul(patches_y)
        .min(ORIGINAL_IMAGE_MAX_PATCHES);
    Some(patches.saturating_mul(4) as i64)
}

/// Decode width/height from the base64 payload of a data URL (PNG/JPEG).
fn decode_image_dimensions(url: &str) -> Option<(u32, u32)> {
    let payload = url.split_once("base64,").map(|(_, b)| b)?;
    let bytes = base64_decode(payload)?;
    parse_png_dimensions(&bytes).or_else(|| parse_jpeg_dimensions(&bytes))
}

fn parse_png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG signature (8) + IHDR length (4) + "IHDR" (4) then width/height u32 BE.
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

fn parse_jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // SOF markers carry dimensions (excluding DHT/DAC/DRI variants).
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let height = u32::from_be_bytes([0, 0, bytes[i + 5], bytes[i + 6]]);
            let width = u32::from_be_bytes([0, 0, bytes[i + 7], bytes[i + 8]]);
            return Some((width, height));
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Inline base64 decoder (standard alphabet, RFC 4648). Deterministic, no deps.
// ---------------------------------------------------------------------------

fn base64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode a standard-alphabet base64 string. Returns `None` on malformed input
/// (bad length, illegal characters, or data after padding).
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut data_len = bytes.len();
    let mut padding = 0usize;
    while data_len > 0 && bytes[data_len - 1] == b'=' {
        padding += 1;
        data_len -= 1;
    }
    if padding > 2 || bytes.len() % 4 != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut acc_bits = 0u32;
    for &c in &bytes[..data_len] {
        let v = base64_val(c)? as u32;
        acc = (acc << 6) | v;
        acc_bits += 6;
        if acc_bits >= 8 {
            acc_bits -= 8;
            out.push((acc >> acc_bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Deterministic SHA1-keyed LRU for image estimates.
// ---------------------------------------------------------------------------

/// Small deterministic SHA1-keyed LRU for image estimates.
///
/// Keys are SHA1 digests of the data-url so large payloads aren't retained.
/// Capacity-bounded with classic LRU eviction. Pure: no clocks, no randomness.
#[derive(Debug)]
pub struct ImageEstimateCache {
    capacity: usize,
    /// (key, value); front = least-recently-used, back = most-recently-used.
    entries: VecDeque<([u8; 20], i64)>,
}

impl ImageEstimateCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
        }
    }

    /// Look up a cached estimate for `data_url`, promoting it to MRU on hit.
    pub fn get(&mut self, data_url: &str) -> Option<i64> {
        let key = sha1(data_url.as_bytes());
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            let entry = self.entries.remove(pos).expect("position is valid");
            let value = entry.1;
            self.entries.push_back(entry);
            Some(value)
        } else {
            None
        }
    }

    /// Insert (or update) an estimate, evicting the LRU entry if at capacity.
    pub fn put(&mut self, data_url: &str, value: i64) {
        let key = sha1(data_url.as_bytes());
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }

    /// Compute-or-insert: returns the cached value, else runs `f`, caches and
    /// returns it.
    pub fn get_or_insert_with<F: FnOnce() -> i64>(&mut self, data_url: &str, f: F) -> i64 {
        if let Some(v) = self.get(data_url) {
            return v;
        }
        let v = f();
        self.put(data_url, v);
        v
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Minimal dependency-free SHA1 (RFC 3174). Used only as a deterministic LRU
/// cache key; not security-sensitive.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    let ml = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let j = i * 4;
            *word = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

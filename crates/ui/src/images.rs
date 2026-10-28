//! Images in rendered documents (notebook outputs, Markdown, HTML):
//! decoding (base64 as nbformat writes it), sizes from their headers,
//! elements that keep their aspect ratio, and the bounded caches and
//! content hashes that keep rendering from redoing the work every frame.

use std::{collections::HashMap, sync::Arc};

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use gpui::{AnyElement, Image, ImageFormat, Img, div, img, prelude::*, px};

/// A bounded map: past `capacity` entries, inserting drops the least
/// recently used one.
pub(crate) struct Lru<V> {
    map: HashMap<u64, (V, u64)>,
    tick: u64,
    capacity: usize,
}

impl<V: Clone> Lru<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            capacity,
        }
    }

    pub(crate) fn get_or_insert_with(&mut self, key: u64, make: impl FnOnce() -> V) -> V {
        self.tick += 1;
        if let Some((value, used)) = self.map.get_mut(&key) {
            *used = self.tick;
            return value.clone();
        }
        if self.map.len() >= self.capacity
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key)
        {
            self.map.remove(&oldest);
        }
        let value = make();
        self.map.insert(key, (value.clone(), self.tick));
        value
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// A fast non-cryptographic hash of `parts` (FxHash's mix, eight bytes at
/// a time): cache keys for outputs hashed every frame.
pub(crate) fn content_hash<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mix = |hash: u64, word: u64| (hash.rotate_left(5) ^ word).wrapping_mul(K);
    let mut hash = 0;
    for part in parts {
        let (chunks, rest) = part.as_chunks::<8>();
        for chunk in chunks {
            hash = mix(hash, u64::from_le_bytes(*chunk));
        }
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        hash = mix(mix(hash, u64::from_le_bytes(last)), part.len() as u64);
    }
    hash
}

/// An image ready to draw, with its size in pixels when its header says.
#[derive(Clone)]
pub(crate) struct DecodedImage {
    pub image: Arc<Image>,
    pub size: Option<(f32, f32)>,
}

/// Base64 as nbformat writes it: with line breaks, padding optional.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub(crate) fn decode_base64(data: &str) -> Option<Vec<u8>> {
    let compact: Vec<u8> = data
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    BASE64.decode(compact).ok()
}

pub(crate) fn decode_image(format: ImageFormat, data: &str, base64: bool) -> Option<DecodedImage> {
    let bytes = if base64 {
        decode_base64(data)?
    } else {
        data.as_bytes().to_vec()
    };
    Some(decoded(format, bytes))
}

pub(crate) fn decoded(format: ImageFormat, bytes: Vec<u8>) -> DecodedImage {
    let size = image_size(format, &bytes);
    DecodedImage {
        image: Arc::new(Image::from_bytes(format, bytes)),
        size,
    }
}

/// Width and height in pixels from the image's header (PNG, JPEG, GIF,
/// SVG `width`/`height` or `viewBox`).
pub(crate) fn image_size(format: ImageFormat, bytes: &[u8]) -> Option<(f32, f32)> {
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    match format {
        ImageFormat::Png if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {
            Some((be32(16)? as f32, be32(20)? as f32))
        }
        ImageFormat::Gif if bytes.starts_with(b"GIF") && bytes.len() >= 10 => Some((
            u16::from_le_bytes([bytes[6], bytes[7]]) as f32,
            u16::from_le_bytes([bytes[8], bytes[9]]) as f32,
        )),
        ImageFormat::Jpeg => jpeg_size(bytes),
        ImageFormat::Svg => svg_size(std::str::from_utf8(bytes).ok()?),
        _ => None,
    }
    .filter(|&(width, height)| width > 0. && height > 0.)
}

/// From the first start-of-frame segment.
fn jpeg_size(bytes: &[u8]) -> Option<(f32, f32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let be16 = |at: usize| Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
    let mut at = 2;
    while at + 1 < bytes.len() {
        if bytes[at] != 0xff {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        match marker {
            0xff => at += 1,
            // Markers without a length.
            0x01 | 0xd0..=0xd8 => at += 2,
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Some((be16(at + 7)? as f32, be16(at + 5)? as f32));
            }
            _ => at += 2 + be16(at + 2)? as usize,
        }
    }
    None
}

/// The root element's `width`/`height` (px or pt), else its `viewBox`.
fn svg_size(svg: &str) -> Option<(f32, f32)> {
    let start = svg.find("<svg")?;
    let tag = &svg[start..start + svg[start..].find('>')?];
    let attribute = |name: &str| {
        tag.match_indices(name).find_map(|(at, _)| {
            if !tag[..at].chars().next_back()?.is_whitespace() {
                return None;
            }
            let rest = tag[at + name.len()..].trim_start().strip_prefix('=')?;
            let rest = rest.trim_start();
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let value = &rest[1..];
            Some(&value[..value.find(quote)?])
        })
    };
    let length = |value: &str| {
        let value = value.trim();
        let (number, scale) = if let Some(number) = value.strip_suffix("pt") {
            (number, 4. / 3.)
        } else {
            (value.strip_suffix("px").unwrap_or(value), 1.)
        };
        number.trim().parse::<f32>().ok().map(|n| n * scale)
    };
    if let (Some(width), Some(height)) = (
        attribute("width").and_then(length),
        attribute("height").and_then(length),
    ) {
        return Some((width, height));
    }
    let view_box: Vec<f32> = attribute("viewBox")?
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    match view_box[..] {
        [_, _, width, height] => Some((width, height)),
        _ => None,
    }
}

/// An image as wide as it is (`size` in pixels) or as its container
/// allows, keeping its aspect ratio.
pub(crate) fn image_element(image: &DecodedImage, size: Option<(f32, f32)>) -> AnyElement {
    sized(img(image.image.clone()), size.or(image.size))
}

/// `image` `size` pixels wide (or as its container allows) and high,
/// keeping its aspect ratio; as large as it is without `size`.
pub(crate) fn sized(image: Img, size: Option<(f32, f32)>) -> AnyElement {
    match size {
        Some((width, height)) if width > 0. && height > 0. => {
            let mut frame = div().w_full().max_w(px(width));
            frame.style().aspect_ratio = Some(width / height);
            frame.child(image.size_full()).into_any_element()
        }
        _ => image.max_w_full().into_any_element(),
    }
}

/// The size to draw an image of `natural` size at: `width` and `height`
/// when both are given, one of them and the other from its aspect ratio,
/// else none (its own).
pub(crate) fn fit(
    width: Option<f32>,
    height: Option<f32>,
    natural: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    match (width, height, natural) {
        (Some(width), Some(height), _) => Some((width, height)),
        (Some(width), None, Some((w, h))) => Some((width, width * h / w)),
        (None, Some(height), Some((w, h))) => Some((height * w / h, height)),
        _ => None,
    }
}

/// The image format of a MIME type (`image/png`).
pub(crate) fn mime_format(mime: &str) -> Option<ImageFormat> {
    Some(match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/svg+xml" => ImageFormat::Svg,
        "image/webp" => ImageFormat::Webp,
        "image/bmp" => ImageFormat::Bmp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_sized_from_their_header() {
        let svg = r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" stroke-width="2" width="30pt" height="15pt" viewBox="0 0 40 20"></svg>"#;
        assert_eq!(
            image_size(ImageFormat::Svg, svg.as_bytes()),
            Some((40., 20.))
        );
        let view_box = r#"<svg viewBox="0 0 400 300"><g/></svg>"#;
        assert_eq!(
            image_size(ImageFormat::Svg, view_box.as_bytes()),
            Some((400., 300.))
        );
        // SOI, APP0 (length 4), SOF0 with height 7 and width 9.
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x07, 0x00, 0x09, 0x03,
        ];
        assert_eq!(image_size(ImageFormat::Jpeg, &jpeg), Some((9., 7.)));
    }

    #[test]
    fn bounded_caches_drop_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.get_or_insert_with(1, || "a");
        lru.get_or_insert_with(2, || "b");
        lru.get_or_insert_with(1, || unreachable!());
        lru.get_or_insert_with(3, || "c");
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get_or_insert_with(1, || "again"), "a");
        assert_eq!(lru.get_or_insert_with(2, || "new"), "new", "2 was dropped");
    }
}

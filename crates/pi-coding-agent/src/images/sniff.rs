//! Magic-byte image detection and header dimension parsing.
//!
//! Ground truth: TS `packages/coding-agent/src/utils/mime.ts:1-116`
//! (`detectSupportedImageMimeType`, `isPng`, `isAnimatedPng`, `isBmp`, and the
//! endian readers) plus `utils/clipboard-image.ts:26-43`
//! (`baseMimeType`, `extensionForImageMimeType`).
//!
//! The dimension readers are new: upstream gets pixel dimensions from Photon
//! after decoding (`image-resize-core.ts:84-90`), while this seam reads only
//! header fields so the resize policy can run without an image backend. See
//! `WIRING.md` for the trade-off.

/// A provider-supported inline image format (the return of TS
/// `detectSupportedImageMimeType`, `mime.ts:6-23`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Gif,
    Webp,
    Bmp,
}

impl ImageFormat {
    /// The wire MIME string upstream returns for this format.
    #[must_use]
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Bmp => "image/bmp",
        }
    }

    /// The conventional file extension (`extensionForImageMimeType`,
    /// `clipboard-image.ts:30-43`). Upstream has no extension for BMP because
    /// it converts BMP to PNG before naming a temp file; keep `bmp` here for
    /// completeness and map it to `png` at the temp-file call site.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Bmp => "bmp",
        }
    }
}

/// PNG's 8-byte signature (`mime.ts:4`).
const PNG_SIGNATURE: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// `detectSupportedImageMimeType` (`mime.ts:6-23`): sniff the leading bytes.
/// `None` for non-images, lossless JPEG, animated PNG, and malformed headers.
#[must_use]
pub fn detect_supported_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if starts_with(bytes, &[0xff, 0xd8, 0xff]) {
        // 0xff 0xd8 0xff 0xf7 is JPEG-LS / lossless JPEG, which upstream omits.
        return (bytes.get(3) != Some(&0xf7)).then_some(ImageFormat::Jpeg);
    }
    if starts_with(bytes, PNG_SIGNATURE) {
        return (is_png(bytes) && !is_animated_png(bytes)).then_some(ImageFormat::Png);
    }
    if starts_with_ascii(bytes, 0, b"GIF87a") || starts_with_ascii(bytes, 0, b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    if starts_with_ascii(bytes, 0, b"RIFF") && starts_with_ascii(bytes, 8, b"WEBP") {
        return Some(ImageFormat::Webp);
    }
    if starts_with_ascii(bytes, 0, b"BM") && is_bmp(bytes) {
        return Some(ImageFormat::Bmp);
    }
    None
}

/// `isPng` (`mime.ts:36-40`): signature plus an `IHDR` length of 13.
fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && read_u32_be(bytes, 8) == 13 && starts_with_ascii(bytes, 12, b"IHDR")
}

/// `isAnimatedPng` (`mime.ts:42-55`): an `acTL` chunk before the first `IDAT`.
fn is_animated_png(bytes: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= bytes.len() {
        let chunk_length = read_u32_be(bytes, offset) as usize;
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(bytes, chunk_type_offset, b"acTL") {
            return true;
        }
        if starts_with_ascii(bytes, chunk_type_offset, b"IDAT") {
            return false;
        }
        let next_offset = offset
            .saturating_add(8)
            .saturating_add(chunk_length)
            .saturating_add(4);
        if next_offset <= offset || next_offset > bytes.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

/// `isBmp` (`mime.ts:57-81`): header sanity plus color planes and bit depth.
fn is_bmp(bytes: &[u8]) -> bool {
    if bytes.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(bytes, 2);
    let pixel_data_offset = read_u32_le(bytes, 10);
    let dib_header_size = read_u32_le(bytes, 14);
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14_u32.saturating_add(dib_header_size) {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel);
    if dib_header_size == 12 {
        color_planes = read_u16_le(bytes, 22);
        bits_per_pixel = read_u16_le(bytes, 24);
    } else if (40..=124).contains(&dib_header_size) {
        if bytes.len() < 30 {
            return false;
        }
        color_planes = read_u16_le(bytes, 26);
        bits_per_pixel = read_u16_le(bytes, 28);
    } else {
        return false;
    }
    color_planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

/// Natural pixel dimensions read straight from header fields, `(width, height)`.
/// `None` when the header is truncated or implausible. These are the values
/// Photon would report after a full decode upstream.
#[must_use]
pub fn image_dimensions(bytes: &[u8], format: ImageFormat) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::Gif => gif_dimensions(bytes),
        ImageFormat::Webp => webp_dimensions(bytes),
        ImageFormat::Bmp => bmp_dimensions(bytes),
    }
}

/// `IHDR` stores width/height as big-endian `u32` at offsets 16 and 20.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 {
        return None;
    }
    Some((read_u32_be(bytes, 16), read_u32_be(bytes, 20)))
}

/// GIF's logical screen descriptor holds two little-endian `u16`s at 6 and 8.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 {
        return None;
    }
    Some((read_u16_le(bytes, 6), read_u16_le(bytes, 8)))
}

/// BMP's DIB header stores dimensions at offset 18 (and 22 for `u32` headers).
fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 26 {
        return None;
    }
    if read_u32_le(bytes, 14) == 12 {
        // BITMAPCOREHEADER: 16-bit unsigned width/height.
        return Some((read_u16_le(bytes, 18), read_u16_le(bytes, 20)));
    }
    if bytes.len() < 26 {
        return None;
    }
    let width = i32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
    let height = i32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);
    // A negative height means top-down rows; its magnitude is the real height.
    Some((u32::try_from(width).ok()?, height.unsigned_abs()))
}

/// WebP's first chunk is `VP8 ` (lossy), `VP8L` (lossless), or `VP8X` (extended).
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30
        || !starts_with_ascii(bytes, 0, b"RIFF")
        || !starts_with_ascii(bytes, 8, b"WEBP")
    {
        return None;
    }
    let chunks = &bytes[12..];
    let mut pos = 0usize;
    while pos + 8 <= chunks.len() {
        let id = &chunks[pos..pos + 4];
        let size = read_u32_le(chunks, pos + 4) as usize;
        let data_start = pos + 8;
        if size > chunks.len() - data_start {
            break;
        }
        let payload = &chunks[data_start..data_start + size];
        if id.starts_with(b"VP8 ") {
            if payload.len() < 10 || payload[3] != 0x9d || payload[4] != 0x01 || payload[5] != 0x2a
            {
                return None;
            }
            let width = u32::from(u16::from_le_bytes([payload[6], payload[7]])) & 0x3fff;
            let height = u32::from(u16::from_le_bytes([payload[8], payload[9]])) & 0x3fff;
            return Some((width, height));
        }
        if id.starts_with(b"VP8L") {
            if payload.len() < 5 || payload[0] != 0x2f {
                return None;
            }
            let width = 1 + u32::from(payload[1]) + ((u32::from(payload[2] & 0x3f)) << 8);
            let height = 1
                + ((u32::from(payload[2] & 0xc0)) >> 6)
                + (u32::from(payload[3]) << 2)
                + ((u32::from(payload[4] & 0x0f)) << 10);
            return Some((width, height));
        }
        if id.starts_with(b"VP8X") {
            if payload.len() < 10 {
                return None;
            }
            let width = 1 + u32::from_le_bytes([payload[4], payload[5], payload[6], 0]);
            let height = 1 + u32::from_le_bytes([payload[7], payload[8], payload[9], 0]);
            return Some((width, height));
        }
        pos = data_start + size + (size % 2); // WebP pads chunks to even bytes.
        if pos > chunks.len() {
            break;
        }
    }
    None
}

/// JPEG stores dimensions in the first SOF marker (`0xC0..=0xCF` except
/// `0xC4`, `0xC8`, `0xCC`, which are Huffman/JPG extensions).
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || !starts_with(bytes, &[0xff, 0xd8, 0xff]) {
        return None;
    }
    let mut pos = 2usize;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xff {
            pos += 1;
            continue;
        }
        let marker = bytes[pos + 1];
        if marker == 0xff {
            pos += 1;
            continue;
        }
        pos += 2;
        if marker == 0x00
            || marker == 0x01
            || marker == 0xd8
            || marker == 0xd9
            || (0xd0..=0xd7).contains(&marker)
        {
            continue; // markers without a length field
        }
        if pos + 2 > bytes.len() {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([bytes[pos], bytes[pos + 1]]));
        if length < 2 || pos + length > bytes.len() {
            return None;
        }
        let is_sof = matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
        if is_sof {
            if length < 8 {
                return None;
            }
            let payload = &bytes[pos + 2..pos + length];
            let height = u16::from_be_bytes([payload[1], payload[2]]);
            let width = u16::from_be_bytes([payload[3], payload[4]]);
            return Some((u32::from(width), u32::from(height)));
        }
        pos += length;
    }
    None
}

fn read_u16_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from(u16::from_le_bytes([
        *bytes.get(offset).unwrap_or(&0),
        *bytes.get(offset.saturating_add(1)).unwrap_or(&0),
    ]))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        *bytes.get(offset).unwrap_or(&0),
        *bytes.get(offset.saturating_add(1)).unwrap_or(&0),
        *bytes.get(offset.saturating_add(2)).unwrap_or(&0),
        *bytes.get(offset.saturating_add(3)).unwrap_or(&0),
    ])
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        *bytes.get(offset).unwrap_or(&0),
        *bytes.get(offset.saturating_add(1)).unwrap_or(&0),
        *bytes.get(offset.saturating_add(2)).unwrap_or(&0),
        *bytes.get(offset.saturating_add(3)).unwrap_or(&0),
    ])
}

fn starts_with(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()] == *prefix
}

fn starts_with_ascii(bytes: &[u8], offset: usize, prefix: &[u8]) -> bool {
    bytes.len() >= offset.saturating_add(prefix.len())
        && bytes[offset..offset + prefix.len()] == *prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[0_u8; 5]); // bit depth, color type, compression, filter, interlace
        bytes.extend_from_slice(&[0_u8; 4]); // IHDR CRC (unused by the sniffer)
        bytes
    }

    fn bmp(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![b'B', b'M'];
        bytes.extend_from_slice(&30_u32.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; 4]);
        bytes.extend_from_slice(&26_u32.to_le_bytes());
        bytes.extend_from_slice(&12_u32.to_le_bytes()); // BITMAPCOREHEADER
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes()); // color planes
        bytes.extend_from_slice(&24_u16.to_le_bytes()); // bits per pixel
        bytes.extend_from_slice(&[0_u8; 4]);
        bytes
    }

    #[test]
    fn jpeg_magic_and_lossless_rejection() {
        assert_eq!(
            detect_supported_image_format(&[0xff, 0xd8, 0xff, 0xe0, 0x00]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            detect_supported_image_format(&[0xff, 0xd8, 0xff, 0xf7, 0x00]),
            None
        );
    }

    #[test]
    fn png_needs_a_valid_ihdr() {
        let image = png(12, 34);
        assert_eq!(
            detect_supported_image_format(&image),
            Some(ImageFormat::Png)
        );
        assert_eq!(image_dimensions(&image, ImageFormat::Png), Some((12, 34)));
        let mut broken = image;
        broken[12] = b'X'; // destroy the IHDR tag
        assert_eq!(detect_supported_image_format(&broken), None);
    }

    #[test]
    fn animated_png_is_rejected() {
        let mut image = png(1, 1);
        let mut actl = Vec::new();
        actl.extend_from_slice(&4_u32.to_be_bytes());
        actl.extend_from_slice(b"acTL");
        actl.extend_from_slice(&[0_u8; 8]); // data + CRC
        image.extend_from_slice(&actl);
        assert_eq!(detect_supported_image_format(&image), None);
    }

    #[test]
    fn gif_and_webp_are_detected() {
        let gif = b"GIF89a\x01\x00\x01\x00";
        assert_eq!(detect_supported_image_format(gif), Some(ImageFormat::Gif));
        assert_eq!(image_dimensions(gif, ImageFormat::Gif), Some((1, 1)));

        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&22_u32.to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(b"VP8X");
        webp.extend_from_slice(&10_u32.to_le_bytes());
        webp.extend_from_slice(&[0_u8; 4]); // flags + reserved
        webp.extend_from_slice(&39_u32.to_le_bytes()[..3]); // width - 1
        webp.extend_from_slice(&59_u32.to_le_bytes()[..3]); // height - 1
        assert_eq!(
            detect_supported_image_format(&webp),
            Some(ImageFormat::Webp)
        );
        assert_eq!(image_dimensions(&webp, ImageFormat::Webp), Some((40, 60)));
    }

    #[test]
    fn bmp_header_fields_and_dimensions() {
        let image = bmp(7, 9);
        assert_eq!(
            detect_supported_image_format(&image),
            Some(ImageFormat::Bmp)
        );
        assert_eq!(image_dimensions(&image, ImageFormat::Bmp), Some((7, 9)));
    }

    #[test]
    fn non_images_and_short_buffers_are_rejected() {
        assert_eq!(detect_supported_image_format(b"hello world"), None);
        assert_eq!(detect_supported_image_format(b""), None);
        assert_eq!(detect_supported_image_format(b"BM"), None);
        assert_eq!(image_dimensions(b"tiny", ImageFormat::Png), None);
    }

    #[test]
    fn jpeg_dimensions_from_sof_marker() {
        // SOI, APP0 with a length, then SOF0 carrying height=2 width=3.
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend_from_slice(&[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]);
        jpeg.extend_from_slice(&[
            0xff, 0xc0, 0x00, 0x08, 0x08, 0x00, 0x02, 0x00, 0x03, 0x01, 0x01, 0x11, 0x00,
        ]);
        assert_eq!(image_dimensions(&jpeg, ImageFormat::Jpeg), Some((3, 2)));
    }
}

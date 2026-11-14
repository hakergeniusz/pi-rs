//! `@file` CLI argument processing (port of `cli/file-processor.ts`).
//!
//! Upstream processes only the bare `@file` positional arguments the args
//! parser collected — prompt text is never scanned for inline `@path`
//! tokens. Text files inline as `<file name="{absolute}">\n{content}\n</file>`
//! blocks (one `\n` after each block; empty files are skipped, the BOM is
//! stripped, invalid UTF-8 is replaced exactly like Node's utf-8 decode).
//! Supported images (jpeg/png/gif/webp) attach as [`ImageContent`] with a
//! `<file name="{absolute}"></file>` reference block. There is no size cap:
//! upstream reads whatever the user points at.
//!
//! Known divergence: upstream `processImage` converts bmp (and other
//! unsupported formats) to png via sharp, and can resize; the print path
//! passes `autoResizeImages: false`, so resizing is faithfully absent here,
//! but bmp conversion is not implemented — bmp yields upstream's
//! could-not-convert text (tracked for the P6 interactive image pipeline).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use pi_ai::types::ImageContent;

use crate::tools::path_utils::resolve_read_path;

/// `processFileArguments` (`file-processor.ts:25-88`): text blocks and image
/// attachments, in argument order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ImageContent>,
}

/// Upstream's could-not-convert message (`image-process.ts:82`).
const UNCONVERTIBLE_IMAGE: &str =
    "[Image omitted: could not be converted to a supported inline image format.]";

/// Processes `@file` arguments into text content and image attachments.
///
/// # Errors
/// A missing file errors with upstream's `Error: File not found: <path>`
/// (the caller exits 1); an unreadable file errors with upstream's
/// could-not-read wording.
pub fn process_file_arguments(file_args: &[String], cwd: &Path) -> Result<ProcessedFiles, String> {
    let mut processed = ProcessedFiles {
        text: String::new(),
        images: Vec::new(),
    };
    for file_arg in file_args {
        let absolute: PathBuf = resolve_read_path(file_arg, cwd);
        if !absolute.is_file() {
            return Err(format!("Error: File not found: {}", absolute.display()));
        }
        let metadata = std::fs::metadata(&absolute).map_err(|error| {
            format!("Error: Could not read file {}: {error}", absolute.display())
        })?;
        if metadata.len() == 0 {
            // Empty files are skipped (`file-processor.ts:44-47`).
            continue;
        }
        let bytes = std::fs::read(&absolute).map_err(|error| {
            format!("Error: Could not read file {}: {error}", absolute.display())
        })?;
        // The four provider-supported formats pass through
        // `normalizeImage` unchanged (`image-process.ts:49-53`), so with
        // `autoResizeImages: false` the attachment is the raw bytes.
        if let Some(mime_type) = detect_supported_image_mime(&bytes) {
            if mime_type == "image/bmp" {
                // Upstream converts bmp to png (`convertImageBytesToPng`);
                // the port has no image pipeline, so it takes upstream's
                // conversion-failure path.
                let _ = writeln!(
                    processed.text,
                    "<file name=\"{}\">{UNCONVERTIBLE_IMAGE}</file>",
                    absolute.display()
                );
                continue;
            }
            processed.images.push(ImageContent {
                data: BASE64_STANDARD.encode(&bytes),
                mime_type: mime_type.to_string(),
            });
            let _ = writeln!(
                processed.text,
                "<file name=\"{}\"></file>",
                absolute.display()
            );
        } else {
            let lossy = String::from_utf8_lossy(&bytes);
            let content = strip_bom(&lossy);
            let _ = writeln!(
                processed.text,
                "<file name=\"{}\">\n{content}\n</file>",
                absolute.display()
            );
        }
    }
    Ok(processed)
}

/// `stripBom` (`utils/text.ts`): drop a leading UTF-8 BOM.
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// `detectSupportedImageMimeType` (`utils/mime.ts:13-32`): magic-byte sniff,
/// `None` for anything that is not a supported (or convertible) image.
fn detect_supported_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if starts_with(bytes, &[0xff, 0xd8, 0xff]) {
        // 0xff 0xd8 0xff 0xf7 is a lossless JPEG, which upstream refuses.
        return if bytes.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if starts_with(bytes, &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return if is_png(bytes) && !is_animated_png(bytes) {
            Some("image/png")
        } else {
            None
        };
    }
    if starts_with_ascii(bytes, 0, b"GIF87a") || starts_with_ascii(bytes, 0, b"GIF89a") {
        return Some("image/gif");
    }
    if starts_with_ascii(bytes, 0, b"RIFF") && starts_with_ascii(bytes, 8, b"WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(bytes, 0, b"BM") && is_bmp(bytes) {
        return Some("image/bmp");
    }
    None
}

/// `isPng`: a PNG signature plus the IHDR chunk header.
fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && read_u32_be(bytes, 8) == 13 && starts_with_ascii(bytes, 12, b"IHDR")
}

/// `isAnimatedPng`: an `acTL` chunk before the first `IDAT`.
fn is_animated_png(bytes: &[u8]) -> bool {
    let mut offset = 8usize; // PNG_SIGNATURE.length
    while offset + 8 <= bytes.len() {
        let chunk_length = read_u32_be(bytes, offset) as usize;
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(bytes, chunk_type_offset, b"acTL") {
            return true;
        }
        if starts_with_ascii(bytes, chunk_type_offset, b"IDAT") {
            return false;
        }
        let next_offset = offset + 8 + chunk_length + 4;
        if next_offset <= offset || next_offset > bytes.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

/// `isBmp` (`mime.ts:57-81`): header sanity plus the DIB color planes and
/// bits-per-pixel fields.
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
    if pixel_data_offset < 14 + dib_header_size {
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

fn read_u16_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from(u16::from_le_bytes([
        *bytes.get(offset).unwrap_or(&0),
        *bytes.get(offset + 1).unwrap_or(&0),
    ]))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        *bytes.get(offset).unwrap_or(&0),
        *bytes.get(offset + 1).unwrap_or(&0),
        *bytes.get(offset + 2).unwrap_or(&0),
        *bytes.get(offset + 3).unwrap_or(&0),
    ])
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        *bytes.get(offset).unwrap_or(&0),
        *bytes.get(offset + 1).unwrap_or(&0),
        *bytes.get(offset + 2).unwrap_or(&0),
        *bytes.get(offset + 3).unwrap_or(&0),
    ])
}

fn starts_with(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()] == *prefix
}

fn starts_with_ascii(bytes: &[u8], offset: usize, prefix: &[u8]) -> bool {
    bytes.len() >= offset + prefix.len() && &bytes[offset..offset + prefix.len()] == prefix
}

#[cfg(test)]
mod tests {
    // `assert!(v.is_empty())` stays: the `assert_eq!(v, [])` alternative is
    // noisier for these fixture assertions.
    #![allow(clippy::assert_is_empty)]
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn text_files_inline_as_upstream_file_blocks() {
        let dir = tempdir();
        let path = write(&dir, "notes.txt", b"alpha");
        let processed = process_file_arguments(&["notes.txt".to_string()], dir.path()).expect("ok");
        assert_eq!(
            processed.text,
            format!("<file name=\"{}\">\nalpha\n</file>\n", path.display())
        );
        assert!(processed.images.is_empty());
    }

    #[test]
    fn empty_files_are_skipped() {
        let dir = tempdir();
        write(&dir, "empty.txt", b"");
        let processed = process_file_arguments(&["empty.txt".to_string()], dir.path()).expect("ok");
        assert_eq!(processed.text, "");
        assert!(processed.images.is_empty());
    }

    #[test]
    fn missing_file_args_error_like_upstream() {
        let dir = tempdir();
        let error = process_file_arguments(&["nope.txt".to_string()], dir.path()).unwrap_err();
        assert!(
            error.contains(&format!(
                "Error: File not found: {}",
                dir.path().join("nope.txt").display()
            )),
            "{error}"
        );
    }

    #[test]
    fn bom_is_stripped_and_invalid_utf8_replaced() {
        let dir = tempdir();
        write(&dir, "bom.txt", &[0xef, 0xbb, 0xbf, b'a', 0xff]);
        let processed = process_file_arguments(&["bom.txt".to_string()], dir.path()).expect("ok");
        assert!(
            processed.text.contains("\na\u{fffd}\n</file>"),
            "{processed:?}"
        );
    }

    #[test]
    fn png_args_attach_images_and_reference_block() {
        let dir = tempdir();
        // Minimal detectable png: signature + IHDR chunk header.
        let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0u8; 8]);
        let path = write(&dir, "image.png", &png);
        let processed = process_file_arguments(&["image.png".to_string()], dir.path()).expect("ok");
        assert_eq!(processed.images.len(), 1);
        assert_eq!(processed.images[0].mime_type, "image/png");
        assert_eq!(
            processed.text,
            format!("<file name=\"{}\"></file>\n", path.display())
        );
    }

    #[test]
    fn lossless_jpeg_is_treated_as_text() {
        let dir = tempdir();
        write(&dir, "lossless.jpg", &[0xff, 0xd8, 0xff, 0xf7, 0x00]);
        let processed =
            process_file_arguments(&["lossless.jpg".to_string()], dir.path()).expect("ok");
        assert!(processed.images.is_empty());
        assert!(processed.text.contains("</file>"));
    }

    #[test]
    fn bmp_files_report_the_upstream_omission_text() {
        let dir = tempdir();
        // Minimal valid BMP: BM, file size 30, pixel data at 26, DIB size 12,
        // one color plane, 24 bits per pixel.
        let mut bmp = vec![b'B', b'M'];
        bmp.extend_from_slice(&30u32.to_le_bytes()); // declared file size
        bmp.extend_from_slice(&[0u8; 4]); // reserved
        bmp.extend_from_slice(&26u32.to_le_bytes()); // pixel data offset
        bmp.extend_from_slice(&12u32.to_le_bytes()); // DIB header size
        bmp.extend_from_slice(&1u32.to_le_bytes()); // width
        bmp.extend_from_slice(&1u16.to_le_bytes()); // color planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        bmp.extend_from_slice(&[0u8; 4]);
        write(&dir, "image.bmp", &bmp);
        let processed = process_file_arguments(&["image.bmp".to_string()], dir.path()).expect("ok");
        assert!(processed.images.is_empty());
        assert!(
            processed.text.contains(UNCONVERTIBLE_IMAGE),
            "{processed:?}"
        );
    }

    #[test]
    fn multiple_files_concatenate_in_order() {
        let dir = tempdir();
        let a = write(&dir, "a.txt", b"A");
        let b = write(&dir, "b.txt", b"B");
        let processed =
            process_file_arguments(&["a.txt".to_string(), "b.txt".to_string()], dir.path())
                .expect("ok");
        assert_eq!(
            processed.text,
            format!(
                "<file name=\"{}\">\nA\n</file>\n<file name=\"{}\">\nB\n</file>\n",
                a.display(),
                b.display()
            )
        );
    }
}
